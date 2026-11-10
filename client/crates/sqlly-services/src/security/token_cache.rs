//! Secure caching of acquired Entra ID tokens.
//!
//! The *reference* to where a token lives (keychain service + account) is the
//! [`crate::security::azure::EntraTokenCacheRef`] already persisted on each
//! [`crate::security::azure::EntraAccountProfile`]. The token *material* itself
//! ([`AcquiredToken`]) is stored through a [`TokenCacheStore`] seam so the
//! actual secret bytes never live in the serialized account registry — only the
//! reference does. On macOS the production store is keychain-backed; tests use
//! the in-memory store.
//!
//! The [`EntraTokenCache`] ties the store to an [`EntraTokenClient`] so callers
//! can ask for "a valid access token for this account" and transparently get a
//! cached token, a silently-refreshed token, or a clear error telling them to
//! sign in interactively again.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::security::azure::{EntraAccountProfile, EntraTokenCacheRef, EntraTokenState};
use crate::security::token_flow::{AcquiredToken, EntraTokenClient, HttpTransport, TokenFlowError};

/// Persists acquired token material keyed by an [`EntraTokenCacheRef`]. The
/// stored value is the serialized [`AcquiredToken`]; implementations are
/// responsible for storing it securely (e.g. macOS Keychain).
#[async_trait]
pub trait TokenCacheStore: Send + Sync {
    /// Returns the cached token for `cache_ref`, or `None` if absent.
    async fn load(
        &self,
        cache_ref: &EntraTokenCacheRef,
    ) -> Result<Option<AcquiredToken>, TokenCacheError>;

    /// Stores (or replaces) the token for `cache_ref`.
    async fn store(
        &self,
        cache_ref: &EntraTokenCacheRef,
        token: &AcquiredToken,
    ) -> Result<(), TokenCacheError>;

    /// Removes any cached token for `cache_ref` (used on sign-out).
    async fn delete(&self, cache_ref: &EntraTokenCacheRef) -> Result<(), TokenCacheError>;
}

/// Errors from the token cache layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenCacheError {
    /// The backing secure store failed (keychain denied, locked, etc.).
    Storage(String),
    /// A stored token could not be deserialized.
    Corrupt(String),
    /// No usable token and no refresh token: interactive sign-in required.
    InteractiveSignInRequired,
    /// A token-flow (refresh) error bubbled up.
    Flow(TokenFlowError),
}

