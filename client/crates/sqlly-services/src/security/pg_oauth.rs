//! OpenID Connect sign-in for PostgreSQL 18's `oauth` authentication.
//!
//! The server only validates bearer tokens; getting one is the client's job.
//! Like libpq's built-in flow this uses the OAuth 2.0 Device Authorization
//! Grant (RFC 8628): read the provider's discovery document, ask for a device
//! code, show the user the verification URL and code, and poll the token
//! endpoint until they approve. Refresh tokens (RFC 6749 §6) renew the access
//! token later without a browser.
//!
//! Provider URLs must be `https`, except loopback hosts (local test
//! providers), so a token request never crosses the network in clear text.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlly_core::DisplayMessage;
use tokio_util::sync::CancellationToken;

/// The OAuth client SQLLY signs in as, from the connection profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub client_id: String,
    /// Confidential clients only; sent with HTTP Basic (RFC 6749 §2.3.1).
    pub client_secret: Option<String>,
    /// Space-separated scope; empty sends none.
    pub scope: String,
}

/// The endpoints SQLLY needs from a provider's discovery document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcEndpoints {
    pub issuer: String,
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

/// A device-code challenge to show the user (RFC 8628 §3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    /// The verification URL with the code filled in, when the provider offers it.
    pub verification_uri_complete: Option<String>,
    /// Time between token polls; grows by [`SLOW_DOWN_STEP`] on `slow_down`.
    pub interval: Duration,
    /// How long the code stays valid.
    pub expires_in: Duration,
}

impl DeviceAuthorization {
    /// The page to open in the browser: the pre-filled one when offered.
    pub fn browser_url(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// Tokens from the provider, with the access token's absolute expiry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OidcToken {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    pub expires_at: DateTime<Utc>,
}

impl OidcToken {
    /// Whether the access token stays valid for at least `skew` more.
    pub fn is_fresh(&self, now: DateTime<Utc>, skew: Duration) -> bool {
        let skew = chrono::Duration::from_std(skew).unwrap_or_else(|_| chrono::Duration::zero());
        self.expires_at - skew > now
    }
}

/// RFC 8628 §3.5: a `slow_down` answer adds five seconds to the interval.
pub const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);

/// Why an OAuth sign-in failed, worded for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgOAuthError {
    /// A provider URL that is not https (and not loopback).
    InsecureUrl(String),
    /// The discovery document could not be fetched or read.
    Discovery { url: String, detail: String },
    /// The provider has no device authorization endpoint.
    DeviceFlowUnsupported { issuer: String },
    /// The server named a different provider than the connection is set to.
    IssuerMismatch {
        expected: String,
        advertised: String,
    },
    /// No client id configured for the device-code flow.
    MissingClientId,
    /// Network failure talking to the provider.
    Transport(String),
    /// The user declined the sign-in (`access_denied`).
    Denied,
    /// The device code expired before the user finished (`expired_token`).
    Expired,
    /// SQLLY stopped waiting (the code's lifetime ran out locally).
    TimedOut,
    /// The user cancelled the sign-in in SQLLY.
    Cancelled,
    /// A new browser sign-in is needed and the caller cannot show one.
    SignInRequired,
    /// Any other OAuth error the provider reported.
    OAuth { code: String, description: String },
    /// A response SQLLY could not understand.
    InvalidResponse(String),
}

impl PgOAuthError {
    /// The catalog-keyed message for this error (rendered on the UI thread).
    pub fn message(&self) -> DisplayMessage {
        match self {
            Self::InsecureUrl(url) => DisplayMessage::new(
                "services.pg_oauth.insecure_url",
                format!("OAuth sign-in needs an https provider URL; {url} is not one."),
            )
            .with_arg("url", url),
            Self::Discovery { url, detail } => DisplayMessage::new(
                "services.pg_oauth.discovery_failed",
                format!("Could not read the OAuth provider's settings from {url}: {detail}"),
            )
            .with_arg("url", url)
            .with_arg("detail", detail),
            Self::DeviceFlowUnsupported { issuer } => DisplayMessage::new(
                "services.pg_oauth.device_flow_unsupported",
                format!("The OAuth provider {issuer} does not offer device sign-in; paste a bearer token instead."),
            )
            .with_arg("issuer", issuer),
            Self::IssuerMismatch {
                expected,
                advertised,
            } => DisplayMessage::new(
                "services.pg_oauth.issuer_mismatch",
                format!("The server asks for OAuth provider {advertised}, but this connection is set to {expected}; SQLLY did not sign in."),
            )
            .with_arg("expected", expected)
            .with_arg("advertised", advertised),
            Self::MissingClientId => DisplayMessage::new(
                "services.pg_oauth.missing_client_id",
                "Enter the OAuth client ID your provider issued for SQLLY in the connection editor.",
            ),
            Self::Transport(detail) => DisplayMessage::new(
                "services.pg_oauth.transport",
                format!("Could not reach the OAuth provider: {detail}"),
            )
            .with_arg("detail", detail),
            Self::Denied => DisplayMessage::new(
                "services.pg_oauth.denied",
                "The OAuth sign-in was declined.",
            ),
            Self::Expired => DisplayMessage::new(
                "services.pg_oauth.expired",
                "The sign-in code expired before the sign-in finished; connect again for a new code.",
            ),
            Self::TimedOut => DisplayMessage::new(
                "services.pg_oauth.timed_out",
                "Timed out waiting for the OAuth sign-in to finish; connect again for a new code.",
            ),
            Self::Cancelled => DisplayMessage::new(
                "services.pg_oauth.cancelled",
                "OAuth sign-in cancelled.",
            ),
            Self::SignInRequired => DisplayMessage::new(
                "services.pg_oauth.sign_in_required",
                "This connection needs an OAuth sign-in; connect from the SQLLY app once to sign in.",
            ),
            Self::OAuth { code, description } => DisplayMessage::new(
                "services.pg_oauth.provider_error",
                format!("The OAuth provider refused the sign-in ({code}): {description}"),
            )
            .with_arg("error_code", code)
            .with_arg("detail", description),
            Self::InvalidResponse(detail) => DisplayMessage::new(
                "services.pg_oauth.invalid_response",
                format!("Unexpected answer from the OAuth provider: {detail}"),
            )
            .with_arg("detail", detail),
        }
    }
}

