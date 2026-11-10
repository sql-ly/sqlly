//! Microsoft Entra ID OAuth authorize URL composition + state nonces.
//!
//! Pure helpers — no I/O, no system access — used by every client (gpui and
//! CLI) that needs to launch a browser-based Entra sign-in. The redirect URI
//! uses the "loopback" technique (bind 127.0.0.1:<random_port>, expose it as
//! `http://127.0.0.1:<port>/auth`) because it works on every OS without
//! registering a custom URL scheme or shipping Info.plist metadata. Microsoft
//! Entra ID accepts loopback redirect URIs for public-client apps exactly
//! because no scheme registration is involved.
//!
//! The [`AZURE_PUBLIC_CLIENT_ID`] is the SQLLY-registered public-client app
//! id — it identifies the application rather than authenticating it, so it
//! is safe to commit in source.

use base64::Engine;

/// SQLLY's public-client app id registered with Microsoft Entra ID. Same
/// value as the Swift app's `AzureTokenService.clientId` and
/// `AzureAccountSupport.browserSignInURL(...)`. App identifier — not a secret.
pub const AZURE_PUBLIC_CLIENT_ID: &str = "ea03eac1-8824-44dd-a990-6f93030d3590";

/// Path the loopback HTTP server listens on. Microsoft v2.0 authorize
/// requires the redirect path to be registered alongside the host; we pin it
/// to `/auth` for symmetry with the Swift app's `sqlly-auth://auth`.
pub const LOOPBACK_REDIRECT_PATH: &str = "/auth";

/// Scopes requested at the browser prompt.
///
/// This is the **Azure Resource Manager (ARM)** scope, matching the working
/// macOS app (`AzureAccountSupport.browserSignInURL` / `AzureTokenService`
/// `armScope`). The primary token minted by sign-in is used to *discover*
/// Azure SQL resources (subscriptions → servers → databases) and to manage
/// firewall rules — both of which call `https://management.azure.com`. That
/// endpoint rejects a `database.windows.net`-audience token, so signing in
/// with the SQL scope leaves discovery and the server explorer permanently
/// empty. The SQL-audience token needed for the TDS connection is derived
/// separately by refreshing this account's refresh token with
/// [`AZURE_SQL_SCOPE`] (mirroring the macOS `validSQLAccessToken` path).
///
/// - `https://management.azure.com/.default` — ARM audience. Required for the
///   subscription/server/database discovery walk and firewall provisioning.
/// - `offline_access` — required to receive a `refresh_token` so the user
///   does not have to re-authenticate every hour, and so a SQL-audience token
///   can be minted on demand for TDS without a second interactive sign-in.
/// - `openid` + `profile` — required to receive an `id_token` from which the
///   app reads the tenant id / username / display name.
pub const AZURE_BROWSER_SCOPE: &str =
    "https://management.azure.com/.default offline_access openid profile";

/// Azure SQL scope. SQL Server (TDS) rejects ARM tokens because the token
/// audience must be `database.windows.net`. A SQL-audience token is minted on
/// demand by refreshing the account's refresh token (acquired at sign-in with
/// [`AZURE_BROWSER_SCOPE`] + `offline_access`) against this scope — the same
/// two-audience strategy the macOS `AzureTokenService.validSQLAccessToken`
/// uses. The double slash after `windows.net` is the historically-documented
/// Azure SQL resource form and is intentional.
pub const AZURE_SQL_SCOPE: &str =
    "https://database.windows.net//.default offline_access openid profile";

/// Azure Cache for Redis / Azure Managed Redis scope. Like the SQL scope, a
/// Redis-audience token is minted on demand from the account's refresh
/// token; Redis takes it as the `AUTH` password with the object id as user.
pub const AZURE_REDIS_SCOPE: &str =
    "https://redis.azure.com/.default offline_access openid profile";

/// The data-plane scope a connection of `kind` signs in with: Redis for the
/// RESP engines, Azure SQL for everything else that uses Entra.
pub fn entra_data_scope(kind: sqlly_lang::sqlmodel::ConnectionKind) -> &'static str {
    match kind {
        sqlly_lang::sqlmodel::ConnectionKind::Resp => AZURE_REDIS_SCOPE,
        _ => AZURE_SQL_SCOPE,
    }
}

/// `tenant_id` used when the multi-tenant `common` endpoint is desired.
/// Equivalent to passing `tenant_id = nil` in the Swift app, which still
/// routes the user to the `organizations` token endpoint after auth.
pub const COMMON_TENANT_ID: &str = "common";

/// Builds the Microsoft identity platform v2.0 authorize URL the user's
/// browser should be sent to. The redirect_uri MUST be the loopback `/auth`
/// endpoint the local capture server is bound to. `state` is opaque CSRF
/// defense — generated on the app side and echoed by AAD in the callback so
/// the capture server rejects callbacks whose `state` does not match.
///
/// `\n` splitting the parameter section keeps the formatting testable from
/// Rust while still producing a single valid URL on the wire.
pub fn build_authorize_url(
    tenant_id: &str,
    client_id: &str,
    redirect_uri: &str,
    scope: &str,
    state: &str,
    code_challenge: &str,
) -> String {
    let params = [
        ("client_id", client_id),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", scope),
        ("state", state),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("prompt", "select_account"),
    ]
    .iter()
    .map(|(k, v)| format!("{}={}", k, percent_encode_component(v)))
    .collect::<Vec<_>>()
    .join("&");
    format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize?{}",
        percent_encode_component(tenant_id),
        params,
    )
}

