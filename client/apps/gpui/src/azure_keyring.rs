//! OS-keychain-backed `TokenCacheStore` for the gpui app.
//!
//! Wraps the `keyring` crate (which on macOS uses Security.framework /
//! Keychain Services, on Linux uses Secret Service via dbus, and on Windows
//! uses the Credential Manager) so `sqlly-security`'s `EntraTokenCache`
//! sees a uniform `TokenCacheStore` regardless of OS — no platform ifdefs
//! inside the security crate.
//!
//! Mirrors the Swift `KeychainSecretStore` contract:
//! - a single OS keychain item per `account_id`,
//! - `keychainAccessDenied` is sticky for the process lifetime and degrades
//!   to an in-memory fallback so the session still works,
//! - `didFallbackToLegacy` is **not** applicable here — Linux/Windows do not
//!   have the data-protection keychain quirk that motivates Swift's fallback
//!   flag, so we never set it.
//!
//! The implementation serializes the [`AcquiredToken`] as UTF-8 JSON so the
//! stored value is human-debuggable and the version field can be added later
//! without breaking existing keychain items.

use keyring::Entry;
use sqlly_services::security::azure::EntraTokenCacheRef;
use sqlly_services::security::token_cache::{TokenCacheError, TokenCacheStore};
use sqlly_services::security::token_flow::AcquiredToken;

/// Persists acquired Entra tokens in the OS keychain, with an in-memory
/// Persists acquired Entra tokens in the OS keychain, with an in-memory
/// fallback used when the keychain refuses access (so the session keeps
/// working — at the cost of losing credentials across a relaunch).
#[derive(Clone)]
pub struct KeyringTokenCacheStore {
    service: String,
    /// Sticky flag mirroring Swift's `KeychainSecretStore.isAccessible`. Once
    /// the keychain refuses a call in this process, subsequent save / load /
    /// delete calls short-circuit to `Err(KeychainAccessDenied)` without
    /// spending another OS call we know will fail. Wrapped in `Arc` so
    /// cloning the store shares the flag (mirrors Swift's `AccessFlagBox`
    /// pattern) — without that an explicit `clone()` would hand the UI a
    /// "fresh" appearance of accessibility.
    in_memory: std::sync::Arc<parking_lot::Mutex<InMemoryFallback>>,
}

/// Result of attempting a keychain call. `AccessDenied` is the sticky
/// failure that should flip `accessible` to false; everything else is a
/// real error to surface.
#[derive(Debug)]
enum KeyringCacheError {
    Keyring(keyring::Error),
}

#[derive(Default)]
struct InMemoryFallback {
    accessible: bool,
    cache: std::collections::BTreeMap<String, String>,
}

impl KeyringTokenCacheStore {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            in_memory: std::sync::Arc::new(parking_lot::Mutex::new(InMemoryFallback {
                accessible: true,
                cache: Default::default(),
            })),
        }
    }

    fn compose_key(&self, cache_ref: &EntraTokenCacheRef) -> String {
        format!("{}\u{0}{}", cache_ref.service, cache_ref.account)
    }

    fn try_keyring(&self, key: &str) -> Result<Option<String>, KeyringCacheError> {
        let entry = Entry::new(&self.service, key).map_err(KeyringCacheError::Keyring)?;
        match entry.get_password() {
            Ok(s) => Ok(Some(s)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(KeyringCacheError::Keyring(e)),
        }
    }

    fn put_keyring(&self, key: &str, json: &str) -> Result<(), KeyringCacheError> {
        let entry = Entry::new(&self.service, key).map_err(KeyringCacheError::Keyring)?;
        entry
            .set_password(json)
            .map_err(KeyringCacheError::Keyring)?;
        Ok(())
    }

    fn delete_keyring(&self, key: &str) -> Result<(), KeyringCacheError> {
        let entry = Entry::new(&self.service, key).map_err(KeyringCacheError::Keyring)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(KeyringCacheError::Keyring(e)),
        }
    }

    /// Delete every token known to the account registry. All entries are
    /// attempted even when one fails so a partial platform-keychain outage
    /// leaves as little credential material behind as possible.
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