impl std::fmt::Display for PgOAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message().fallback())
    }
}

impl std::error::Error for PgOAuthError {}

/// Whether `url` may carry OAuth traffic: https, or http to a loopback host.
pub fn check_url(url: &str) -> Result<url::Url, PgOAuthError> {
    let parsed = url::Url::parse(url).map_err(|_| PgOAuthError::InsecureUrl(url.to_string()))?;
    let loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    match parsed.scheme() {
        "https" => Ok(parsed),
        "http" if loopback => Ok(parsed),
        _ => Err(PgOAuthError::InsecureUrl(url.to_string())),
    }
}

/// The discovery document URL for an issuer (or the URL itself when it
/// already names a `.well-known` document, as a PostgreSQL server's hint does).
pub fn discovery_url(issuer_or_document: &str) -> String {
    let trimmed = issuer_or_document.trim();
    if trimmed.contains("/.well-known/") {
        trimmed.to_string()
    } else {
        format!(
            "{}/.well-known/openid-configuration",
            trimmed.trim_end_matches('/')
        )
    }
}

/// Compare issuer identifiers the way RFC 8414 does (exact, ignoring a
/// trailing slash).
pub fn same_issuer(a: &str, b: &str) -> bool {
    a.trim().trim_end_matches('/') == b.trim().trim_end_matches('/')
}

/// The issuer a discovery document URL belongs to (the part before
/// `/.well-known/`), for comparing against a configured issuer.
pub fn issuer_of_discovery_url(url: &str) -> String {
    match url.find("/.well-known/") {
        Some(at) => url[..at].to_string(),
        None => url.trim_end_matches('/').to_string(),
    }
}

/// The issuer to sign in with: the configured one, the one the server
/// advertises, and a refusal when both are present and disagree (a server
/// must not be able to send the user's sign-in to a provider the connection
/// was not set up for).
pub fn choose_issuer(
    configured: &str,
    advertised_document: Option<&str>,
) -> Result<String, PgOAuthError> {
    let configured = configured.trim();
    match (configured.is_empty(), advertised_document) {
        (false, Some(document))
            if !same_issuer(
                &issuer_of_discovery_url(configured),
                &issuer_of_discovery_url(document),
            ) =>
        {
            Err(PgOAuthError::IssuerMismatch {
                expected: issuer_of_discovery_url(configured),
                advertised: issuer_of_discovery_url(document),
            })
        }
        (false, _) => Ok(configured.to_string()),
        (true, Some(document)) => Ok(document.to_string()),
        (true, None) => Err(PgOAuthError::Discovery {
            url: String::new(),
            detail: "no issuer configured and the server did not name one".into(),
        }),
    }
}

/// HTTP plumbing for the OAuth endpoints.
#[derive(Clone)]
pub struct PgOAuthHttp {
    http: reqwest::Client,
}

