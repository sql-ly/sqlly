//! Browser (wasm) variant of [`crate::azure_signin`]. The browser sign-in
//! pipeline needs a loopback HTTP listener for the OAuth redirect, which a
//! web page cannot host — Entra sign-in is desktop-only. `run_signin` fails
//! immediately with an explicit notice; the types mirror the native module
//! so the UI wiring (status bar, accounts dialog) compiles unchanged.

use std::time::Duration;

use sqlly_services::security::azure::{EntraAccountId, EntraAccountProfile};

use crate::azure_http::ReqwestHttpTransport;
use crate::azure_keyring::KeyringTokenCacheStore;
use crate::azure_registry::EntraAccountRegistryStore;

/// Outcome of a complete Entra sign-in. Mirrors the native struct; never
/// constructed in the browser build.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SigninOutcome {
    pub profile_id: EntraAccountId,
    pub profile: EntraAccountProfile,
    pub access_token_expiry: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug)]
pub enum SigninError {
    Unsupported(String),
    Keychain(sqlly_services::security::token_cache::TokenCacheError),
}

impl std::fmt::Display for SigninError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unsupported(message) => write!(f, "{message}"),
            Self::Keychain(error) => {
                let message = crate::localization::format(
                    "connections.entra.keychain_failed",
                    &[("error", &error.to_string())],
                );
                write!(f, "{message}")
            }
        }
    }
}

impl std::error::Error for SigninError {}

impl From<sqlly_services::security::token_cache::TokenCacheError> for SigninError {
    fn from(error: sqlly_services::security::token_cache::TokenCacheError) -> Self {
        Self::Keychain(error)
    }
}

/// Browser build: Entra sign-in cannot run (no loopback redirect listener).
/// Fails with a user-comprehensible notice.
pub async fn run_signin(
    _transport: ReqwestHttpTransport,
    _cache: KeyringTokenCacheStore,
    _registry: EntraAccountRegistryStore,
    _tenant_id: Option<String>,
    _idle_timeout: Duration,
) -> Result<SigninOutcome, SigninError> {
    Err(SigninError::Unsupported(crate::localization::text(
        "connections.entra.browser_signin_unavailable",
    )))
}

/// Mirror of the native helper; stores a token bundle through the cache.
#[allow(dead_code)]
pub async fn store_token_via_cache(
    cache: &KeyringTokenCacheStore,
    cache_ref: &sqlly_services::security::azure::EntraTokenCacheRef,
    token: &sqlly_services::security::token_flow::AcquiredToken,
) -> Result<(), SigninError> {
    use sqlly_services::security::token_cache::TokenCacheStore;
    cache.store(cache_ref, token).await?;
    Ok(())
}