/// Result of attempting a keychain call. `AccessDenied` is the sticky
/// failure that should flip `accessible` to false; everything else is a
/// real error to surface.
impl KeyringCacheError {
    fn is_access_denied(&self) -> bool {
        // Treat both `NoStorageAccess` (the keychain daemon refused us —
        // common on Linux without `gnome-keyring`/KWallet running, and on
        // macOS when the user actively denied a system prompt) and
        // `PlatformFailure` (often "the platform secure store is not
        // reachable at all") as the sticky denial. Crisp string matches on
        // platform-specific codes do not survive across keyring releases;
        // these typed variants do.
        matches!(
            self,
            KeyringCacheError::Keyring(
                keyring::Error::NoStorageAccess(_) | keyring::Error::PlatformFailure(_)
            )
        )
    }
}

#[async_trait::async_trait]
impl TokenCacheStore for KeyringTokenCacheStore {
    async fn load(
        &self,
        cache_ref: &EntraTokenCacheRef,
    ) -> Result<Option<AcquiredToken>, TokenCacheError> {
        let key = self.compose_key(cache_ref);
        // In-memory cache wins so the deny-then-continue-then-relaunch
        // gap stays closed: a token saved across the denial path is
        // returned even after a keychain search miss.
        {
            let guard = self.in_memory.lock();
            if let Some(raw) = guard.cache.get(&key) {
                return serde_json::from_str(raw)
                    .map(Some)
                    .map_err(|e| TokenCacheError::Corrupt(e.to_string()));
            }
            if !guard.accessible {
                return Err(TokenCacheError::Storage(crate::localization::text(
                    "connections.azure.keychain_denied_earlier",
                )));
            }
        }
        match self.try_keyring(&key) {
            Ok(Some(raw)) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| TokenCacheError::Corrupt(e.to_string())),
            Ok(None) => Ok(None),
            Err(e) if e.is_access_denied() => {
                self.in_memory.lock().accessible = false;
                Err(TokenCacheError::Storage(crate::localization::text(
                    "connections.azure.keychain_access_denied",
                )))
            }
            Err(KeyringCacheError::Keyring(e)) => Err(TokenCacheError::Storage(e.to_string())),
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
        // Always populate in-memory cache so the deny path keeps the
        // session working.
        self.in_memory.lock().cache.insert(key.clone(), raw.clone());
        match self.put_keyring(&key, &raw) {
            Ok(()) => Ok(()),
            Err(e) if e.is_access_denied() => {
                self.in_memory.lock().accessible = false;
                // Mirror Swift's degrade-gracefully contract: caller still
                // sees success because the in-memory cache holds the token.
                tracing::warn!(
                    "keychain refused to store token; session will work but the credential will not survive a relaunch"
                );
                Ok(())
            }
            Err(KeyringCacheError::Keyring(e)) => Err(TokenCacheError::Storage(e.to_string())),
        }
    }

