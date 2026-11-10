//! On-disk persistence for the signed-in Entra accounts list, and the single
//! `EntraAccountRegistry` that the menu, dialog, and connection-profile picker
//! all read from. The token *material* itself lives in the keychain (see
//! [`crate::azure_keyring`]) — this file only persists the registry of
//! account identities (tenant, username, display name, last-login), which is
//! not secret and is safe to leave in plain JSON under
//! `~/Library/Application Support/SQLLY/entra-accounts.json` (macOS) or the
//! equivalent XDG / Roaming location on Linux/Windows.
//!
//! The pure snapshot/load functions live in `sqlly-security`'s
//! `EntraAccountRegistry` (`Serialize`/`Deserialize`). This module adds:
//!
//! - the platform-aware path resolution (`default_path` mirrors the Swift
//!   app's `ConnectionStoreData.defaultURL` exactly),
//! - a thread-safe `EntraAccountRegistryStore` wrapper the menu/dialog hold
//!   a clone of, backed by `parking_lot::RwLock`.

use std::path::{Path, PathBuf};

use sqlly_services::security::azure::{EntraAccountId, EntraAccountProfile, EntraAccountRegistry};

#[derive(Debug)]
pub enum RegistryError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (key, error) = match self {
            RegistryError::Io(error) => ("connections.azure.registry_io_error", error.to_string()),
            RegistryError::Json(error) => {
                ("connections.azure.registry_json_error", error.to_string())
            }
        };
        let message = crate::localization::format(key, &[("error", &error)]);
        write!(f, "{message}")
    }
}

impl std::error::Error for RegistryError {}

/// Computes the canonical on-disk path. Mirrors the Swift app's
/// `ConnectionStoreData.defaultURL()` family: per-user app-data directory
/// + `entra-accounts.json`. The directory itself is created on demand by
///   [`load_or_default`].
#[allow(dead_code)] // exposed for tests in `sqlly-services`; not used at runtime yet.
#[cfg(not(target_arch = "wasm32"))]
pub fn default_path() -> PathBuf {
    if let Some(dir) = dirs::config_dir() {
        dir.join("SQLLY").join("entra-accounts.json")
    } else {
        PathBuf::from("entra-accounts.json")
    }
}

/// Browser build: nominal path only — load/save use localStorage.
#[allow(dead_code)]
#[cfg(target_arch = "wasm32")]
pub fn default_path() -> PathBuf {
    PathBuf::from("entra-accounts.json")
}

/// Loads the registry from `path`. Missing or unreadable files default to an
/// empty registry (no account has signed in yet) so a fresh install does not
/// surface a confusing "load failed" error on first launch.
pub fn load_or_default(path: &Path) -> Result<EntraAccountRegistry, RegistryError> {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        match crate::host_storage::load_text(
            crate::host_storage::StorageArea::Config,
            "entra-accounts.json",
        ) {
            Some(text) => serde_json::from_str(&text).map_err(RegistryError::Json),
            None => Ok(EntraAccountRegistry::new()),
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EntraAccountRegistry::new());
            }
            Err(e) => return Err(RegistryError::Io(e)),
        };
        serde_json::from_slice(&bytes).map_err(RegistryError::Json)
    }
}

/// Atomic write: serialize to a sibling `.json.tmp` then rename so a crash
/// mid-write cannot leave a half-written JSON on disk.
/// Browser build: writes localStorage instead of the filesystem.
pub fn save(path: &Path, registry: &EntraAccountRegistry) -> Result<(), RegistryError> {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        let text = serde_json::to_string_pretty(registry).map_err(RegistryError::Json)?;
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Config,
            "entra-accounts.json",
            &text,
        )
        .map_err(|e| RegistryError::Io(std::io::Error::other(e)))
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(RegistryError::Io)?;
        }
        let bytes = serde_json::to_vec_pretty(registry).map_err(RegistryError::Json)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(RegistryError::Io)?;
        std::fs::rename(&tmp, path).map_err(RegistryError::Io)?;
        Ok(())
    }
}

/// Thread-safe in-memory cache of the registry, persisted to disk on every
/// mutation. The menu and dialog hold a clone of this; both can mutate it
/// concurrently without breaking each other's view because the `RwLock`
/// takes care of consistency.
#[derive(Clone, Debug)]
pub struct EntraAccountRegistryStore {
    path: PathBuf,
    inner: std::sync::Arc<parking_lot::RwLock<EntraAccountRegistry>>,
}

