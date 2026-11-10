//! Mints an Azure SQL (`database.windows.net`) access token for a connect
//! attempt from an account that signed in with the ARM scope.
//!
//! The gpui browser sign-in requests the **ARM** scope so resource discovery
//! and firewall management work (see `authorize_url::AZURE_BROWSER_SCOPE`).
//! SQL Server's TDS gateway, however, rejects ARM tokens — it requires a token
//! whose audience is `database.windows.net`. Azure AD refresh tokens are not
//! audience-bound, so we exchange the account's stored refresh token for a SQL
//! token on demand, exactly as the macOS app's
//! `AzureTokenService.validSQLAccessToken` does.
//!
//! The resulting token is stamped onto the `ConnectionProfile`'s
//! `runtime_entra_access_token` (in-memory only, never persisted) before any
//! engine call so the engine's TDS layer can authenticate.

use chrono::Utc;
use sqlly_core::EntraAccountBinding;
use sqlly_services::security::authorize_url::{AZURE_PUBLIC_CLIENT_ID, AZURE_SQL_SCOPE};
use sqlly_services::security::azure::EntraAccountId;
use sqlly_services::security::token_cache::{TokenCacheError, mint_access_token_for_scope};
use sqlly_services::security::token_flow::EntraTokenClient;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// The skew before a cached SQL token's expiry at which it is treated as stale
/// and re-minted, so it never expires mid-operation. Azure access tokens live
/// ~1h; 5 minutes is comfortable headroom.
const TOKEN_REFRESH_SKEW: chrono::Duration = chrono::Duration::minutes(5);

fn is_fresh(expires_at: chrono::DateTime<Utc>, now: chrono::DateTime<Utc>) -> bool {
    expires_at > now + TOKEN_REFRESH_SKEW
}

/// A shared, cloneable minter of Azure SQL (`database.windows.net`) access
/// tokens. Every connection surface — running a query, connecting, Test
/// Connection, the edit dialog's Databases dropdown, and Server Explorer
/// expansion — mints through one of these so they all refresh the SQL token the
/// same way (5-minute skew) and share a single in-memory cache keyed by Entra
/// account id.
///
/// Before this existed, only the query/connect path re-minted; Test/Databases/
/// Explorer dialed with whatever (possibly hours-stale) token was last stamped,
/// so they failed with "token expired" while a query window against the same
/// server worked.
///
/// The cache is `Rc<RefCell<..>>`: the provider is cloned into several GPUI
/// entities that all run on the single foreground executor, so no locking is
/// needed — and a borrow is never held across an `.await`.
/// account id → (access token, absolute expiry). Shared across every clone of
/// the provider so all connection surfaces reuse one cache.
type SqlTokenCache = Rc<RefCell<HashMap<String, (String, chrono::DateTime<Utc>)>>>;

#[derive(Clone)]
pub struct EntraSqlTokenProvider {
    transport: crate::azure_http::ReqwestHttpTransport,
    keyring: crate::azure_keyring::KeyringTokenCacheStore,
    registry: crate::azure_registry::EntraAccountRegistryStore,
    cache: SqlTokenCache,
}

