//! Microsoft Entra ID sign-in for Azure Managed Redis and Azure Cache for
//! Redis.
//!
//! Azure's Redis services accept an Entra access token as the `AUTH`
//! password, with the identity's object id (`oid`) as the user name. The app
//! mints the token for the Redis audience ([`AZURE_REDIS_SCOPE`]) through the
//! same account sign-in and refresh-token exchange SQL Server uses, and
//! stamps it on the profile's runtime-only `runtime_entra_access_token`
//! before every connect. One-shot CLI runs may instead set
//! `SQLLY_ENTRA_TOKEN_<HOST>` or `SQLLY_ENTRA_ACCESS_TOKEN`.
//!
//! Tokens live about an hour and Azure drops a connection whose token has
//! expired, so a connection that outlives its token re-authenticates on the
//! live connection (`AUTH <oid> <token>` is allowed mid-session): every
//! script run sends `AUTH` first when the profile carries a newer token than
//! the one the connection signed in with, and a connection that stays open
//! re-authenticates on its own from the newest token the app has published
//! for its profile ([`publish_token`]) shortly before the current one
//! expires.

use std::collections::HashMap;
use std::sync::OnceLock;

use base64::Engine as _;
use sqlly_core::{AuthenticationType, ConnectionProfile, StudioError, StudioResult};

/// The OAuth resource scope Azure Cache for Redis and Azure Managed Redis
/// accept tokens for.
pub const AZURE_REDIS_SCOPE: &str = "https://redis.azure.com/.default";

/// Host-name suffixes of Azure's Redis endpoints (Azure Cache for Redis in
/// the public, China and US Government clouds, Azure Managed Redis, and the
/// Enterprise tier), for the editor's hint.
pub const AZURE_REDIS_HOST_SUFFIXES: &[&str] = &[
    ".redis.cache.windows.net",
    ".redis.azure.net",
    ".redisenterprise.cache.azure.net",
    ".redis.cache.chinacloudapi.cn",
    ".redis.cache.usgovcloudapi.net",
];

/// How long before a token's expiry a connection re-authenticates.
pub const REAUTH_SKEW_SECS: i64 = 5 * 60;

/// Whether `host` is an Azure Redis endpoint (case-insensitive).
pub fn is_azure_redis_host(host: &str) -> bool {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    AZURE_REDIS_HOST_SUFFIXES
        .iter()
        .any(|suffix| host.ends_with(suffix) && host.len() > suffix.len())
}

/// Whether the profile signs in with Microsoft Entra.
pub fn uses_entra(profile: &ConnectionProfile) -> bool {
    matches!(
        profile.authentication,
        AuthenticationType::EntraBrowser | AuthenticationType::EntraDeviceCode
    )
}

/// The claims of an access token the driver reads: the object id Azure
/// Redis expects as the user name, and the expiry (Unix seconds).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenClaims {
    pub oid: Option<String>,
    pub exp: Option<i64>,
}

/// Read `oid` and `exp` from a JWT's payload without verifying it (the
/// server verifies; the driver only needs the user name and the expiry).
/// Anything that is not a three-part base64url JWT yields no claims.
pub fn token_claims(token: &str) -> TokenClaims {
    let mut parts = token.trim().split('.');
    let (Some(_), Some(payload), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return TokenClaims::default();
    };
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let Some(json) = decoded else {
        return TokenClaims::default();
    };
    TokenClaims {
        oid: json
            .get("oid")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        exp: json.get("exp").and_then(|v| v.as_i64()),
    }
}

/// The user name, token and expiry an Entra profile authenticates with.
#[derive(Clone, PartialEq, Eq)]
pub struct EntraLogin {
    pub user: String,
    pub token: String,
    pub expires_at: Option<i64>,
}