impl PgOAuthHttp {
    /// A client with a 30-second request timeout.
    pub fn new() -> Result<Self, PgOAuthError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| PgOAuthError::Transport(error.to_string()))?;
        Ok(Self { http })
    }

    /// Fetch and validate the discovery document for `issuer_or_document`.
    pub async fn discover(&self, issuer_or_document: &str) -> Result<OidcEndpoints, PgOAuthError> {
        let url = discovery_url(issuer_or_document);
        check_url(&url)?;
        let discovery_error = |detail: String| PgOAuthError::Discovery {
            url: url.clone(),
            detail,
        };
        let response = self
            .http
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|error| discovery_error(error.to_string()))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| discovery_error(error.to_string()))?;
        if !status.is_success() {
            return Err(discovery_error(format!("HTTP {}", status.as_u16())));
        }
        let document: serde_json::Value = serde_json::from_str(&body)
            .map_err(|error| discovery_error(format!("not JSON: {error}")))?;
        let field = |name: &str| {
            document
                .get(name)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        let issuer = field("issuer").ok_or_else(|| discovery_error("no issuer".into()))?;
        let token_endpoint =
            field("token_endpoint").ok_or_else(|| discovery_error("no token_endpoint".into()))?;
        let device_authorization_endpoint =
            field("device_authorization_endpoint").ok_or_else(|| {
                PgOAuthError::DeviceFlowUnsupported {
                    issuer: issuer.clone(),
                }
            })?;
        check_url(&token_endpoint)?;
        check_url(&device_authorization_endpoint)?;
        Ok(OidcEndpoints {
            issuer,
            device_authorization_endpoint,
            token_endpoint,
        })
    }

    /// RFC 8628 §3.1: request a device code.
    pub async fn start_device_authorization(
        &self,
        endpoints: &OidcEndpoints,
        client: &OAuthClient,
    ) -> Result<DeviceAuthorization, PgOAuthError> {
        if client.client_id.trim().is_empty() {
            return Err(PgOAuthError::MissingClientId);
        }
        let mut form = vec![("client_id", client.client_id.as_str())];
        if !client.scope.trim().is_empty() {
            form.push(("scope", client.scope.trim()));
        }
        let (status, body) = self
            .post_form(&endpoints.device_authorization_endpoint, client, &form)
            .await?;
        if status >= 400 {
            return Err(oauth_error(&body));
        }
        parse_device_authorization(&body)
    }

    /// One poll of the token endpoint (RFC 8628 §3.4).
    async fn poll_once(
        &self,
        endpoints: &OidcEndpoints,
        client: &OAuthClient,
        device_code: &str,
        now: DateTime<Utc>,
    ) -> Result<OidcToken, PgOAuthError> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", device_code),
            ("client_id", client.client_id.as_str()),
        ];
        let (status, body) = self
            .post_form(&endpoints.token_endpoint, client, &form)
            .await?;
        if status >= 400 {
            return Err(oauth_error(&body));
        }
        parse_token(&body, now)
    }

    /// Poll until the user approves, declines, the code expires, or `cancel`
    /// fires. `authorization_pending` keeps polling, `slow_down` widens the
    /// interval by `slow_down_step` (RFC 8628 says five seconds).
    pub async fn poll_for_token(
        &self,
        endpoints: &OidcEndpoints,
        client: &OAuthClient,
        authorization: &DeviceAuthorization,
        slow_down_step: Duration,
        cancel: &CancellationToken,
    ) -> Result<OidcToken, PgOAuthError> {
        let deadline = tokio::time::Instant::now() + authorization.expires_in;
        let mut interval = authorization.interval;
        loop {
            let wake = tokio::time::Instant::now() + interval;
            if wake > deadline {
                return Err(PgOAuthError::TimedOut);
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(PgOAuthError::Cancelled),
                _ = tokio::time::sleep_until(wake) => {}
            }
            let poll = tokio::select! {
                _ = cancel.cancelled() => return Err(PgOAuthError::Cancelled),
                poll = self.poll_once(endpoints, client, &authorization.device_code, Utc::now()) => poll,
            };
            match poll {
                Ok(token) => return Ok(token),
                Err(PgOAuthError::OAuth { code, .. }) if code == "authorization_pending" => {}
                Err(PgOAuthError::OAuth { code, .. }) if code == "slow_down" => {
                    interval += slow_down_step;
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Renew the access token with a refresh token (RFC 6749 §6). A
    /// provider that does not rotate refresh tokens keeps the old one.
    pub async fn refresh(
        &self,
        endpoints: &OidcEndpoints,
        client: &OAuthClient,
        refresh_token: &str,
    ) -> Result<OidcToken, PgOAuthError> {
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client.client_id.as_str()),
        ];
        if !client.scope.trim().is_empty() {
            form.push(("scope", client.scope.trim()));
        }
        let (status, body) = self
            .post_form(&endpoints.token_endpoint, client, &form)
            .await?;
        if status >= 400 {
            return Err(oauth_error(&body));
        }
        let mut token = parse_token(&body, Utc::now())?;
        if token.refresh_token.is_none() {
            token.refresh_token = Some(refresh_token.to_string());
        }
        Ok(token)
    }

    async fn post_form(
        &self,
        url: &str,
        client: &OAuthClient,
        form: &[(&str, &str)],
    ) -> Result<(u16, String), PgOAuthError> {
        check_url(url)?;
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form.iter().copied())
            .finish();
        let mut request = self
            .http
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body);
        if let Some(secret) = client.client_secret.as_deref().filter(|s| !s.is_empty()) {
            // RFC 6749 §2.3.1: both halves are form-encoded before Basic.
            let encode = |value: &str| {
                url::form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>()
            };
            request = request.basic_auth(encode(&client.client_id), Some(encode(secret)));
        }
        let response = request
            .send()
            .await
            .map_err(|error| PgOAuthError::Transport(error.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|error| PgOAuthError::Transport(error.to_string()))?;
        Ok((status, body))
    }
}

fn json(body: &str) -> Result<serde_json::Value, PgOAuthError> {
    serde_json::from_str(body)
        .map_err(|error| PgOAuthError::InvalidResponse(format!("not JSON: {error}")))
}

/// Map an OAuth error body (RFC 6749 §5.2 / RFC 8628 §3.5).
fn oauth_error(body: &str) -> PgOAuthError {
    let Ok(value) = json(body) else {
        return PgOAuthError::InvalidResponse(body.chars().take(200).collect());
    };
    let code = value
        .get("error")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown_error")
        .to_string();
    let description = value
        .get("error_description")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    match code.as_str() {
        "access_denied" => PgOAuthError::Denied,
        "expired_token" => PgOAuthError::Expired,
        _ => PgOAuthError::OAuth { code, description },
    }
}

/// Parse a device authorization response, clamping the provider's numbers so
/// a broken one cannot spin the poll loop or wait forever.
pub fn parse_device_authorization(body: &str) -> Result<DeviceAuthorization, PgOAuthError> {
    let value = json(body)?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).map(str::to_string);
    let missing = |name: &str| PgOAuthError::InvalidResponse(format!("missing {name}"));
    let verification_uri = field("verification_uri")
        .or_else(|| field("verification_url"))
        .ok_or_else(|| missing("verification_uri"))?;
    let verification_uri_complete = field("verification_uri_complete");
    // The user opens these; refuse anything but a web page.
    check_url(&verification_uri)?;
    if let Some(complete) = &verification_uri_complete {
        check_url(complete)?;
    }
    Ok(DeviceAuthorization {
        device_code: field("device_code").ok_or_else(|| missing("device_code"))?,
        user_code: field("user_code").ok_or_else(|| missing("user_code"))?,
        verification_uri,
        verification_uri_complete,
        interval: Duration::from_secs(
            value
                .get("interval")
                .and_then(|v| v.as_u64())
                .unwrap_or(5)
                .clamp(1, 300),
        ),
        expires_in: Duration::from_secs(
            value
                .get("expires_in")
                .and_then(|v| v.as_u64())
                .unwrap_or(600)
                .clamp(30, 3600),
        ),
    })
}

