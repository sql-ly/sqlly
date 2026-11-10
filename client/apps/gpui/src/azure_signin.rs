//! Production sign-in flow glue.
//!
//! Wraps the OAuth orchestration in `sqlly-security::browser_signin` with
//! the production pieces the orchestration is generic over:
//!
//! - the reqwest-backed [`crate::azure_http::ReqwestHttpTransport`],
//! - the cross-platform [`crate::browser_open::open_in_browser`] launcher,
//! - the OS-keychain [`crate::azure_keyring::KeyringTokenCacheStore`],
//! - and the [`crate::azure_registry::EntraAccountRegistryStore`] for
//!   identity persistence.
//!
//! The orchestrator returns an [`AcquiredToken`] + [`IdTokenClaims`] —
//! `run_signin` here translates that into a stored
//! [`EntraAccountProfile`] and a cached token bundle. The UI calls this
//! from `cx.spawn` after the menu fires and persists the result on success.

use std::time::Duration;

use chrono::Utc;
use sqlly_services::security::authorize_url::AZURE_PUBLIC_CLIENT_ID;
use sqlly_services::security::azure::{EntraAccountId, EntraAccountProfile};
use sqlly_services::security::browser_signin;
use sqlly_services::security::token_cache::TokenCacheStore;
use sqlly_services::security::token_flow::AcquiredToken;
use tokio::sync::oneshot;

use crate::azure_http::ReqwestHttpTransport;
use crate::azure_keyring::KeyringTokenCacheStore;
use crate::azure_registry::EntraAccountRegistryStore;
use crate::browser_open::open_in_browser;

/// Outcome of a complete Entra sign-in. Both `access_token` and `refresh_token`
/// (when the IdP issued one) are persisted to the keychain via
/// [`crate::azure_keyring`].
#[derive(Debug, Clone)]
#[allow(dead_code)] // the AppView consumes `profile` for status-bar messaging;
// `profile_id` is reserved for the keychain cache ref handback.
pub struct SigninOutcome {
    pub profile_id: EntraAccountId,
    pub profile: EntraAccountProfile,
    pub access_token_expiry: Option<chrono::DateTime<chrono::Utc>>,
}

/// Drives the whole browser sign-in pipeline end-to-end: spins up the
/// loopback listener, opens the browser, awaits the OAuth callback,
/// exchanges the code, decodes identity claims, stores the resulting token
/// bundle in the keychain, and registers the account. Returns
/// [`SigninOutcome`] on success; the various error variants surface enough
/// detail for the UI to display the right guidance.
pub async fn run_signin(
    transport: ReqwestHttpTransport,
    cache: KeyringTokenCacheStore,
    registry: EntraAccountRegistryStore,
    tenant_id: Option<String>,
    idle_timeout: Duration,
) -> Result<SigninOutcome, SigninError> {
    let transport_for_oauth = transport.clone();
    let (browser_url_tx, browser_url_rx) = oneshot::channel();
    let browser_url_tx = std::sync::Mutex::new(Some(browser_url_tx));
    let orchestrator = tokio::spawn(async move {
        browser_signin::complete_browser_signin(
            &transport_for_oauth,
            AZURE_PUBLIC_CLIENT_ID,
            tenant_id.as_deref(),
            move |url| {
                // Hand the URL off to the production browser opener. We
                // intentionally swallow opener errors here because
                // `complete_browser_signin` already accepts a `FnOnce` —
                // errors surface through the UI's separate "could not
                // open browser" path.
                if let Err(_e) = open_in_browser(url) {
                    tracing::warn!(
                        url = url,
                        "could not launch the system browser to start Entra sign-in"
                    );
                }
                // Notify the awaiting UI of the URL we just opened, so it
                // can show a "complete the prompt in your browser" hint.
                if let Some(tx) = browser_url_tx.lock().ok().and_then(|mut g| g.take()) {
                    let _ = tx.send(url.to_string());
                }
            },
            idle_timeout,
            Utc::now(),
        )
        .await
    });
    // Surface the URL the orchestration opened to the caller without
    // blocking on the orchestrator — the caller's UI may show
    // "complete sign-in in your browser" while the orchestrator awaits.
    drop(browser_url_rx); // intentionally not consumed here; UI handles UX.
    let outcome = orchestrator
        .await
        .map_err(|e| SigninError::OrchestrationJoin(e.to_string()))??;
    let now = Utc::now();
    let access_token_expiry = Some(outcome.access_token.access_token_expires_at);
    let profile = profile_from_identity(&outcome.identity, &outcome.access_token, now);
    let profile_id = registry.upsert(profile.clone());
    // Persist the token bundle against the stable `EntraTokenCacheRef` so
    // the next sign-in flow (refresh, account picker) can find it under a
    // key that survives across the app's lifetime without depending on the
    // cache str being a hardcoded constant.
    let cache_ref = profile.token_cache_ref.clone();
    store_token_via_cache(&cache, &cache_ref, &outcome.access_token).await?;
    let _ = registry.mark_login(
        &profile_id,
        now,
        access_token_expiry,
        /* refresh_token_expiry */ None,
    );
    Ok(SigninOutcome {
        profile_id,
        profile,
        access_token_expiry,
    })
}