impl std::fmt::Debug for EntraLogin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EntraLogin")
            .field("user", &self.user)
            .field("token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// The access token for the profile: the app-stamped runtime token, else a
/// host-scoped or generic environment variable (one-shot CLI runs).
fn access_token(profile: &ConnectionProfile) -> StudioResult<String> {
    if let Some(token) = profile
        .runtime_entra_access_token
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        return Ok(token.to_string());
    }
    let scoped = format!(
        "SQLLY_ENTRA_TOKEN_{}",
        profile
            .server
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    );
    std::env::var(&scoped)
        .or_else(|_| std::env::var("SQLLY_ENTRA_ACCESS_TOKEN"))
        .ok()
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| {
            StudioError::Authentication(format!(
                "missing Microsoft Entra access token for Redis; sign in to the profile's Azure account (or set {scoped} or SQLLY_ENTRA_ACCESS_TOKEN)"
            ))
        })
}

/// The Entra login for a Redis profile: user name = the Redis user name
/// configured on the profile ([`ConnectionProfile::redis_entra_user`]) when
/// it is not blank, else the token's object id (`oid`), else the user name
/// bound on the profile's Entra account; password = the access token.
pub fn entra_login(profile: &ConnectionProfile) -> StudioResult<EntraLogin> {
    let token = access_token(profile)?;
    let claims = token_claims(&token);
    let configured = profile
        .redis_entra_user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    let user = configured
        .or(claims.oid)
        .or_else(|| {
            profile
                .entra_account
                .as_ref()
                .and_then(|b| b.username.clone())
                .map(|u| u.trim().to_string())
                .filter(|u| !u.is_empty())
        })
        .ok_or_else(|| {
            StudioError::Authentication(
                "the Microsoft Entra token carries no object id (oid) and the profile names no user; Azure Redis signs in with the identity's object id"
                    .into(),
            )
        })?;
    Ok(EntraLogin {
        user,
        token,
        expires_at: claims.exp,
    })
}

/// Whether a connection signed in with a token expiring at `expires_at`
/// should re-authenticate now (inside the skew window, or already expired).
pub fn reauth_due(expires_at: Option<i64>, now: i64) -> bool {
    expires_at.is_some_and(|exp| exp - REAUTH_SKEW_SECS <= now)
}

/// Whether `candidate` should replace the connection's current login: a
/// different token that does not expire earlier.
pub fn is_newer_token(current: &EntraLogin, candidate: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() || candidate == current.token {
        return false;
    }
    match (current.expires_at, token_claims(candidate).exp) {
        (Some(now_exp), Some(new_exp)) => new_exp >= now_exp,
        _ => true,
    }
}

fn token_board() -> &'static parking_lot::Mutex<HashMap<uuid::Uuid, String>> {
    static BOARD: OnceLock<parking_lot::Mutex<HashMap<uuid::Uuid, String>>> = OnceLock::new();
    BOARD.get_or_init(Default::default)
}

/// Publish a freshly minted Redis-audience token for a profile, so open
/// connections of that profile re-authenticate with it before their own
/// token expires. In memory only; never persisted.
pub fn publish_token(profile_id: uuid::Uuid, token: String) {
    token_board().lock().insert(profile_id, token);
}

/// The newest token published for a profile.
pub fn published_token(profile_id: uuid::Uuid) -> Option<String> {
    token_board().lock().get(&profile_id).cloned()
}

/// `profile` carrying the newest of its own runtime token, the token the
/// app last published for it, and `current` (a live connection's login),
/// by expiry; unchanged for a profile without Entra sign-in. Used when an
/// open client dials a further connection (a Sentinel failover's new
/// master, a replica), which must not sign in with an expired token.
pub fn freshest_profile(profile: &ConnectionProfile, current: Option<&str>) -> ConnectionProfile {
    let mut out = profile.clone();
    if !uses_entra(profile) {
        return out;
    }
    let candidates = [
        profile.runtime_entra_access_token.clone(),
        published_token(profile.id.0),
        current.map(str::to_string),
    ];
    let newest = candidates
        .into_iter()
        .flatten()
        .filter(|t| !t.trim().is_empty())
        .max_by_key(|t| token_claims(t).exp.unwrap_or(i64::MIN));
    if newest.is_some() {
        out.runtime_entra_access_token = newest;
    }
    out
}