impl EntraAccountRegistryStore {
    pub fn from_path(path: impl Into<PathBuf>) -> Result<Self, RegistryError> {
        let path = path.into();
        let registry = load_or_default(&path)?;
        Ok(Self {
            path,
            inner: std::sync::Arc::new(parking_lot::RwLock::new(registry)),
        })
    }

    pub fn at_default() -> Result<Self, RegistryError> {
        Self::from_path(default_path())
    }

    pub fn list(&self) -> Vec<EntraAccountProfile> {
        self.inner.read().accounts().into_iter().cloned().collect()
    }

    pub fn upsert(&self, profile: EntraAccountProfile) -> EntraAccountId {
        let id = {
            let mut guard = self.inner.write();
            guard.upsert(profile)
        };
        let _ = save(&self.path, &self.inner.read());
        id
    }

    pub fn find(&self, id: &EntraAccountId) -> Option<EntraAccountProfile> {
        self.inner.read().get(id).cloned()
    }

    pub fn remove(&self, id: &EntraAccountId) -> Option<EntraAccountProfile> {
        let removed = self.inner.write().remove(id);
        let _ = save(&self.path, &self.inner.read());
        removed
    }

    /// Records a successful login (refresh-token acquisition) for an
    /// existing account. Used by the orchestrator after a fresh
    /// sign-in or refresh so the profile's `last_login_at` tracks reality.
    pub fn mark_login(
        &self,
        id: &EntraAccountId,
        now: chrono::DateTime<chrono::Utc>,
        access_token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
        refresh_token_expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<(), sqlly_core::StudioError> {
        {
            let mut guard = self.inner.write();
            sqlly_services::security::azure::EntraAccountRegistry::mark_login(
                &mut guard,
                id,
                now,
                access_token_expires_at,
                refresh_token_expires_at,
            )?;
        }
        let _ = save(&self.path, &self.inner.read());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn tmp_path(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sqlly-entra-test-{}-{}.json",
            label,
            uuid::Uuid::new_v4()
        ))
    }

    #[test]
    fn default_registry_is_empty_on_first_run() {
        let path = tmp_path("empty");
        let store = EntraAccountRegistryStore::from_path(&path).unwrap();
        assert!(
            store.list().is_empty(),
            "fresh install should see no accounts"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upsert_persists_then_reloads() {
        let path = tmp_path("upsert");
        let store = EntraAccountRegistryStore::from_path(&path).unwrap();
        let now = Utc::now();
        let profile = EntraAccountProfile::new(
            "alex@contoso.com",
            "tenant-a",
            "home-a",
            Some("Alex".into()),
            now,
        );
        let id = store.upsert(profile.clone());
        // Round-trip via a fresh store reading the same path.
        let reloaded = EntraAccountRegistryStore::from_path(&path).unwrap();
        let restored = reloaded
            .find(&id)
            .expect("upserted account should reload from disk");
        assert_eq!(restored.username, "alex@contoso.com");
        assert_eq!(restored.tenant_id, "tenant-a");
        assert_eq!(restored.display_name.as_deref(), Some("Alex"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_persists_deletion() {
        let path = tmp_path("remove");
        let store = EntraAccountRegistryStore::from_path(&path).unwrap();
        let now = Utc::now();
        let id = store.upsert(EntraAccountProfile::new(
            "bob@contoso.com",
            "tenant-b",
            "home-b",
            None,
            now,
        ));
        store.remove(&id);
        let reloaded = EntraAccountRegistryStore::from_path(&path).unwrap();
        assert!(reloaded.find(&id).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_treated_as_empty_registry() {
        let path = tmp_path("missing");
        // File intentionally absent.
        assert!(!path.exists());
        let registry = load_or_default(&path).unwrap();
        assert!(registry.accounts().is_empty());
    }

    #[test]
    fn corrupt_file_surfaces_an_error_instead_of_clobbering() {
        let path = tmp_path("corrupt");
        std::fs::write(&path, b"not valid json {").unwrap();
        let result = load_or_default(&path);
        assert!(matches!(result, Err(RegistryError::Json(_))));
        let _ = std::fs::remove_file(&path);
    }
}