impl EntraSqlTokenProvider {
    pub fn new(
        transport: crate::azure_http::ReqwestHttpTransport,
        keyring: crate::azure_keyring::KeyringTokenCacheStore,
        registry: crate::azure_registry::EntraAccountRegistryStore,
    ) -> Self {
        Self {
            transport,
            keyring,
            registry,
            cache: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    /// A cached token that is still comfortably valid (past the refresh skew),
    /// or `None`. Synchronous — the fast path callers use before deciding
    /// whether to spawn an async re-mint.
    pub fn cached(&self, account_id: &str) -> Option<(String, chrono::DateTime<Utc>)> {
        self.cached_at(account_id, Utc::now())
    }

    /// Like [`cached`](Self::cached) but uses an explicit clock for freshness tests.
    pub(crate) fn cached_at(
        &self,
        account_id: &str,
        now: chrono::DateTime<Utc>,
    ) -> Option<(String, chrono::DateTime<Utc>)> {
        self.cache
            .borrow()
            .get(account_id)
            .filter(|(_, expires_at)| is_fresh(*expires_at, now))
            .cloned()
    }

    /// Reuse a still-valid cached token, else exchange the account's refresh
    /// token for a fresh SQL-audience token and cache it. The returned token is
    /// ready to stamp onto `ConnectionProfile.runtime_entra_access_token`.
    ///
    /// `scope` picks the data-plane audience — Azure SQL, or Azure Redis
    /// (`https://redis.azure.com/.default`); tokens are cached per account
    /// and audience, so a Redis token never stands in for a SQL one.
    pub async fn ensure_token_for_scope(
        &self,
        binding: &EntraAccountBinding,
        scope: &'static str,
    ) -> SqlTokenOutcome {
        let cache_key = scoped_cache_key(&binding.account_id, scope);
        if let Some((access_token, expires_at)) = self.cached(&cache_key) {
            return SqlTokenOutcome::Token {
                access_token,
                expires_at,
            };
        }
        let account_id = crate::azure_firewall_flow::account_id_from_binding(&binding.account_id);
        let outcome = resolve_scoped_access_token(
            self.transport.clone(),
            self.keyring.clone(),
            self.registry.clone(),
            &account_id,
            scope,
        )
        .await;
        if let SqlTokenOutcome::Token {
            access_token,
            expires_at,
        } = &outcome
        {
            self.cache
                .borrow_mut()
                .insert(cache_key, (access_token.clone(), *expires_at));
        }
        outcome
    }
}

/// The provider's cache key for an account's token of one audience: the
/// bare account id for Azure SQL (the original key), `account|scope`
/// otherwise.
pub fn scoped_cache_key(account_id: &str, scope: &str) -> String {
    if scope == AZURE_SQL_SCOPE {
        account_id.to_string()
    } else {
        format!("{account_id}|{scope}")
    }
}

/// The data-plane scope an Entra connection signs in with: Redis for the
/// Redis-protocol engines, Azure SQL otherwise.
pub fn scope_for_profile(profile: &sqlly_core::ConnectionProfile) -> &'static str {
    sqlly_services::security::authorize_url::entra_data_scope(
        profile.database_type.connection_kind(),
    )
}

/// Stamp a minted token on the profile; a Redis profile's token is also
/// published to its open connections so they re-authenticate before the
/// token they signed in with expires.
pub fn stamp_entra_token(profile: &mut sqlly_core::ConnectionProfile, token: String) {
    #[cfg(not(target_arch = "wasm32"))]
    if profile.database_type.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Resp {
        sqlly_engines::redis::entra::publish_token(profile.id.0, token.clone());
    }
    profile.runtime_entra_access_token = Some(token);
}

/// Outcome of resolving a SQL-audience access token for a connect attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SqlTokenOutcome {
    /// A SQL-audience access token, ready to place on
    /// `ConnectionProfile.runtime_entra_access_token`, together with its
    /// absolute expiry so the caller can cache it and re-mint before it
    /// lapses (Azure access tokens live ~1h).
    Token {
        access_token: String,
        expires_at: chrono::DateTime<Utc>,
    },
    /// The account has no live refresh token (expired / revoked). The user
    /// must sign in again via "Add User".
    SignInRequired,
    /// The connection references an Entra account that is not in the local
    /// registry (never signed in on this machine).
    AccountNotRegistered,
    /// Any other failure (transport, unexpected OAuth error), with a
    /// user-readable summary.
    Failed(String),
}

/// A human label for the Entra account a connection authenticates as, for the
/// "ask my admin" permission-request message. Derived ONLY from the selected
/// binding — never a global/first registered account — so a user with several
/// Entra accounts always names the identity they actually picked. Falls back
/// through username → account id → "my account".
pub fn permission_account_label(binding: Option<&EntraAccountBinding>) -> String {
    let Some(binding) = binding else {
        return crate::localization::text("connections.azure.generic_permission_account");
    };
    if let Some(username) = &binding.username
        && !username.is_empty()
    {
        return username.clone();
    }
    if !binding.account_id.is_empty() {
        return binding.account_id.clone();
    }
    crate::localization::text("connections.azure.generic_permission_account")
}