/// Parse a token response (RFC 6749 §5.1).
pub fn parse_token(body: &str, now: DateTime<Utc>) -> Result<OidcToken, PgOAuthError> {
    let value = json(body)?;
    if value.get("error").is_some() {
        return Err(oauth_error(body));
    }
    let access_token = value
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| PgOAuthError::InvalidResponse("missing access_token".into()))?
        .to_string();
    if let Some(kind) = value.get("token_type").and_then(|v| v.as_str())
        && !kind.eq_ignore_ascii_case("bearer")
    {
        return Err(PgOAuthError::InvalidResponse(format!(
            "token_type {kind} is not Bearer"
        )));
    }
    let lifetime = value
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600)
        .clamp(1, 86_400);
    Ok(OidcToken {
        access_token,
        refresh_token: value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string),
        expires_at: now + chrono::Duration::seconds(lifetime),
    })
}

/// A cached sign-in for one connection, as stored in the OS keyring. It is
/// only reused while the connection still names the same provider, client
/// and scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPgOAuthToken {
    pub issuer: String,
    pub client_id: String,
    pub scope: String,
    pub endpoints: OidcEndpoints,
    pub token: OidcToken,
}

impl CachedPgOAuthToken {
    /// Whether this entry was minted for the given settings.
    pub fn matches(&self, issuer: &str, client_id: &str, scope: &str) -> bool {
        self.issuer == issuer.trim()
            && self.client_id == client_id.trim()
            && self.scope == scope.trim()
    }
}

/// The OAuth settings of a [`sqlly_core::AuthenticationType::PostgresOAuth`]
/// device-code profile, with the client secret resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgOAuthSettings {
    pub issuer: String,
    pub client: OAuthClient,
}

impl PgOAuthSettings {
    /// `None` for any other kind of profile (including the bearer-token flow).
    pub fn from_profile(
        profile: &sqlly_core::ConnectionProfile,
    ) -> sqlly_core::StudioResult<Option<Self>> {
        let sqlly_core::AuthenticationType::PostgresOAuth {
            issuer,
            client_id,
            scope,
            flow: sqlly_core::PgOAuthFlow::DeviceCode,
            secret_ref,
            ..
        } = &profile.authentication
        else {
            return Ok(None);
        };
        let client_secret = if secret_ref.is_empty_in_memory() {
            None
        } else {
            Some(secret_ref.resolve()?).filter(|s| !s.trim().is_empty())
        };
        Ok(Some(Self {
            issuer: issuer.trim().to_string(),
            client: OAuthClient {
                client_id: client_id.trim().to_string(),
                client_secret,
                scope: scope.trim().to_string(),
            },
        }))
    }
}

/// Why getting a token failed: the OAuth side, or the database server
/// (asked first for its provider, so "no OAuth here" shows up before the
/// browser does).
#[derive(Debug)]
pub enum PgOAuthSignInError {
    Flow(PgOAuthError),
    Server(sqlly_core::StudioError),
}

impl PgOAuthSignInError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            Self::Flow(error) => error.message(),
            Self::Server(error) => error.message(),
        }
    }
}

impl From<PgOAuthError> for PgOAuthSignInError {
    fn from(error: PgOAuthError) -> Self {
        Self::Flow(error)
    }
}

/// Access tokens are renewed this long before they expire.
pub const REFRESH_SKEW: Duration = Duration::from_secs(60);

/// The server's answer to a discovery probe: the provider hint, or `None`
/// when it named none (the configured issuer is then used as is).
pub type ServerHint =
    Result<Option<sqlly_engines::pg::oauth::PgOAuthServerHint>, sqlly_core::StudioError>;