impl std::fmt::Display for TokenCacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenCacheError::Storage(m) => write!(f, "Secure token store error: {m}"),
            TokenCacheError::Corrupt(m) => write!(f, "Cached token was unreadable: {m}"),
            TokenCacheError::InteractiveSignInRequired => {
                write!(f, "Your session has expired. Please sign in again.")
            }
            TokenCacheError::Flow(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TokenCacheError {}

impl TokenCacheError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::Storage(detail) => {
                DisplayMessage::new("services.security.token_cache.storage", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Corrupt(detail) => {
                DisplayMessage::new("services.security.token_cache.corrupt", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::InteractiveSignInRequired => DisplayMessage::new(
                "services.security.token_cache.interactive_required",
                self.to_string(),
            ),
            Self::Flow(error) => error.message(),
        }
    }
}

impl From<TokenFlowError> for TokenCacheError {
    fn from(value: TokenFlowError) -> Self {
        TokenCacheError::Flow(value)
    }
}

impl From<TokenCacheError> for sqlly_core::StudioError {
    fn from(value: TokenCacheError) -> Self {
        sqlly_core::StudioError::Authentication(value.to_string())
    }
}

/// An in-memory [`TokenCacheStore`] for tests and ephemeral sessions. Not
/// persistent and not secure across processes; production uses a keychain store.
#[derive(Default)]
pub struct InMemoryTokenCacheStore {
    entries: Mutex<BTreeMap<String, String>>,
}

impl InMemoryTokenCacheStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn key(cache_ref: &EntraTokenCacheRef) -> String {
        format!("{}\u{0}{}", cache_ref.service, cache_ref.account)
    }

    /// Test helper: how many tokens are currently held.
    pub fn len(&self) -> usize {
        self.entries.lock().expect("token cache lock").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl TokenCacheStore for InMemoryTokenCacheStore {
    async fn load(
        &self,
        cache_ref: &EntraTokenCacheRef,
    ) -> Result<Option<AcquiredToken>, TokenCacheError> {
        let guard = self.entries.lock().expect("token cache lock");
        match guard.get(&Self::key(cache_ref)) {
            None => Ok(None),
            Some(raw) => serde_json::from_str(raw)
                .map(Some)
                .map_err(|e| TokenCacheError::Corrupt(e.to_string())),
        }
    }

    async fn store(
        &self,
        cache_ref: &EntraTokenCacheRef,
        token: &AcquiredToken,
    ) -> Result<(), TokenCacheError> {
        let raw =
            serde_json::to_string(token).map_err(|e| TokenCacheError::Storage(e.to_string()))?;
        self.entries
            .lock()
            .expect("token cache lock")
            .insert(Self::key(cache_ref), raw);
        Ok(())
    }

    async fn delete(&self, cache_ref: &EntraTokenCacheRef) -> Result<(), TokenCacheError> {
        self.entries
            .lock()
            .expect("token cache lock")
            .remove(&Self::key(cache_ref));
        Ok(())
    }
}

/// Ties a [`TokenCacheStore`] to an [`EntraTokenClient`] to provide
/// "always-valid" access tokens with transparent silent refresh.
pub struct EntraTokenCache<S: TokenCacheStore, T: HttpTransport> {
    store: S,
    client: EntraTokenClient<T>,
}

impl<S: TokenCacheStore, T: HttpTransport> EntraTokenCache<S, T> {
    pub fn new(store: S, client: EntraTokenClient<T>) -> Self {
        Self { store, client }
    }

    /// Persists a freshly acquired token (e.g. just after interactive sign-in).
    pub async fn save(
        &self,
        cache_ref: &EntraTokenCacheRef,
        token: &AcquiredToken,
    ) -> Result<(), TokenCacheError> {
        self.store.store(cache_ref, token).await
    }

    /// Removes any cached token (sign-out).
    pub async fn forget(&self, cache_ref: &EntraTokenCacheRef) -> Result<(), TokenCacheError> {
        self.store.delete(cache_ref).await
    }

    /// Returns a valid access token for `cache_ref`, refreshing silently when
    /// the cached token is expired or expiring soon. When no refresh is
    /// possible, returns [`TokenCacheError::InteractiveSignInRequired`].
    ///
    /// On a successful refresh the new token is written back to the store.
    pub async fn valid_access_token(
        &self,
        cache_ref: &EntraTokenCacheRef,
        now: DateTime<Utc>,
    ) -> Result<AcquiredToken, TokenCacheError> {
        let cached = self
            .store
            .load(cache_ref)
            .await?
            .ok_or(TokenCacheError::InteractiveSignInRequired)?;

        if !token_needs_refresh(&cached, now) {
            return Ok(cached);
        }

        // Expired or expiring soon: try a silent refresh.
        let refresh_token = cached
            .refresh_token
            .clone()
            .ok_or(TokenCacheError::InteractiveSignInRequired)?;

        match self.client.refresh(&refresh_token, now).await {
            Ok(mut refreshed) => {
                // Microsoft may omit a new refresh token; keep the prior one.
                if refreshed.refresh_token.is_none() {
                    refreshed.refresh_token = Some(refresh_token);
                }
                self.store.store(cache_ref, &refreshed).await?;
                Ok(refreshed)
            }
            // A non-recoverable refresh failure means the user must sign in.
            Err(TokenFlowError::Expired)
            | Err(TokenFlowError::ConsentRequired(_))
            | Err(TokenFlowError::OAuth { .. }) => Err(TokenCacheError::InteractiveSignInRequired),
            Err(other) => Err(TokenCacheError::Flow(other)),
        }
    }
}

/// Mints an access token for a *different scope/audience* than the account was
/// signed in with, by exchanging the account's stored refresh token through
/// `scoped_client` (which must be built with the desired scope — e.g. the Azure
/// SQL scope). This mirrors the macOS `AzureTokenService.validSQLAccessToken`
/// strategy: an account signed in with the ARM scope (used for resource
/// discovery) can mint a `database.windows.net` token for the TDS connection
/// because Azure AD refresh tokens are not audience-bound — they can be
/// redeemed for any scope the app is consented to.
///
/// The cached sign-in token (the ARM access token relied on by discovery) is
/// **never overwritten** with the minted SQL token. Only a *rotated* refresh
/// token — when Azure returns a new one — is written back onto the cached
/// entry, so the stored refresh token stays live for future refreshes of
/// either audience. When no valid token/refresh token exists, returns
/// [`TokenCacheError::InteractiveSignInRequired`] so the caller can prompt a
/// fresh interactive sign-in.
pub async fn mint_access_token_for_scope<S: TokenCacheStore, T: HttpTransport>(
    store: &S,
    cache_ref: &EntraTokenCacheRef,
    scoped_client: &EntraTokenClient<T>,
    now: DateTime<Utc>,
) -> Result<AcquiredToken, TokenCacheError> {
    let cached = store
        .load(cache_ref)
        .await?
        .ok_or(TokenCacheError::InteractiveSignInRequired)?;
    let refresh_token = cached
        .refresh_token
        .clone()
        .ok_or(TokenCacheError::InteractiveSignInRequired)?;

    match scoped_client.refresh(&refresh_token, now).await {
        Ok(mut minted) => {
            match minted.refresh_token.clone() {
                // Azure rotated the refresh token: persist the new one onto the
                // cached (sign-in/ARM) entry so the stored refresh token stays
                // live, WITHOUT clobbering the cached access token or expiry.
                Some(rotated) if rotated != refresh_token => {
                    let updated = AcquiredToken {
                        refresh_token: Some(rotated),
                        ..cached.clone()
                    };
                    store.store(cache_ref, &updated).await?;
                }
                // Azure omitted a refresh token on this response: reuse the
                // prior one so the returned bundle still carries a usable one.
                None => {
                    minted.refresh_token = Some(refresh_token);
                }
                _ => {}
            }
            Ok(minted)
        }
        Err(TokenFlowError::Expired)
        | Err(TokenFlowError::ConsentRequired(_))
        | Err(TokenFlowError::OAuth { .. }) => Err(TokenCacheError::InteractiveSignInRequired),
        Err(other) => Err(TokenCacheError::Flow(other)),
    }
}

/// True when the cached token is expired or within the 5-minute refresh window,
/// mirroring [`EntraTokenState`] semantics on [`EntraAccountProfile`].
fn token_needs_refresh(token: &AcquiredToken, now: DateTime<Utc>) -> bool {
    let probe = EntraAccountProfile {
        access_token_expires_at: Some(token.access_token_expires_at),
        ..probe_profile()
    };
    matches!(
        probe.token_state_at(now),
        EntraTokenState::Expired | EntraTokenState::ExpiringSoon | EntraTokenState::Missing
    )
}

/// A throwaway profile used only to reuse the [`EntraTokenState`] window logic.
fn probe_profile() -> EntraAccountProfile {
    EntraAccountProfile::new("probe", "tenant", "home", None, Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::azure::EntraAccountId;
    use crate::security::token_flow::HttpResponse;
    use chrono::Duration;
    use std::sync::Mutex as StdMutex;

    fn cache_ref() -> EntraTokenCacheRef {
        EntraTokenCacheRef::keychain(&EntraAccountId::new("home-a", "tenant-a"))
    }

    fn token(access: &str, refresh: Option<&str>, expires_at: DateTime<Utc>) -> AcquiredToken {
        AcquiredToken {
            access_token: access.to_string(),
            refresh_token: refresh.map(|r| r.to_string()),
            access_token_expires_at: expires_at,
            token_type: "Bearer".to_string(),
            scope: None,
            id_token: None,
        }
    }

    /// Transport that returns scripted responses for refresh calls.
    struct ScriptedTransport {
        responses: StdMutex<Vec<Result<HttpResponse, TokenFlowError>>>,
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post_form(
            &self,
            _url: &str,
            _form: &[(&str, &str)],
        ) -> Result<HttpResponse, TokenFlowError> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .expect("no scripted response")
        }
    }

    fn client(
        responses: Vec<Result<HttpResponse, TokenFlowError>>,
    ) -> EntraTokenClient<ScriptedTransport> {
        EntraTokenClient::new(
            ScriptedTransport {
                responses: StdMutex::new(responses.into_iter().rev().collect()),
            },
            "tenant-a",
            "client-a",
            "scope/.default",
        )
    }

    #[tokio::test]
    async fn missing_token_requires_interactive_sign_in() {
        let store = InMemoryTokenCacheStore::new();
        let cache = EntraTokenCache::new(store, client(vec![]));
        let now = Utc::now();
        let err = cache
            .valid_access_token(&cache_ref(), now)
            .await
            .unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }

    #[tokio::test]
    async fn valid_cached_token_is_returned_without_refresh() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let good = token("access-1", Some("refresh-1"), now + Duration::hours(1));
        store.store(&cref, &good).await.unwrap();

        // Empty client: any network call would panic, proving no refresh happens.
        let cache = EntraTokenCache::new(store, client(vec![]));
        let got = cache.valid_access_token(&cref, now).await.unwrap();
        assert_eq!(got.access_token, "access-1");
    }

    #[tokio::test]
    async fn expiring_token_is_silently_refreshed_and_persisted() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        // Expires in 1 minute → within the 5-minute window → refresh.
        let stale = token(
            "access-old",
            Some("refresh-old"),
            now + Duration::minutes(1),
        );
        store.store(&cref, &stale).await.unwrap();

        let body = r#"{"access_token":"access-new","refresh_token":"refresh-new","expires_in":3600,"token_type":"Bearer"}"#.to_string();
        let cache = EntraTokenCache::new(store, client(vec![Ok(HttpResponse::new(200, body))]));
        let got = cache.valid_access_token(&cref, now).await.unwrap();
        assert_eq!(got.access_token, "access-new");

        // The refreshed token must be written back.
        let reloaded = cache.valid_access_token(&cref, now).await.unwrap();
        assert_eq!(reloaded.access_token, "access-new");
        assert_eq!(reloaded.refresh_token.as_deref(), Some("refresh-new"));
    }

    #[tokio::test]
    async fn refresh_preserves_prior_refresh_token_when_idp_omits_it() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let stale = token(
            "access-old",
            Some("refresh-keep"),
            now + Duration::minutes(1),
        );
        store.store(&cref, &stale).await.unwrap();

        // Response with no refresh_token.
        let body = r#"{"access_token":"access-new","expires_in":3600,"token_type":"Bearer"}"#;
        let cache = EntraTokenCache::new(
            store,
            client(vec![Ok(HttpResponse::new(200, body.to_string()))]),
        );
        let got = cache.valid_access_token(&cref, now).await.unwrap();
        assert_eq!(got.refresh_token.as_deref(), Some("refresh-keep"));
    }

    #[tokio::test]
    async fn expired_token_without_refresh_requires_interactive() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let expired = token("access-old", None, now - Duration::minutes(1));
        store.store(&cref, &expired).await.unwrap();

        let cache = EntraTokenCache::new(store, client(vec![]));
        let err = cache.valid_access_token(&cref, now).await.unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }

    #[tokio::test]
    async fn failed_refresh_maps_to_interactive_sign_in() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let stale = token(
            "access-old",
            Some("refresh-bad"),
            now + Duration::minutes(1),
        );
        store.store(&cref, &stale).await.unwrap();

        let body = r#"{"error":"invalid_grant","error_description":"expired"}"#;
        let cache = EntraTokenCache::new(
            store,
            client(vec![Ok(HttpResponse::new(400, body.to_string()))]),
        );
        let err = cache.valid_access_token(&cref, now).await.unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }

    #[tokio::test]
    async fn forget_removes_cached_token() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        store
            .store(&cref, &token("a", Some("r"), now + Duration::hours(1)))
            .await
            .unwrap();
        let cache = EntraTokenCache::new(store, client(vec![]));
        cache.forget(&cref).await.unwrap();
        let err = cache.valid_access_token(&cref, now).await.unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }

    // ---- mint_access_token_for_scope (SQL token from ARM refresh token) ----

    #[tokio::test]
    async fn mint_exchanges_refresh_token_for_a_different_scope() {
        // An account signed in with the ARM scope holds a still-valid ARM
        // access token plus a refresh token. Minting a SQL token must redeem
        // that refresh token via the SQL-scoped client and return the SQL
        // access token — mirroring the macOS validSQLAccessToken path.
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let arm = token("arm-access", Some("refresh-1"), now + Duration::hours(1));
        store.store(&cref, &arm).await.unwrap();

        // SQL-scoped client returns a database.windows.net-audience token.
        let body = r#"{"access_token":"sql-access","refresh_token":"refresh-1","expires_in":3600,"token_type":"Bearer","scope":"https://database.windows.net//.default"}"#;
        let sql_client = client(vec![Ok(HttpResponse::new(200, body.to_string()))]);

        let minted = mint_access_token_for_scope(&store, &cref, &sql_client, now)
            .await
            .unwrap();
        assert_eq!(minted.access_token, "sql-access");

        // The cached ARM access token must be preserved untouched (discovery
        // still relies on it) — minting must NOT clobber it with the SQL token.
        let reloaded = store.load(&cref).await.unwrap().unwrap();
        assert_eq!(reloaded.access_token, "arm-access");
    }

    #[tokio::test]
    async fn mint_persists_rotated_refresh_token_without_clobbering_access_token() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        let arm = token("arm-access", Some("refresh-old"), now + Duration::hours(1));
        store.store(&cref, &arm).await.unwrap();

        // Azure rotates the refresh token on redemption.
        let body = r#"{"access_token":"sql-access","refresh_token":"refresh-new","expires_in":3600,"token_type":"Bearer"}"#;
        let sql_client = client(vec![Ok(HttpResponse::new(200, body.to_string()))]);

        let minted = mint_access_token_for_scope(&store, &cref, &sql_client, now)
            .await
            .unwrap();
        assert_eq!(minted.access_token, "sql-access");
        assert_eq!(minted.refresh_token.as_deref(), Some("refresh-new"));

        // The rotated refresh token is written back so the stored entry stays
        // live, but the cached ARM access token is preserved.
        let reloaded = store.load(&cref).await.unwrap().unwrap();
        assert_eq!(reloaded.access_token, "arm-access");
        assert_eq!(reloaded.refresh_token.as_deref(), Some("refresh-new"));
    }

    #[tokio::test]
    async fn mint_without_cached_token_requires_interactive_sign_in() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let sql_client = client(vec![]);
        let err = mint_access_token_for_scope(&store, &cache_ref(), &sql_client, now)
            .await
            .unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }

    #[tokio::test]
    async fn mint_maps_failed_refresh_to_interactive_sign_in() {
        let store = InMemoryTokenCacheStore::new();
        let now = Utc::now();
        let cref = cache_ref();
        store
            .store(
                &cref,
                &token("arm-access", Some("refresh-bad"), now + Duration::hours(1)),
            )
            .await
            .unwrap();

        let body = r#"{"error":"invalid_grant","error_description":"expired"}"#;
        let sql_client = client(vec![Ok(HttpResponse::new(400, body.to_string()))]);
        let err = mint_access_token_for_scope(&store, &cref, &sql_client, now)
            .await
            .unwrap_err();
        assert_eq!(err, TokenCacheError::InteractiveSignInRequired);
    }
}