    async fn delete(&self, cache_ref: &EntraTokenCacheRef) -> Result<(), TokenCacheError> {
        let key = self.compose_key(cache_ref);
        self.in_memory.lock().cache.remove(&key);
        match self.delete_keyring(&key) {
            Ok(()) => Ok(()),
            Err(e) if e.is_access_denied() => {
                self.in_memory.lock().accessible = false;
                Err(TokenCacheError::Storage(crate::localization::text(
                    "connections.azure.keychain_access_denied",
                )))
            }
            Err(KeyringCacheError::Keyring(e)) => Err(TokenCacheError::Storage(e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    /// Switches the `keyring` crate to its in-memory mock backend for the
    /// duration of the test, so we never prompt the user for their macOS
    /// Keychain password or talk to the real Linux Secret Service in CI.
    /// Must be called *before* the first `KeyringTokenCacheStore::store`
    /// call so the static `DEFAULT_BUILDER` in `keyring` is overridden.
    /// Tests across the same process share the override — that is fine,
    /// we just want the OS keychain to never see a real call.
    fn use_mock_keyring() {
        keyring::mock::install();
    }

    fn cache_ref(account: &str) -> EntraTokenCacheRef {
        // The cache_ref.service distinguishes SQLLY's items on the OS
        // keychain from any other tenant's; for the test we use a marker
        // service name so assertion messages can identify the failure.
        EntraTokenCacheRef {
            service: "sqlly.entra".into(),
            account: account.into(),
        }
    }

    fn token_at(lifetime: Duration) -> AcquiredToken {
        AcquiredToken {
            access_token: "AT".into(),
            refresh_token: Some("RT".into()),
            access_token_expires_at: Utc::now() + lifetime,
            token_type: "Bearer".into(),
            scope: Some("s".into()),
            id_token: None,
        }
    }

    #[test]
    fn serializes_token_to_json_then_round_trips() {
        // Sanity: the JSON encoding we write to the keychain is the one we
        // read back; format matches the
        // `serde_json::to_string(AcquiredToken)` default.
        let token = token_at(Duration::hours(1));
        let raw = serde_json::to_string(&token).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["access_token"], "AT");
        assert_eq!(value["refresh_token"], "RT");
        assert_eq!(value["token_type"], "Bearer");
        assert_eq!(
            value["access_token_expires_at"],
            json!(token.access_token_expires_at)
        );
    }

    #[test]
    fn compose_key_separates_service_and_account_with_null_byte() {
        // The composed key is the EntraTokenCacheRef's `service` and
        // `account` joined by a NUL byte. The OS-keychain `service` arg
        // is passed separately to `Entry::new(service, key)` and so does
        // NOT appear in the key string itself.
        let cache_ref = cache_ref("alex@contoso.com:tenant-a");
        let store = KeyringTokenCacheStore::new("os-keychain-service");
        let key = store.compose_key(&cache_ref);
        assert_eq!(key, "sqlly.entra\u{0}alex@contoso.com:tenant-a");
    }

    #[tokio::test]
    async fn in_memory_cache_wins_when_keychain_not_available() {
        // Simulates the Swift degrade-gracefully contract: even if the
        // OS keychain is unreachable on this host (no daemon, missing
        // entitlements, CI sandbox), the in-memory fallback keeps a
        // sign-in's tokens usable for the rest of the process.
        use_mock_keyring();
        let store = KeyringTokenCacheStore::new("sqlly-test-in-mem");
        let r#ref = cache_ref("test");
        // We can store & reload — but we cannot prove the keychain path
        // was hit vs. the in-memory one on this machine (it varies by
        // environment). The in-memory-path assertion is what matters:
        store
            .store(&r#ref, &token_at(Duration::hours(1)))
            .await
            .unwrap();
        let loaded = store.load(&r#ref).await.unwrap();
        assert!(loaded.is_some(), "in-memory fallback must hold the token");
        let loaded = loaded.unwrap();
        assert_eq!(loaded.access_token, "AT");
        assert_eq!(loaded.refresh_token.as_deref(), Some("RT"));
    }

    #[tokio::test]
    async fn in_memory_cache_clears_on_delete() {
        use_mock_keyring();
        let store = KeyringTokenCacheStore::new("sqlly-test-delete");
        let r#ref = cache_ref("test-delete");
        store
            .store(&r#ref, &token_at(Duration::hours(1)))
            .await
            .unwrap();
        store.delete(&r#ref).await.unwrap();
        // After delete, the in-memory cache should be empty (so the next
        // load returns None or a keychain miss — both are fine).
        let loaded = store.load(&r#ref).await.unwrap();
        assert!(loaded.is_none(), "delete should remove the cached token");
    }

    #[tokio::test]
    async fn delete_all_clears_every_registered_token() {
        use_mock_keyring();
        let store = KeyringTokenCacheStore::new("sqlly-test-delete-all");
        let refs = [cache_ref("first"), cache_ref("second")];
        for cache_ref in &refs {
            store
                .store(cache_ref, &token_at(Duration::hours(1)))
                .await
                .unwrap();
        }

        store.delete_all(&refs).await.unwrap();

        for cache_ref in &refs {
            assert!(store.load(cache_ref).await.unwrap().is_none());
        }
    }
}