/// Ensures a profile carries a fresh SQL access token before a dial from a
/// connection surface other than the main app choke point — Test Connection,
/// the edit dialog's Databases dropdown, and Server Explorer expansion. For
/// non-Entra auth or an Entra profile with no bound account, the profile is
/// returned unchanged (nothing to mint). On failure it returns a user-facing
/// message mirroring the guidance the query/connect path shows.
pub async fn hydrate_entra_token(
    provider: &EntraSqlTokenProvider,
    mut profile: sqlly_core::ConnectionProfile,
) -> Result<sqlly_core::ConnectionProfile, String> {
    let binding = match (&profile.authentication, &profile.entra_account) {
        (sqlly_core::AuthenticationType::EntraBrowser, Some(binding)) => binding.clone(),
        _ => return Ok(profile),
    };
    match provider
        .ensure_token_for_scope(&binding, scope_for_profile(&profile))
        .await
    {
        SqlTokenOutcome::Token { access_token, .. } => {
            stamp_entra_token(&mut profile, access_token);
            Ok(profile)
        }
        SqlTokenOutcome::SignInRequired => Err(crate::localization::format(
            "connections.azure.session_expired",
            &[("profile_name", &profile.name)],
        )),
        SqlTokenOutcome::AccountNotRegistered => Err(crate::localization::format(
            "connections.azure.account_not_registered",
            &[("profile_name", &profile.name)],
        )),
        SqlTokenOutcome::Failed(msg) => Err(msg),
    }
}

/// Exchange the registered account's refresh token for an Azure SQL access
/// token. Pure async; no UI. Mirrors the token-resolution shape of
/// [`crate::azure_firewall_flow::run_firewall_flow`] but targets the SQL scope.
/// `scope` is the data-plane audience (Azure SQL or Azure Redis).
pub async fn resolve_scoped_access_token(
    transport: crate::azure_http::ReqwestHttpTransport,
    keyring: crate::azure_keyring::KeyringTokenCacheStore,
    registry: crate::azure_registry::EntraAccountRegistryStore,
    account_id: &EntraAccountId,
    scope: &'static str,
) -> SqlTokenOutcome {
    let profile = match registry.find(account_id) {
        Some(p) => p,
        None => return SqlTokenOutcome::AccountNotRegistered,
    };
    let tenant_id = profile.tenant_id.clone();
    let cache_ref = profile.token_cache_ref.clone();

    // A client bound to the SQL scope. Using the same public client id as
    // sign-in is what keeps any incidental consent screen branded as SQLLY.
    let sql_client = EntraTokenClient::new(transport, tenant_id, AZURE_PUBLIC_CLIENT_ID, scope);

    match mint_access_token_for_scope(&keyring, &cache_ref, &sql_client, Utc::now()).await {
        Ok(token) => SqlTokenOutcome::Token {
            access_token: token.access_token,
            expires_at: token.access_token_expires_at,
        },
        Err(TokenCacheError::InteractiveSignInRequired) => SqlTokenOutcome::SignInRequired,
        Err(e) => SqlTokenOutcome::Failed(crate::localization::format(
            "connections.azure.sql_token_failed",
            &[("error", &e.to_string())],
        )),
    }
}