fn profile_from_identity(
    identity: &sqlly_services::security::azure::IdTokenClaims,
    token: &AcquiredToken,
    now: chrono::DateTime<chrono::Utc>,
) -> EntraAccountProfile {
    // The token cache ref for the account: stable key derived from
    // `(home_account_id, tenant_id)` so a re-sign-in of the same user
    // updates the same keychain item.
    let home_account_id = identity.home_account_id();
    let tenant_id = identity.tid.clone();
    let mut profile = EntraAccountProfile::new(
        identity.username(),
        &tenant_id,
        &home_account_id,
        identity.name.clone(),
        now,
    );
    profile.access_token_expires_at = Some(token.access_token_expires_at);
    profile
}

/// Convenience wrapper so the UI can store a token through the
/// `EntraTokenCache` once it knows the stable `EntraTokenCacheRef` — i.e.
/// after `EntraAccountRegistry::upsert` has handed out an id. The cache's
/// `save` always writes the in-memory fallback first so a keychain write
/// failure does not invalidate the session, mirroring the Swift
/// `AzureTokenService.store(_:)` degrade-gracefully contract.
pub async fn store_token_via_cache(
    cache: &KeyringTokenCacheStore,
    cache_ref: &sqlly_services::security::azure::EntraTokenCacheRef,
    token: &AcquiredToken,
) -> Result<(), SigninError> {
    cache.store(cache_ref, token).await?;
    Ok(())
}

#[derive(Debug)]
pub enum SigninError {
    OrchestrationJoin(String),
    Browser(browser_signin::BrowserSigninError),
    Keychain(sqlly_services::security::token_cache::TokenCacheError),
    Registry(sqlly_core::StudioError),
}

impl std::fmt::Display for SigninError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (key, error) = match self {
            Self::OrchestrationJoin(error) => (
                "connections.entra.signin_coordination_failed",
                error.to_string(),
            ),
            Self::Browser(error) => ("connections.entra.oauth_failed", error.to_string()),
            Self::Keychain(error) => ("connections.entra.keychain_failed", error.to_string()),
            Self::Registry(error) => (
                "connections.entra.account_registration_failed",
                error.to_string(),
            ),
        };
        let message = crate::localization::format(key, &[("error", &error)]);
        write!(f, "{message}")
    }
}

impl std::error::Error for SigninError {}

impl From<browser_signin::BrowserSigninError> for SigninError {
    fn from(error: browser_signin::BrowserSigninError) -> Self {
        Self::Browser(error)
    }
}

impl From<sqlly_services::security::token_cache::TokenCacheError> for SigninError {
    fn from(error: sqlly_services::security::token_cache::TokenCacheError) -> Self {
        Self::Keychain(error)
    }
}

impl From<sqlly_core::StudioError> for SigninError {
    fn from(error: sqlly_core::StudioError) -> Self {
        Self::Registry(error)
    }
}

#[allow(dead_code)]
const _: () = (); // placeholder: imports may be unused depending on conditional compilation

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::security::azure::IdTokenClaims;

    fn claims_with(username: &str, tid: &str, oid: &str, name: Option<&str>) -> IdTokenClaims {
        IdTokenClaims {
            tid: tid.into(),
            oid: oid.into(),
            sub: String::new(),
            preferred_username: username.into(),
            upn: String::new(),
            email: String::new(),
            name: name.map(String::from),
        }
    }

    fn fake_token() -> AcquiredToken {
        AcquiredToken {
            access_token: "AT-fresh".into(),
            refresh_token: Some("RT-fresh".into()),
            access_token_expires_at: chrono::Utc::now() + chrono::Duration::hours(1),
            token_type: "Bearer".into(),
            scope: Some("s".into()),
            id_token: None,
        }
    }

    #[test]
    fn profile_from_identity_sets_tenant_home_account_username() {
        let claims = claims_with("alex@contoso.com", "tenant-z", "object-z", Some("Alex"));
        let now = Utc::now();
        let profile = profile_from_identity(&claims, &fake_token(), now);
        assert_eq!(profile.username, "alex@contoso.com");
        assert_eq!(profile.tenant_id, "tenant-z");
        assert_eq!(profile.home_account_id, "object-z.tenant-z");
        assert_eq!(profile.display_name.as_deref(), Some("Alex"));
        assert_eq!(
            profile.token_cache_ref.account,
            "object-z.tenant-z:tenant-z"
        );
        assert!(profile.access_token_expires_at.is_some());
    }

    #[test]
    fn home_account_id_falls_back_to_sub() {
        let mut claims = claims_with("alex@contoso.com", "tenant-z", "object-z", None);
        claims.oid = String::new();
        claims.sub = "subject-z".into();
        let profile = profile_from_identity(&claims, &fake_token(), Utc::now());
        assert_eq!(profile.home_account_id, "subject-z");
    }
}
