//! Where a cell's cached output is kept between runs, behind one trait.
//! Memory keeps nothing beyond the cell's results view (the default);
//! encrypted disk writes the output to the notebook security lane's
//! encrypted snapshot store, filed under the connection identity that
//! fetched it, so it can come back after a restart and is cleared when that
//! connection disconnects.

use sqlly_services::notebook::{CellOutput, ConnectionIdentity};

/// What a cache entry is for: the identity that fetched it and the cell
/// fingerprint it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheKey {
    pub identity: ConnectionIdentity,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StoreOptions {
    pub ttl_seconds: Option<u64>,
    pub pinned: bool,
}

/// A cached output read back, with when it was stored.
#[derive(Debug, Clone, PartialEq)]
pub struct CachedOutput {
    pub output: CellOutput,
    pub stored_at_ms: i64,
}

pub trait CacheBackend {
    fn store(
        &self,
        key: &CacheKey,
        output: &CellOutput,
        options: StoreOptions,
    ) -> Result<(), String>;
    fn load(&self, key: &CacheKey) -> Option<CachedOutput>;
    /// Forget everything fetched through `profile_id` (disconnect).
    fn clear_profile(&self, profile_id: &str) -> Result<usize, String>;
    fn clear_all(&self) -> Result<usize, String>;
}

/// Keeps nothing: the output lives in the cell's results view only.
pub struct MemoryCache;

impl CacheBackend for MemoryCache {
    fn store(&self, _: &CacheKey, _: &CellOutput, _: StoreOptions) -> Result<(), String> {
        Ok(())
    }

    fn load(&self, _: &CacheKey) -> Option<CachedOutput> {
        None
    }

    fn clear_profile(&self, _: &str) -> Result<usize, String> {
        Ok(0)
    }

    fn clear_all(&self) -> Result<usize, String> {
        Ok(0)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub use encrypted::EncryptedCache;

#[cfg(not(target_arch = "wasm32"))]
mod encrypted {
    use super::*;
    use sqlly_services::notebook_security::cache::{
        EntryKey, GetOutcome, PutOptions, SnapshotStoreConfig,
    };
    use sqlly_services::notebook_security::{CacheKeyProvider, SnapshotStore};

    /// The encrypted snapshot store.
    pub struct EncryptedCache {
        store: SnapshotStore,
    }

    impl EncryptedCache {
        pub fn open(
            config: SnapshotStoreConfig,
            keys: &dyn CacheKeyProvider,
        ) -> Result<Self, String> {
            SnapshotStore::open(config, keys)
                .map(|store| Self { store })
                .map_err(|e| crate::localization::message(e.message()))
        }

        /// The app's store under the notebooks folder, its key in the OS
        /// credential store.
        pub fn open_default() -> Result<Self, String> {
            Self::open(
                SnapshotStoreConfig::new(SnapshotStoreConfig::default_root()),
                &sqlly_services::notebook_security::KeyringCacheKey::default(),
            )
        }
    }

    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }

    impl CacheBackend for EncryptedCache {
        fn store(
            &self,
            key: &CacheKey,
            output: &CellOutput,
            options: StoreOptions,
        ) -> Result<(), String> {
            let scope = self.store.scope_for(&key.identity);
            self.store
                .put(
                    &scope,
                    &EntryKey::Cell {
                        fingerprint: key.fingerprint.clone(),
                    },
                    output,
                    PutOptions {
                        ttl: options.ttl_seconds.map(std::time::Duration::from_secs),
                        pinned: options.pinned,
                    },
                    now_ms(),
                )
                .map(|_| ())
                .map_err(|e| crate::localization::message(e.message()))
        }

        fn load(&self, key: &CacheKey) -> Option<CachedOutput> {
            let scope = self.store.scope_for(&key.identity);
            match self.store.get(
                &scope,
                &EntryKey::Cell {
                    fingerprint: key.fingerprint.clone(),
                },
                now_ms(),
            ) {
                Ok(GetOutcome::Hit(hit)) => Some(CachedOutput {
                    output: hit.output,
                    stored_at_ms: hit.stored_at_ms,
                }),
                _ => None,
            }
        }

        fn clear_profile(&self, profile_id: &str) -> Result<usize, String> {
            self.store
                .clear_profile(profile_id)
                .map_err(|e| crate::localization::message(e.message()))
        }

        fn clear_all(&self) -> Result<usize, String> {
            self.store
                .clear_all()
                .map_err(|e| crate::localization::message(e.message()))
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_services::notebook::{Completeness, Fingerprint, Freshness};
    use sqlly_services::notebook_security::MemoryCacheKey;
    use sqlly_services::notebook_security::cache::{MasterKey, SnapshotStoreConfig};
    use std::sync::Arc;

    fn output() -> CellOutput {
        CellOutput {
            columns: super::super::materialize::columns(&[("id", "int")]),
            rows: Arc::new(vec![vec![sqlly_core::CellValue::I64(7)]]),
            completeness: Completeness::Complete,
            row_count: 1,
            elapsed_ms: 3,
            run_id: "run_1".into(),
            fingerprint: Fingerprint("abc".into()),
            provenance: Default::default(),
            freshness: Freshness::Live,
        }
    }

    fn key(profile: &str) -> CacheKey {
        CacheKey {
            identity: ConnectionIdentity {
                profile_id: profile.into(),
                database: Some("Sales".into()),
                ..Default::default()
            },
            fingerprint: "abc".into(),
        }
    }

    #[test]
    fn memory_keeps_nothing() {
        assert!(
            MemoryCache
                .store(&key("p"), &output(), StoreOptions::default())
                .is_ok()
        );
        assert!(MemoryCache.load(&key("p")).is_none());
    }

    #[test]
    fn encrypted_disk_round_trips_per_identity_and_clears_on_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryCacheKey::with_key(MasterKey::from_bytes([7; 32]));
        let cache = EncryptedCache::open(SnapshotStoreConfig::new(dir.path()), &keys).unwrap();
        cache
            .store(&key("p1"), &output(), StoreOptions::default())
            .unwrap();
        let back = cache.load(&key("p1")).unwrap();
        assert_eq!(back.output.rows, output().rows);
        assert!(
            cache.load(&key("p2")).is_none(),
            "another sign-in sees nothing"
        );
        assert_eq!(cache.clear_profile("p1").unwrap(), 1);
        assert!(cache.load(&key("p1")).is_none());
    }
}