#[cfg(test)]
impl EntraSqlTokenProvider {
    fn cache_insert_for_test(
        &self,
        account_id: &str,
        access_token: &str,
        expires_at: chrono::DateTime<Utc>,
    ) {
        self.cache.borrow_mut().insert(
            account_id.to_string(),
            (access_token.to_string(), expires_at),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::EntraAccountBinding;

    fn binding(account_id: &str, username: Option<&str>) -> EntraAccountBinding {
        EntraAccountBinding {
            account_id: account_id.to_string(),
            tenant_id: "tenant".to_string(),
            username: username.map(str::to_string),
        }
    }

    #[test]
    fn permission_label_prefers_selected_binding_username() {
        assert_eq!(
            permission_account_label(Some(&binding("acct-1", Some("me@corp.com")))),
            "me@corp.com"
        );
    }

    #[test]
    fn permission_label_falls_back_to_account_id_then_generic() {
        // No username → the account id.
        assert_eq!(
            permission_account_label(Some(&binding("acct-1", None))),
            "acct-1"
        );
        // Empty username is treated as absent.
        assert_eq!(
            permission_account_label(Some(&binding("acct-1", Some("")))),
            "acct-1"
        );
        // No binding at all → the generic label.
        assert_eq!(permission_account_label(None), "my account");
    }

    #[test]
    fn redis_entra_profiles_mint_and_cache_the_redis_audience() {
        use sqlly_services::security::authorize_url::AZURE_REDIS_SCOPE;
        let mut redis = sqlly_core::ConnectionProfile::azure_sql_entra_browser(
            "cache",
            "contoso.redis.cache.windows.net",
            "",
        );
        redis.database_type = sqlly_core::DatabaseType::Redis;
        assert_eq!(scope_for_profile(&redis), AZURE_REDIS_SCOPE);
        let sql = sqlly_core::ConnectionProfile::azure_sql_entra_browser(
            "db",
            "x.database.windows.net",
            "sales",
        );
        assert_eq!(scope_for_profile(&sql), AZURE_SQL_SCOPE);
        // SQL keeps its original key; another audience never shares it.
        assert_eq!(scoped_cache_key("acct", AZURE_SQL_SCOPE), "acct");
        assert_ne!(scoped_cache_key("acct", AZURE_REDIS_SCOPE), "acct");

        let provider = test_provider();
        let now = Utc::now();
        provider.cache_insert_for_test("acct", "sql-token", now + chrono::Duration::hours(1));
        assert_eq!(
            provider.cached_at(&scoped_cache_key("acct", AZURE_REDIS_SCOPE), now),
            None,
            "a SQL token must not stand in for a Redis one"
        );

        stamp_entra_token(&mut redis, "redis-token".into());
        assert_eq!(
            redis.runtime_entra_access_token.as_deref(),
            Some("redis-token")
        );
        assert_eq!(
            sqlly_engines::redis::entra::published_token(redis.id.0).as_deref(),
            Some("redis-token")
        );
    }

    #[test]
    fn is_fresh_boundary_vectors() {
        let now = Utc::now();
        assert!(is_fresh(now + chrono::Duration::hours(1), now));
        assert!(!is_fresh(now + TOKEN_REFRESH_SKEW, now));
        assert!(is_fresh(
            now + TOKEN_REFRESH_SKEW + chrono::Duration::seconds(1),
            now
        ));
        assert!(!is_fresh(now - chrono::Duration::seconds(1), now));
    }

    fn test_provider() -> EntraSqlTokenProvider {
        EntraSqlTokenProvider::new(
            crate::azure_http::ReqwestHttpTransport::build_default().expect("transport"),
            crate::azure_keyring::KeyringTokenCacheStore::new("sqlly-test"),
            crate::azure_registry::EntraAccountRegistryStore::from_path(
                std::env::temp_dir().join(format!("sqlly-token-test-{}", uuid::Uuid::new_v4())),
            )
            .expect("registry path"),
        )
    }

    #[test]
    fn cached_at_respects_skew_and_missing_entries() {
        let provider = test_provider();
        let now = Utc::now();
        assert_eq!(provider.cached_at("missing", now), None);

        provider.cache_insert_for_test("acct", "token-value", now + chrono::Duration::hours(1));
        assert_eq!(
            provider.cached_at("acct", now).map(|(t, _)| t),
            Some("token-value".to_string())
        );

        provider.cache_insert_for_test("acct", "stale", now + TOKEN_REFRESH_SKEW);
        assert_eq!(provider.cached_at("acct", now), None);

        provider.cache_insert_for_test(
            "acct",
            "edge",
            now + TOKEN_REFRESH_SKEW + chrono::Duration::seconds(1),
        );
        assert_eq!(
            provider.cached_at("acct", now).map(|(t, _)| t),
            Some("edge".to_string())
        );
    }
}
