//! Browser (wasm) variant of [`crate::azure_keyring`]: a `TokenCacheStore`
//! backed by `sessionStorage` via [`crate::host_storage::secrets`] instead of
//! the OS keychain. Tokens survive reloads within the tab session but are
//! cleared when the tab closes — the desktop app is the one that uses the
//! operating system's secure credential storage.

use sqlly_services::security::azure::EntraTokenCacheRef;
use sqlly_services::security::token_cache::{TokenCacheError, TokenCacheStore};
use sqlly_services::security::token_flow::AcquiredToken;

use crate::host_storage::secrets;

/// sessionStorage-backed token cache store. Same public surface as the
/// native `KeyringTokenCacheStore`.
#[derive(Clone)]
pub struct KeyringTokenCacheStore {
    service: String,
}

impl KeyringTokenCacheStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn compose_key(&self, cache_ref: &EntraTokenCacheRef) -> String {
        format!("{}\u{0}{}", cache_ref.service, cache_ref.account)
    }

    /// Delete every token known to the account registry. All entries are
    /// attempted even when one fails so a partial storage outage leaves as
    /// little credential material behind as possible.
    pub async fn delete_all(
        &self,
        cache_refs: &[EntraTokenCacheRef],
    ) -> Result<(), TokenCacheError> {
        let mut failures = Vec::new();
        for cache_ref in cache_refs {
            if let Err(error) = self.delete(cache_ref).await {
                failures.push(format!("{}: {error}", cache_ref.account));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(TokenCacheError::Storage(failures.join("; ")))
        }
    }
}

#[async_trait::async_trait]
impl TokenCacheStore for KeyringTokenCacheStore {
    async fn load(
        &self,
        cache_ref: &EntraTokenCacheRef,
    ) -> Result<Option<AcquiredToken>, TokenCacheError> {
        let key = self.compose_key(cache_ref);
        match secrets::get(&self.service, &key) {
            Ok(Some(raw)) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| TokenCacheError::Corrupt(e.to_string())),
            Ok(None) => Ok(None),
            Err(e) => Err(TokenCacheError::Storage(e)),
        }
    }

    async fn store(
        &self,
        cache_ref: &EntraTokenCacheRef,
        token: &AcquiredToken,
    ) -> Result<(), TokenCacheError> {
        let key = self.compose_key(cache_ref);
        let raw =
            serde_json::to_string(token).map_err(|e| TokenCacheError::Storage(e.to_string()))?;
        secrets::set(&self.service, &key, &raw).map_err(TokenCacheError::Storage)
    }

    async fn delete(&self, cache_ref: &EntraTokenCacheRef) -> Result<(), TokenCacheError> {
        let key = self.compose_key(cache_ref);
        secrets::delete(&self.service, &key).map_err(TokenCacheError::Storage)
    }
}
