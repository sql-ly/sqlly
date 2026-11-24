//! PostgreSQL 18 OAuth tokens for connect attempts.
//!
//! A device-code profile's access token is stamped onto
//! `ConnectionProfile.runtime_cloud_iam_token` (in memory only) before any
//! engine call, like a cloud IAM token. The sign-in itself (discovery,
//! device code, polling, refresh) lives in
//! `sqlly_services::security::pg_oauth`; this module keeps the resulting
//! tokens in the OS keyring (one entry per connection, service
//! `SQLLY.pg-oauth`, account `token.<connection id>`) plus a process cache,
//! and runs the sign-in on the tokio runtime.

use std::collections::HashMap;
use std::sync::OnceLock;

use chrono::Utc;
use keyring::Entry;
use parking_lot::Mutex;
use sqlly_core::{ConnectionProfile, DisplayMessage};
use sqlly_services::security::pg_oauth::{
    self, CachedPgOAuthToken, DeviceAuthorization, OidcEndpoints, PgOAuthHttp, PgOAuthSettings,
    PgOAuthSignInError, REFRESH_SKEW, SLOW_DOWN_STEP,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const SERVICE: &str = crate::distribution::keychain_service!("SQLLY.pg-oauth");

fn account(id: Uuid) -> String {
    format!("token.{id}")
}

fn memory() -> &'static Mutex<HashMap<Uuid, CachedPgOAuthToken>> {
    static CACHE: OnceLock<Mutex<HashMap<Uuid, CachedPgOAuthToken>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// What the user needs to finish a device-code sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceChallenge {
    pub user_code: String,
    pub verification_uri: String,
    /// The page opened in the browser (pre-filled with the code when the
    /// provider offers that).
    pub browser_url: String,
    pub issuer: String,
}

impl DeviceChallenge {
    fn new(authorization: &DeviceAuthorization, endpoints: &OidcEndpoints) -> Self {
        Self {
            user_code: authorization.user_code.clone(),
            verification_uri: authorization.verification_uri.clone(),
            browser_url: authorization.browser_url().to_string(),
            issuer: endpoints.issuer.clone(),
        }
    }
}

/// The stored sign-in for a connection: the process cache first, then the
/// keyring. A keyring that refuses or holds garbage reads as "none".
pub fn load_cached(id: Uuid) -> Option<CachedPgOAuthToken> {
    if let Some(entry) = memory().lock().get(&id).cloned() {
        return Some(entry);
    }
    let stored = Entry::new(SERVICE, &account(id))
        .ok()?
        .get_password()
        .ok()?;
    let entry: CachedPgOAuthToken = serde_json::from_str(&stored).ok()?;
    memory().lock().insert(id, entry.clone());
    Some(entry)
}

/// Remember a sign-in. The process cache always takes it, so the session
/// keeps working when the keyring refuses; the error says so.
pub fn store_cached(id: Uuid, entry: &CachedPgOAuthToken) -> Result<(), String> {
    memory().lock().insert(id, entry.clone());
    let json = serde_json::to_string(entry).map_err(|error| error.to_string())?;
    Entry::new(SERVICE, &account(id))
        .map_err(|error| error.to_string())?
        .set_password(&json)
        .map_err(|error| error.to_string())
}

/// Drop a connection's stored sign-in (connection deleted).
pub fn forget(id: Uuid) {
    memory().lock().remove(&id);
    if let Ok(entry) = Entry::new(SERVICE, &account(id)) {
        let _ = entry.delete_credential();
    }
}

/// The access token to use right now without any network call: the cached
/// one, if it was minted for this profile's settings and is still fresh.
pub fn fresh_access_token(profile: &ConnectionProfile) -> Option<String> {
    let settings = PgOAuthSettings::from_profile(profile).ok().flatten()?;
    let entry = memory().lock().get(&profile.id.0).cloned()?;
    (entry.matches(
        &settings.issuer,
        &settings.client.client_id,
        &settings.client.scope,
    ) && entry.token.is_fresh(Utc::now(), REFRESH_SKEW))
    .then_some(entry.token.access_token)
}