/// Get a usable token for a device-code profile, cheapest first:
///
/// 1. the cached token, if it was minted for these settings and is fresh;
/// 2. a refresh with the cached refresh token;
/// 3. a new device-code sign-in: ask the database server which provider and
///    scope it wants (`discover_server`), read the provider's discovery
///    document, hand the code to `on_challenge` (which shows it and opens the
///    browser), and poll. Without `on_challenge` (headless callers) this step
///    fails with [`PgOAuthError::SignInRequired`].
///
/// The returned entry is what the caller stores back in the keyring.
pub async fn acquire_token<F, Fut, C>(
    http: &PgOAuthHttp,
    settings: &PgOAuthSettings,
    cached: Option<CachedPgOAuthToken>,
    discover_server: F,
    on_challenge: Option<C>,
    slow_down_step: Duration,
    cancel: &CancellationToken,
) -> Result<CachedPgOAuthToken, PgOAuthSignInError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ServerHint>,
    C: FnOnce(&DeviceAuthorization, &OidcEndpoints),
{
    let scope_key = settings.client.scope.clone();
    if let Some(entry) = cached
        .filter(|entry| entry.matches(&settings.issuer, &settings.client.client_id, &scope_key))
    {
        if entry.token.is_fresh(Utc::now(), REFRESH_SKEW) {
            return Ok(entry);
        }
        if let Some(refresh_token) = entry.token.refresh_token.as_deref() {
            match http
                .refresh(&entry.endpoints, &settings.client, refresh_token)
                .await
            {
                Ok(token) => return Ok(CachedPgOAuthToken { token, ..entry }),
                // A revoked or expired refresh token: sign in afresh.
                Err(error) => tracing::debug!(%error, "postgres oauth refresh failed"),
            }
        }
    }
    let Some(on_challenge) = on_challenge else {
        return Err(PgOAuthError::SignInRequired.into());
    };
    if settings.client.client_id.is_empty() {
        return Err(PgOAuthError::MissingClientId.into());
    }
    let hint = match discover_server().await {
        Ok(hint) => hint,
        // Nothing to learn from the server; the configured issuer decides.
        Err(error) if is_discovery_unavailable(&error) && !settings.issuer.is_empty() => None,
        Err(error) => return Err(PgOAuthSignInError::Server(error)),
    };
    let advertised = hint
        .as_ref()
        .and_then(|h| h.openid_configuration.as_deref());
    let issuer = choose_issuer(&settings.issuer, advertised)?;
    let mut client = settings.client.clone();
    if client.scope.is_empty() {
        client.scope = hint.and_then(|h| h.scope).unwrap_or_default();
    }
    let endpoints = http.discover(&issuer).await?;
    let authorization = http.start_device_authorization(&endpoints, &client).await?;
    on_challenge(&authorization, &endpoints);
    let token = http
        .poll_for_token(&endpoints, &client, &authorization, slow_down_step, cancel)
        .await?;
    Ok(CachedPgOAuthToken {
        issuer: settings.issuer.clone(),
        client_id: settings.client.client_id.clone(),
        scope: scope_key,
        endpoints,
        token,
    })
}