/// Builds the loopback redirect URI for a port chosen by the orchestrator
/// (the local HTTP server picks a free port and passes it back here before
/// the authorize URL is composed).
pub fn loopback_redirect_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{LOOPBACK_REDIRECT_PATH}")
}

/// Generates an opaque, unguessable CSRF state nonce. 32 bytes of OS RNG
/// rendered as URL-safe base64 (RFC 4648 §5) — 43 ASCII chars, no `=` padding
/// to leak the byte-count. Uniqueness-not-secret is what CSRF state needs:
/// the local capture server checks the callback's `state` against the one it
/// generated, and a 256-bit random value makes a guessing attack infeasible.
pub fn generate_state() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Generates an RFC 7636 code verifier: 32 bytes of OS RNG as URL-safe
/// base64 (43 chars — within the 43..=128 required range, unreserved chars only).
pub fn generate_code_verifier() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// S256 code challenge for a verifier: BASE64URL-NOPAD(SHA256(ASCII(verifier))).
pub fn code_challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// RFC 3986 §2.1 percent-encoder for URL components. Leaves the unreserved
/// set (`A-Z a-z 0-9 - _ . ~`) alone; everything else is encoded byte-by-byte,
/// so non-ASCII bytes (e.g. UTF-8 in tenant ids) round-trip cleanly. Matching
/// the `URLComponents.percentEncoded` behavior in the Swift app.
fn percent_encode_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{:02X}", byte)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_targets_v2_endpoint_with_required_params() {
        let url = build_authorize_url(
            "common",
            "ea03eac1-8824-44dd-a990-6f93030d3590",
            "http://127.0.0.1:1234/auth",
            "https://database.windows.net//.default offline_access openid profile",
            "state-123",
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM",
        );
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("client_id=ea03eac1-8824-44dd-a990-6f93030d3590"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1234%2Fauth"));
        assert!(url.contains(
            "scope=https%3A%2F%2Fdatabase.windows.net%2F%2F.default%20offline_access%20openid%20profile"
        ));
        assert!(url.contains("state=state-123"));
        assert!(url.contains("code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("prompt=select_account"));
    }

    #[test]
    fn authorize_url_percent_encodes_tenant_with_special_chars() {
        let url = build_authorize_url(
            "tenant with space",
            "client",
            "http://127.0.0.1:1/auth",
            "s",
            "s",
            "challenge",
        );
        assert!(url.contains("/tenant%20with%20space/oauth2/"));
    }

    #[test]
    fn authorize_url_encodes_utf_8_tenant_byte_by_byte() {
        let url = build_authorize_url("tënant", "c", "http://127.0.0.1:1/auth", "s", "s", "c");
        // "ë" is 0xC3 0xAB in UTF-8; both bytes must be percent-encoded.
        assert!(url.contains("%C3%AB"));
    }

    #[test]
    fn loopback_uri_uses_loopback_path() {
        assert_eq!(loopback_redirect_uri(8123), "http://127.0.0.1:8123/auth");
        assert_eq!(loopback_redirect_uri(0), "http://127.0.0.1:0/auth");
    }

    #[test]
    fn state_is_url_safe_and_long_enough_to_unguess() {
        let s = generate_state();
        // 32 bytes → 43 base64 chars (no padding).
        assert_eq!(s.len(), 43);
        assert!(
            s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn state_is_unique_per_call() {
        // Two calls should differ — a static-key or `&[0u8; 32]` bug would
        // both produce the same value here.
        assert_ne!(generate_state(), generate_state());
    }

    #[test]
    fn browser_scope_targets_arm_not_sql() {
        // Regression guard for the "Azure credentials not usable / server
        // explorer never populates" bug: the interactive sign-in MUST request
        // the ARM audience, because discovery and firewall calls hit
        // management.azure.com. A database.windows.net-audience token is
        // rejected by ARM with 401. This mirrors the macOS app's
        // `AzureAccountSupportTests` assertion on the ARM scope.
        assert!(
            AZURE_BROWSER_SCOPE.contains("https://management.azure.com/.default"),
            "browser sign-in must request the ARM scope, got: {AZURE_BROWSER_SCOPE}"
        );
        assert!(
            !AZURE_BROWSER_SCOPE.contains("database.windows.net"),
            "browser sign-in must NOT request the SQL scope (ARM rejects it)"
        );
        // offline_access is required so a SQL-audience token can be minted on
        // demand from the refresh token without a second interactive sign-in.
        assert!(AZURE_BROWSER_SCOPE.contains("offline_access"));
        assert!(AZURE_BROWSER_SCOPE.contains("openid"));
        assert!(AZURE_BROWSER_SCOPE.contains("profile"));
    }

    #[test]
    fn code_challenge_s256_matches_rfc7636_appendix_b() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            code_challenge_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn code_verifier_is_url_safe_and_long_enough() {
        let v = generate_code_verifier();
        assert_eq!(v.len(), 43);
        assert!(
            v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn code_verifier_is_unique_per_call() {
        assert_ne!(generate_code_verifier(), generate_code_verifier());
    }

    #[test]
    fn sql_scope_targets_database_windows_net() {
        // The TDS-audience scope used to derive a SQL access token from the
        // account's refresh token. SQL Server rejects ARM tokens, so this
        // audience must stay database.windows.net.
        assert!(AZURE_SQL_SCOPE.contains("https://database.windows.net//.default"));
        assert!(AZURE_SQL_SCOPE.contains("offline_access"));
    }
}