/// Get an access token for a device-code profile: cached, refreshed, or —
/// when `on_challenge` is given — through a new browser sign-in, whose code
/// `on_challenge` receives (it runs off the UI thread). The result is stored
/// for the next connect. Runs on the tokio runtime.
pub async fn ensure_token(
    profile: ConnectionProfile,
    on_challenge: Option<Box<dyn FnOnce(DeviceChallenge) + Send>>,
    cancel: CancellationToken,
) -> Result<String, DisplayMessage> {
    let task = crate::tokio_handle().spawn(async move {
        let settings = PgOAuthSettings::from_profile(&profile)
            .map_err(|error| error.message())?
            .ok_or_else(|| pg_oauth::PgOAuthError::SignInRequired.message())?;
        let id = profile.id.0;
        let cached = load_cached(id);
        let http = PgOAuthHttp::new().map_err(|error| error.message())?;
        let probe = profile.clone();
        let entry = pg_oauth::acquire_token(
            &http,
            &settings,
            cached,
            || async move { sqlly_engines::pg::discover_oauth(&probe).await.map(Some) },
            on_challenge.map(|show| {
                move |authorization: &DeviceAuthorization, endpoints: &OidcEndpoints| {
                    show(DeviceChallenge::new(authorization, endpoints))
                }
            }),
            SLOW_DOWN_STEP,
            &cancel,
        )
        .await
        .map_err(|error: PgOAuthSignInError| error.message())?;
        if let Err(error) = store_cached(id, &entry) {
            tracing::warn!(%error, "could not save the PostgreSQL OAuth token to the keyring");
        }
        Ok(entry.token.access_token)
    });
    task.await.unwrap_or_else(|error| {
        Err(DisplayMessage::new(
            "services.pg_oauth.transport",
            format!("Could not reach the OAuth provider: {error}"),
        )
        .with_arg("detail", error.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::security::pg_oauth::OidcToken;

    fn entry(expires_in: i64) -> CachedPgOAuthToken {
        CachedPgOAuthToken {
            issuer: String::new(),
            client_id: "sqlly".into(),
            scope: String::new(),
            endpoints: OidcEndpoints {
                issuer: "https://idp.example.com".into(),
                device_authorization_endpoint: "https://idp.example.com/device".into(),
                token_endpoint: "https://idp.example.com/token".into(),
            },
            token: OidcToken {
                access_token: "AT".into(),
                refresh_token: Some("RT".into()),
                expires_at: Utc::now() + chrono::Duration::seconds(expires_in),
            },
        }
    }

    fn profile(client_id: &str) -> ConnectionProfile {
        let mut profile = ConnectionProfile::postgres_oauth("pg", "db.example.com", 5432, "alice");
        if let sqlly_core::AuthenticationType::PostgresOAuth { client_id: id, .. } =
            &mut profile.authentication
        {
            *id = client_id.into();
        }
        profile
    }

    #[test]
    fn tokens_round_trip_through_the_keyring() {
        keyring::mock::install();
        let id = Uuid::new_v4();
        assert_eq!(load_cached(id), None);
        let stored = entry(3600);
        store_cached(id, &stored).unwrap();
        memory().lock().remove(&id);
        assert_eq!(load_cached(id), Some(stored));
        forget(id);
        assert_eq!(load_cached(id), None);
    }

    #[test]
    fn only_a_fresh_token_for_the_same_settings_is_used_without_network() {
        keyring::mock::install();
        let mut p = profile("sqlly");
        let _ = store_cached(p.id.0, &entry(3600));
        assert_eq!(fresh_access_token(&p).as_deref(), Some("AT"));
        // Different client id: the cached sign-in belongs to other settings.
        let other = {
            let mut other = profile("someone-else");
            other.id = p.id;
            other
        };
        assert_eq!(fresh_access_token(&other), None);
        // About to expire: needs a refresh first.
        let _ = store_cached(p.id.0, &entry(30));
        assert_eq!(fresh_access_token(&p), None);
        // A bearer-token profile never uses the device-flow cache.
        if let sqlly_core::AuthenticationType::PostgresOAuth { flow, .. } = &mut p.authentication {
            *flow = sqlly_core::PgOAuthFlow::BearerToken;
        }
        let _ = store_cached(p.id.0, &entry(3600));
        assert_eq!(fresh_access_token(&p), None);
        forget(p.id.0);
    }
}