fn is_discovery_unavailable(error: &sqlly_core::StudioError) -> bool {
    matches!(
        error,
        sqlly_core::StudioError::AuthenticationMessage(message)
            if message.key() == Some("services.pg.oauth_discovery_unavailable")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A recorded request to the mock provider.
    #[derive(Debug, Clone)]
    struct Seen {
        method: String,
        path: String,
        authorization: Option<String>,
        body: String,
    }

    /// A tiny HTTP/1.1 provider: each path answers from its own queue of
    /// (status, body) replies; the last reply repeats.
    struct MockProvider {
        base: String,
        seen: Arc<Mutex<Vec<Seen>>>,
    }

    type Routes = Vec<(&'static str, VecDeque<(u16, String)>)>;

    async fn mock_provider(routes: Vec<(&'static str, Vec<(u16, String)>)>) -> MockProvider {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let routes: Arc<Mutex<Routes>> = Arc::new(Mutex::new(
            routes
                .into_iter()
                .map(|(path, replies)| (path, replies.into_iter().collect()))
                .collect(),
        ));
        let base_for_docs = base.clone();
        let seen_task = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let routes = routes.clone();
                let seen = seen_task.clone();
                let base = base_for_docs.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break at + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let length = head
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            k.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let mut request_line = head.lines().next().unwrap().split(' ');
                    let method = request_line.next().unwrap().to_string();
                    let path = request_line.next().unwrap().to_string();
                    let authorization = head.lines().find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("authorization")
                            .then(|| v.trim().to_string())
                    });
                    seen.lock().unwrap().push(Seen {
                        method,
                        path: path.clone(),
                        authorization,
                        body: String::from_utf8_lossy(&buf[head_end..]).to_string(),
                    });
                    let (status, body) = {
                        let mut routes = routes.lock().unwrap();
                        match routes.iter_mut().find(|(p, _)| *p == path) {
                            Some((_, replies)) if replies.len() > 1 => replies.pop_front().unwrap(),
                            Some((_, replies)) => replies.front().cloned().unwrap(),
                            None => (404, "{}".to_string()),
                        }
                    };
                    let body = body.replace("{base}", &base);
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        MockProvider { base, seen }
    }

    fn discovery_doc() -> (u16, String) {
        (
            200,
            r#"{"issuer":"{base}","device_authorization_endpoint":"{base}/device","token_endpoint":"{base}/token"}"#
                .to_string(),
        )
    }

    fn device_reply() -> (u16, String) {
        (
            200,
            r#"{"device_code":"DEV-1","user_code":"WDJB-MJHT","verification_uri":"{base}/activate","verification_uri_complete":"{base}/activate?code=WDJB-MJHT","interval":1,"expires_in":600}"#
                .to_string(),
        )
    }

    fn pending() -> (u16, String) {
        (400, r#"{"error":"authorization_pending"}"#.to_string())
    }

    fn client() -> OAuthClient {
        OAuthClient {
            client_id: "sqlly".into(),
            client_secret: None,
            scope: "openid postgres".into(),
        }
    }

    /// Discover + start, with the interval shortened so tests stay fast.
    async fn begin(
        provider: &MockProvider,
        client: &OAuthClient,
    ) -> (PgOAuthHttp, OidcEndpoints, DeviceAuthorization) {
        let http = PgOAuthHttp::new().unwrap();
        let endpoints = http.discover(&provider.base).await.unwrap();
        let mut authorization = http
            .start_device_authorization(&endpoints, client)
            .await
            .unwrap();
        authorization.interval = Duration::from_millis(10);
        (http, endpoints, authorization)
    }

    #[test]
    fn discovery_url_appends_the_well_known_path_once() {
        assert_eq!(
            discovery_url("https://idp.example.com/realms/pg/"),
            "https://idp.example.com/realms/pg/.well-known/openid-configuration"
        );
        let document = "https://idp.example.com/.well-known/oauth-authorization-server";
        assert_eq!(discovery_url(document), document);
    }

    #[test]
    fn provider_urls_must_be_https_except_loopback() {
        assert!(check_url("https://idp.example.com").is_ok());
        assert!(check_url("http://127.0.0.1:8080/x").is_ok());
        assert!(check_url("http://localhost/x").is_ok());
        assert_eq!(
            check_url("http://idp.example.com").unwrap_err(),
            PgOAuthError::InsecureUrl("http://idp.example.com".into())
        );
        assert!(check_url("javascript:alert(1)").is_err());
    }

    #[test]
    fn a_server_cannot_redirect_a_configured_connection_to_another_provider() {
        let advertised = "https://evil.example.com/.well-known/openid-configuration";
        assert!(matches!(
            choose_issuer("https://idp.example.com", Some(advertised)),
            Err(PgOAuthError::IssuerMismatch { .. })
        ));
        assert_eq!(
            choose_issuer(
                "https://idp.example.com/",
                Some("https://idp.example.com/.well-known/openid-configuration")
            )
            .unwrap(),
            "https://idp.example.com/"
        );
        assert_eq!(choose_issuer("", Some(advertised)).unwrap(), advertised);
        assert!(choose_issuer(" ", None).is_err());
    }

    #[test]
    fn token_response_parses_with_absolute_expiry() {
        let now = Utc::now();
        let token = parse_token(
            r#"{"access_token":"AT","refresh_token":"RT","token_type":"Bearer","expires_in":300}"#,
            now,
        )
        .unwrap();
        assert_eq!(token.access_token, "AT");
        assert_eq!(token.refresh_token.as_deref(), Some("RT"));
        assert_eq!(token.expires_at, now + chrono::Duration::seconds(300));
        assert!(token.is_fresh(now, Duration::from_secs(60)));
        assert!(!token.is_fresh(now, Duration::from_secs(301)));
        assert!(parse_token(r#"{"access_token":"AT","token_type":"mac"}"#, now).is_err());
        assert_eq!(
            parse_token(r#"{"error":"access_denied"}"#, now).unwrap_err(),
            PgOAuthError::Denied
        );
    }

    #[test]
    fn device_authorization_rejects_a_non_web_verification_url() {
        let body = r#"{"device_code":"d","user_code":"u","verification_uri":"file:///etc/passwd"}"#;
        assert!(matches!(
            parse_device_authorization(body),
            Err(PgOAuthError::InsecureUrl(_))
        ));
    }

    #[tokio::test]
    async fn device_flow_polls_through_pending_and_slow_down_to_a_token() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            ("/device", vec![device_reply()]),
            (
                "/token",
                vec![
                    pending(),
                    (400, r#"{"error":"slow_down"}"#.to_string()),
                    pending(),
                    (
                        200,
                        r#"{"access_token":"AT-1","refresh_token":"RT-1","token_type":"Bearer","expires_in":3600}"#
                            .to_string(),
                    ),
                ],
            ),
        ])
        .await;
        let client = client();
        let (http, endpoints, authorization) = begin(&provider, &client).await;
        assert_eq!(endpoints.issuer, provider.base);
        assert_eq!(authorization.user_code, "WDJB-MJHT");
        assert!(authorization.browser_url().ends_with("?code=WDJB-MJHT"));

        let token = http
            .poll_for_token(
                &endpoints,
                &client,
                &authorization,
                Duration::from_millis(10),
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(token.access_token, "AT-1");
        assert_eq!(token.refresh_token.as_deref(), Some("RT-1"));

        let seen = provider.seen.lock().unwrap().clone();
        let device = seen.iter().find(|s| s.path == "/device").unwrap();
        assert_eq!(device.method, "POST");
        assert!(device.body.contains("client_id=sqlly"));
        assert!(device.body.contains("scope=openid+postgres"));
        let polls: Vec<_> = seen.iter().filter(|s| s.path == "/token").collect();
        assert_eq!(polls.len(), 4, "pending, slow_down, pending, success");
        assert!(
            polls[0]
                .body
                .contains("grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code")
        );
        assert!(polls[0].body.contains("device_code=DEV-1"));
        assert!(
            polls[0].authorization.is_none(),
            "public client sends no secret"
        );
    }

    #[tokio::test]
    async fn a_declined_sign_in_is_reported_as_denied() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            ("/device", vec![device_reply()]),
            (
                "/token",
                vec![pending(), (400, r#"{"error":"access_denied"}"#.to_string())],
            ),
        ])
        .await;
        let client = client();
        let (http, endpoints, authorization) = begin(&provider, &client).await;
        let error = http
            .poll_for_token(
                &endpoints,
                &client,
                &authorization,
                Duration::ZERO,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error, PgOAuthError::Denied);
        assert_eq!(error.message().key(), Some("services.pg_oauth.denied"));
    }

    #[tokio::test]
    async fn an_expired_code_and_a_local_timeout_are_reported() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            ("/device", vec![device_reply()]),
            (
                "/token",
                vec![(400, r#"{"error":"expired_token"}"#.to_string())],
            ),
        ])
        .await;
        let client = client();
        let (http, endpoints, mut authorization) = begin(&provider, &client).await;
        let cancel = CancellationToken::new();
        assert_eq!(
            http.poll_for_token(&endpoints, &client, &authorization, Duration::ZERO, &cancel)
                .await
                .unwrap_err(),
            PgOAuthError::Expired
        );
        // The next poll would land after the code's lifetime: stop waiting.
        authorization.expires_in = Duration::from_millis(5);
        authorization.interval = Duration::from_millis(50);
        assert_eq!(
            http.poll_for_token(&endpoints, &client, &authorization, Duration::ZERO, &cancel)
                .await
                .unwrap_err(),
            PgOAuthError::TimedOut
        );
    }

    #[tokio::test]
    async fn cancelling_stops_the_poll() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            ("/device", vec![device_reply()]),
            ("/token", vec![pending()]),
        ])
        .await;
        let client = client();
        let (http, endpoints, authorization) = begin(&provider, &client).await;
        let cancel = CancellationToken::new();
        let stopper = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.cancel();
        });
        assert_eq!(
            http.poll_for_token(&endpoints, &client, &authorization, Duration::ZERO, &cancel)
                .await
                .unwrap_err(),
            PgOAuthError::Cancelled
        );
    }

    #[tokio::test]
    async fn a_confidential_client_authenticates_with_basic_and_refresh_keeps_the_refresh_token() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            (
                "/token",
                vec![(
                    200,
                    r#"{"access_token":"AT-2","expires_in":60}"#.to_string(),
                )],
            ),
        ])
        .await;
        let client = OAuthClient {
            client_secret: Some("s3cr:et".into()),
            ..client()
        };
        let http = PgOAuthHttp::new().unwrap();
        let endpoints = http.discover(&provider.base).await.unwrap();
        let token = http.refresh(&endpoints, &client, "RT-1").await.unwrap();
        assert_eq!(token.access_token, "AT-2");
        assert_eq!(token.refresh_token.as_deref(), Some("RT-1"));
        let seen = provider.seen.lock().unwrap().clone();
        let refresh = seen.iter().find(|s| s.path == "/token").unwrap();
        assert!(refresh.body.contains("grant_type=refresh_token"));
        assert!(refresh.body.contains("refresh_token=RT-1"));
        use base64::Engine as _;
        let expected = base64::engine::general_purpose::STANDARD.encode("sqlly:s3cr%3Aet");
        assert_eq!(
            refresh.authorization.as_deref(),
            Some(format!("Basic {expected}").as_str())
        );
    }

    #[tokio::test]
    async fn discovery_failures_are_readable() {
        let provider = mock_provider(vec![(
            "/.well-known/openid-configuration",
            vec![(
                200,
                r#"{"issuer":"{base}","token_endpoint":"{base}/token"}"#.to_string(),
            )],
        )])
        .await;
        let http = PgOAuthHttp::new().unwrap();
        assert_eq!(
            http.discover(&provider.base).await.unwrap_err(),
            PgOAuthError::DeviceFlowUnsupported {
                issuer: provider.base.clone()
            }
        );
        let missing = http
            .discover(&format!("{}/nowhere", provider.base))
            .await
            .unwrap_err();
        assert!(
            matches!(&missing, PgOAuthError::Discovery { detail, .. } if detail.contains("404")),
            "{missing:?}"
        );
        assert_eq!(
            missing.message().key(),
            Some("services.pg_oauth.discovery_failed")
        );
        assert!(matches!(
            http.discover("http://idp.example.com").await.unwrap_err(),
            PgOAuthError::InsecureUrl(_)
        ));
    }

    #[test]
    fn cached_tokens_only_match_their_own_settings() {
        let cached = CachedPgOAuthToken {
            issuer: "https://idp".into(),
            client_id: "sqlly".into(),
            scope: "openid".into(),
            endpoints: OidcEndpoints {
                issuer: "https://idp".into(),
                device_authorization_endpoint: "https://idp/device".into(),
                token_endpoint: "https://idp/token".into(),
            },
            token: OidcToken {
                access_token: "AT".into(),
                refresh_token: None,
                expires_at: Utc::now(),
            },
        };
        assert!(cached.matches("https://idp", "sqlly", " openid "));
        assert!(!cached.matches("https://idp", "other", "openid"));
        assert!(!cached.matches("https://idp", "sqlly", "openid email"));
        let json = serde_json::to_string(&cached).unwrap();
        assert_eq!(
            serde_json::from_str::<CachedPgOAuthToken>(&json).unwrap(),
            cached
        );
    }

    // ---- acquire_token -----------------------------------------------------

    fn settings(issuer: &str, scope: &str) -> PgOAuthSettings {
        PgOAuthSettings {
            issuer: issuer.into(),
            client: OAuthClient {
                client_id: "sqlly".into(),
                client_secret: None,
                scope: scope.into(),
            },
        }
    }

    fn cached_entry(base: &str, expires_in: i64, refresh: Option<&str>) -> CachedPgOAuthToken {
        CachedPgOAuthToken {
            issuer: base.into(),
            client_id: "sqlly".into(),
            scope: String::new(),
            endpoints: OidcEndpoints {
                issuer: base.into(),
                device_authorization_endpoint: format!("{base}/device"),
                token_endpoint: format!("{base}/token"),
            },
            token: OidcToken {
                access_token: "AT-old".into(),
                refresh_token: refresh.map(str::to_string),
                expires_at: Utc::now() + chrono::Duration::seconds(expires_in),
            },
        }
    }

    type Challenge = fn(&DeviceAuthorization, &OidcEndpoints);

    async fn never_asked() -> ServerHint {
        panic!("the server must not be asked")
    }

    #[tokio::test]
    async fn a_fresh_cached_token_is_reused_without_any_request() {
        let http = PgOAuthHttp::new().unwrap();
        let entry = cached_entry("https://idp.invalid", 3600, None);
        let got = acquire_token(
            &http,
            &settings("https://idp.invalid", ""),
            Some(entry.clone()),
            never_asked,
            None::<Challenge>,
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(got, entry);
    }

    #[tokio::test]
    async fn a_stale_token_is_refreshed_without_the_browser() {
        let provider = mock_provider(vec![(
            "/token",
            vec![(
                200,
                r#"{"access_token":"AT-new","refresh_token":"RT-2","expires_in":3600}"#.to_string(),
            )],
        )])
        .await;
        let http = PgOAuthHttp::new().unwrap();
        let got = acquire_token(
            &http,
            &settings(&provider.base, ""),
            Some(cached_entry(&provider.base, 10, Some("RT-1"))),
            never_asked,
            None::<Challenge>,
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(got.token.access_token, "AT-new");
        assert_eq!(got.token.refresh_token.as_deref(), Some("RT-2"));
        let seen = provider.seen.lock().unwrap().clone();
        assert!(
            !seen[0].body.contains("scope="),
            "refresh keeps the granted scope"
        );
    }

    #[tokio::test]
    async fn a_cache_for_other_settings_is_ignored_and_headless_callers_must_sign_in() {
        let http = PgOAuthHttp::new().unwrap();
        let mut entry = cached_entry("https://idp.invalid", 3600, None);
        entry.client_id = "someone-else".into();
        let error = acquire_token(
            &http,
            &settings("https://idp.invalid", ""),
            Some(entry),
            never_asked,
            None::<Challenge>,
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            PgOAuthSignInError::Flow(PgOAuthError::SignInRequired)
        ));
    }

    #[tokio::test]
    async fn device_sign_in_follows_the_servers_provider_and_scope() {
        let provider = mock_provider(vec![
            ("/.well-known/openid-configuration", vec![discovery_doc()]),
            ("/device", vec![device_reply()]),
            (
                "/token",
                vec![(
                    200,
                    r#"{"access_token":"AT-3","expires_in":3600}"#.to_string(),
                )],
            ),
        ])
        .await;
        let http = PgOAuthHttp::new().unwrap();
        let document = format!("{}/.well-known/openid-configuration", provider.base);
        let shown = Arc::new(Mutex::new(None));
        let shown_by_challenge = shown.clone();
        let got = acquire_token(
            &http,
            &settings("", ""),
            None,
            || async move {
                Ok(Some(sqlly_engines::pg::oauth::PgOAuthServerHint {
                    status: Some("invalid_token".into()),
                    openid_configuration: Some(document),
                    scope: Some("openid pg".into()),
                }))
            },
            Some(
                move |auth: &DeviceAuthorization, endpoints: &OidcEndpoints| {
                    *shown_by_challenge.lock().unwrap() =
                        Some((auth.user_code.clone(), endpoints.issuer.clone()));
                },
            ),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await;
        // Polling waits the provider's one-second interval once.
        let got = got.unwrap();
        assert_eq!(got.token.access_token, "AT-3");
        assert_eq!(
            shown.lock().unwrap().clone(),
            Some(("WDJB-MJHT".to_string(), provider.base.clone()))
        );
        let seen = provider.seen.lock().unwrap().clone();
        let device = seen.iter().find(|s| s.path == "/device").unwrap();
        assert!(device.body.contains("scope=openid+pg"), "{}", device.body);
    }

    #[tokio::test]
    async fn a_server_without_oauth_fails_before_the_browser_opens() {
        let http = PgOAuthHttp::new().unwrap();
        let error = acquire_token(
            &http,
            &settings("https://idp.invalid", ""),
            None,
            || async {
                Err(sqlly_core::StudioError::AuthenticationMessage(
                    DisplayMessage::new("services.pg.oauth_not_offered", "no oauth"),
                ))
            },
            Some(|_: &DeviceAuthorization, _: &OidcEndpoints| panic!("no challenge")),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.message().key(), Some("services.pg.oauth_not_offered"));
    }

    #[tokio::test]
    async fn a_server_naming_another_provider_is_refused() {
        let http = PgOAuthHttp::new().unwrap();
        let error = acquire_token(
            &http,
            &settings("https://idp.example.com", ""),
            None,
            || async {
                Ok(Some(sqlly_engines::pg::oauth::PgOAuthServerHint {
                    openid_configuration: Some(
                        "https://evil.example.com/.well-known/openid-configuration".into(),
                    ),
                    ..Default::default()
                }))
            },
            Some(|_: &DeviceAuthorization, _: &OidcEndpoints| panic!("no challenge")),
            Duration::ZERO,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.message().key(),
            Some("services.pg_oauth.issuer_mismatch")
        );
    }

    #[test]
    fn settings_come_from_a_device_code_profile_only() {
        let mut profile = sqlly_core::ConnectionProfile::postgres_oauth("pg", "db", 5432, "alice");
        if let sqlly_core::AuthenticationType::PostgresOAuth {
            client_id,
            secret_ref,
            ..
        } = &mut profile.authentication
        {
            *client_id = " sqlly ".into();
            secret_ref.key = "shh".into();
        }
        let settings = PgOAuthSettings::from_profile(&profile).unwrap().unwrap();
        assert_eq!(settings.client.client_id, "sqlly");
        assert_eq!(settings.client.client_secret.as_deref(), Some("shh"));
        if let sqlly_core::AuthenticationType::PostgresOAuth { flow, .. } =
            &mut profile.authentication
        {
            *flow = sqlly_core::PgOAuthFlow::BearerToken;
        }
        assert_eq!(PgOAuthSettings::from_profile(&profile).unwrap(), None);
    }
}