pub(crate) fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, EntraAccountBinding};

    pub(crate) fn jwt(claims: serde_json::Value) -> String {
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            enc.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
            enc.encode(claims.to_string())
        )
    }

    fn profile(token: Option<String>) -> ConnectionProfile {
        let mut p = ConnectionProfile::azure_sql_entra_browser("r", "cache.redis.azure.net", "");
        p.database_type = DatabaseType::Redis;
        p.database = None;
        p.runtime_entra_access_token = token;
        p
    }

    #[test]
    fn entra_claims_read_oid_and_exp_and_ignore_junk() {
        let token = jwt(serde_json::json!({"oid": "1111-2222", "exp": 1_900_000_000}));
        assert_eq!(
            token_claims(&token),
            TokenClaims {
                oid: Some("1111-2222".into()),
                exp: Some(1_900_000_000)
            }
        );
        assert_eq!(token_claims("opaque-token"), TokenClaims::default());
        assert_eq!(token_claims("a.%%%.c"), TokenClaims::default());
    }

    #[test]
    fn entra_login_uses_oid_then_bound_user() {
        let token = jwt(serde_json::json!({"oid": "oid-1", "exp": 100}));
        let login = entra_login(&profile(Some(token.clone()))).unwrap();
        assert_eq!(login.user, "oid-1");
        assert_eq!(login.token, token);
        assert_eq!(login.expires_at, Some(100));
        assert!(!format!("{login:?}").contains(&token));

        let no_oid = jwt(serde_json::json!({"exp": 100}));
        let mut bound = profile(Some(no_oid.clone()));
        bound.entra_account = Some(EntraAccountBinding {
            account_id: "acct".into(),
            tenant_id: "t".into(),
            username: Some("me@corp.com".into()),
        });
        assert_eq!(entra_login(&bound).unwrap().user, "me@corp.com");
        assert!(matches!(
            entra_login(&profile(Some(no_oid))),
            Err(StudioError::Authentication(_))
        ));
    }

    #[test]
    fn entra_login_uses_a_configured_user_name_and_the_oid_only_when_blank() {
        let token = jwt(serde_json::json!({"oid": "oid-1", "exp": 100}));
        let mut named = profile(Some(token.clone()));
        named.redis_entra_user = Some("  cache-reader  ".into());
        let login = entra_login(&named).unwrap();
        assert_eq!(login.user, "cache-reader");
        assert_eq!(login.token, token);
        named.redis_entra_user = Some("   ".into());
        assert_eq!(entra_login(&named).unwrap().user, "oid-1");
        named.redis_entra_user = None;
        assert_eq!(entra_login(&named).unwrap().user, "oid-1");
        // A configured name also covers a token without an oid.
        let mut no_oid = profile(Some(jwt(serde_json::json!({"exp": 100}))));
        no_oid.redis_entra_user = Some("svc".into());
        assert_eq!(entra_login(&no_oid).unwrap().user, "svc");
    }

    #[test]
    fn freshest_profile_picks_the_latest_expiring_token() {
        let old = jwt(serde_json::json!({"oid": "o", "exp": 100}));
        let mid = jwt(serde_json::json!({"oid": "o", "exp": 200}));
        let new = jwt(serde_json::json!({"oid": "o", "exp": 300}));
        let p = profile(Some(old.clone()));
        assert_eq!(
            freshest_profile(&p, Some(&mid)).runtime_entra_access_token,
            Some(mid.clone())
        );
        publish_token(p.id.0, new.clone());
        assert_eq!(
            freshest_profile(&p, Some(&mid)).runtime_entra_access_token,
            Some(new)
        );
        let mut plain = p.clone();
        plain.authentication = AuthenticationType::Integrated;
        assert_eq!(
            freshest_profile(&plain, Some(&mid)).runtime_entra_access_token,
            Some(old)
        );
    }

    #[test]
    fn entra_without_token_reports_actionable_error() {
        // A host no environment variable names, so the env fallback is empty
        // unless the generic variable happens to be set on this machine.
        if std::env::var("SQLLY_ENTRA_ACCESS_TOKEN").is_ok() {
            return;
        }
        let mut p = profile(None);
        p.server = "no-token-host-wsk.redis.azure.net".into();
        match entra_login(&p) {
            Err(StudioError::Authentication(msg)) => assert!(msg.contains("Entra"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn entra_reauth_window_and_newer_token_rules() {
        assert!(!reauth_due(None, 1_000));
        assert!(!reauth_due(Some(1_000 + REAUTH_SKEW_SECS + 1), 1_000));
        assert!(reauth_due(Some(1_000 + REAUTH_SKEW_SECS), 1_000));
        assert!(reauth_due(Some(10), 1_000));

        let current = EntraLogin {
            user: "u".into(),
            token: jwt(serde_json::json!({"oid": "u", "exp": 500})),
            expires_at: Some(500),
        };
        assert!(!is_newer_token(&current, &current.token.clone()));
        assert!(!is_newer_token(&current, ""));
        assert!(!is_newer_token(
            &current,
            &jwt(serde_json::json!({"oid": "u", "exp": 400}))
        ));
        assert!(is_newer_token(
            &current,
            &jwt(serde_json::json!({"oid": "u", "exp": 4_000}))
        ));
        assert!(is_newer_token(&current, "opaque"));
    }

    #[test]
    fn entra_azure_host_suffixes() {
        assert!(is_azure_redis_host("contoso.redis.cache.windows.net"));
        assert!(is_azure_redis_host("Contoso.EastUS.Redis.Azure.Net."));
        assert!(is_azure_redis_host("x.redisenterprise.cache.azure.net"));
        assert!(!is_azure_redis_host("redis.cache.windows.net.evil.com"));
        assert!(!is_azure_redis_host(".redis.azure.net"));
        assert!(!is_azure_redis_host("localhost"));
    }

    /// Live: an ACL user named like a token's `oid`, whose passwords are two
    /// tokens, stands in for Azure. The client signs in with the first and
    /// re-authenticates on the open connection with the second; a run that
    /// carries the second token does not re-authenticate again.
    #[tokio::test]
    async fn entra_live_reauth_on_open_connection() {
        let Some(admin) = super::super::live_profile_from_env("wsk-entra-admin") else {
            eprintln!("SQLLY_REDIS_URL not set; skipping");
            return;
        };
        let admin_client = super::super::open_client(&admin).await.expect("admin");
        let user = format!("wsk_entra_{}", std::process::id());
        let first = jwt(serde_json::json!({"oid": user, "exp": 4_000_000_000_i64}));
        let second = jwt(serde_json::json!({"oid": user, "exp": 4_000_000_100_i64}));
        let setup = format!("ACL SETUSER {user} on >{first} >{second} ~wsk:* +@all");
        super::super::query_rows_on_client(&admin_client, &setup)
            .await
            .expect("acl setuser");

        let mut profile = admin.clone();
        profile.authentication = AuthenticationType::EntraBrowser;
        profile.runtime_entra_access_token = Some(first.clone());
        let client = super::super::open_client(&profile)
            .await
            .expect("entra open");
        let whoami = super::super::query_rows_on_client(&client, "ACL WHOAMI")
            .await
            .expect("whoami");
        assert_eq!(
            whoami.rows[0][0],
            sqlly_core::CellValue::String(user.clone())
        );
        assert!(client.refresh_entra(Some(&second)).await.expect("reauth"));
        assert!(!client.refresh_entra(Some(&second)).await.expect("same"));
        assert!(!client.refresh_entra(Some(&first)).await.expect("older"));
        publish_token(profile.id.0, second.clone());
        assert_eq!(published_token(profile.id.0), Some(second));

        let teardown = format!("ACL DELUSER {user}");
        super::super::query_rows_on_client(&admin_client, &teardown)
            .await
            .expect("acl deluser");
    }

    #[test]
    fn entra_token_board_keeps_newest_per_profile() {
        let id = uuid::Uuid::new_v4();
        assert_eq!(published_token(id), None);
        publish_token(id, "a".into());
        publish_token(id, "b".into());
        assert_eq!(published_token(id).as_deref(), Some("b"));
    }
}
