//! Microsoft Entra ID OAuth token acquisition flows.
//!
//! This module implements the *real* token-acquisition logic — device-code
//! polling, authorization-code (browser redirect) exchange, and refresh-token
//! renewal — behind a mockable [`HttpTransport`] seam. No concrete HTTP client
//! is bundled (per project decision: no `reqwest`); the application wires a
//! transport that performs the actual network I/O, while tests inject a scripted
//! transport. This keeps the protocol logic fully unit-testable without live
//! Azure credentials.
//!
//! Reference: OAuth 2.0 device authorization grant (RFC 8628) and authorization
//! code grant as implemented by the Microsoft identity platform v2.0 endpoints.

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A minimal HTTP form-POST transport. The application provides a concrete
/// implementation (e.g. backed by the system URL loader); tests provide a
/// scripted one. Only `application/x-www-form-urlencoded` POSTs are needed for
/// the OAuth token endpoints.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// POSTs `form` (already key/value pairs) to `url` and returns the raw
    /// response. Implementations MUST NOT panic; network failures should be
    /// surfaced as `Err`.
    async fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<HttpResponse, TokenFlowError>;

    /// GETs `url` with the given bearer `access_token` in the `Authorization`
    /// header. Used by the Azure Resource Manager discovery client. Default
    /// implementation returns a transport error so existing POST-only mocks do
    /// not need to implement it.
    async fn get_with_bearer(
        &self,
        url: &str,
        access_token: &str,
    ) -> Result<HttpResponse, TokenFlowError> {
        let _ = (url, access_token);
        Err(TokenFlowError::Transport(
            "get_with_bearer not implemented by this transport".to_string(),
        ))
    }

    /// PUTs `body_json` (a fully serialized JSON request body) to `url` with
    /// the given bearer `access_token` in the `Authorization` header and a
    /// `Content-Type: application/json`. Used by the Azure Resource Manager
    /// firewall-rule provisioner (idempotent PUT on
    /// `Microsoft.Sql/servers/.../firewallRules/<name>`). Default
    /// implementation returns a transport error so existing tests/mocks that
    /// only handle GET/POST do not need to implement it.
    async fn put_json_with_bearer(
        &self,
        url: &str,
        access_token: &str,
        body_json: &str,
    ) -> Result<HttpResponse, TokenFlowError> {
        let _ = (url, access_token, body_json);
        Err(TokenFlowError::Transport(
            "put_json_with_bearer not implemented by this transport".to_string(),
        ))
    }
}

/// A raw HTTP response: status code plus body text (expected to be JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// Errors that can arise during a token flow, mapped to clear, actionable
/// categories so the UI can render specific guidance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenFlowError {
    /// The transport failed (DNS, TLS, socket). Carries a redacted message.
    Transport(String),
    /// The IdP returned a malformed or unparseable response body.
    InvalidResponse(String),
    /// The user has not yet completed the device-code prompt; keep polling.
    AuthorizationPending,
    /// The IdP asked us to back off; the poll interval was increased.
    SlowDown,
    /// The device code / auth session expired before completion.
    Expired,
    /// The user or admin declined consent for the requested scopes.
    ConsentRequired(String),
    /// The account lacks permission for the requested resource/scope.
    InsufficientPermissions(String),
    /// Any other OAuth error, carrying the IdP `error` code and description.
    OAuth { code: String, description: String },
}

impl std::fmt::Display for TokenFlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenFlowError::Transport(m) => write!(f, "Network error contacting Entra ID: {m}"),
            TokenFlowError::InvalidResponse(m) => {
                write!(f, "Unexpected response from Entra ID: {m}")
            }
            TokenFlowError::AuthorizationPending => {
                write!(f, "Waiting for you to complete sign-in…")
            }
            TokenFlowError::SlowDown => write!(f, "Polling too fast; slowing down."),
            TokenFlowError::Expired => write!(
                f,
                "The sign-in request expired. Start the sign-in again to get a new code."
            ),
            TokenFlowError::ConsentRequired(m) => write!(
                f,
                "Admin or user consent is required for Azure SQL access: {m}"
            ),
            TokenFlowError::InsufficientPermissions(m) => write!(
                f,
                "Your account does not have permission for this resource: {m}"
            ),
            TokenFlowError::OAuth { code, description } => {
                write!(f, "Sign-in failed ({code}): {description}")
            }
        }
    }
}

impl std::error::Error for TokenFlowError {}

impl TokenFlowError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::Transport(detail) => {
                DisplayMessage::new("services.security.token_flow.transport", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::InvalidResponse(detail) => DisplayMessage::new(
                "services.security.token_flow.invalid_response",
                self.to_string(),
            )
            .with_arg("detail", detail),
            Self::AuthorizationPending => DisplayMessage::new(
                "services.security.token_flow.authorization_pending",
                self.to_string(),
            ),
            Self::SlowDown => {
                DisplayMessage::new("services.security.token_flow.slow_down", self.to_string())
            }
            Self::Expired => {
                DisplayMessage::new("services.security.token_flow.expired", self.to_string())
            }
            Self::ConsentRequired(detail) => DisplayMessage::new(
                "services.security.token_flow.consent_required",
                self.to_string(),
            )
            .with_arg("detail", detail),
            Self::InsufficientPermissions(detail) => DisplayMessage::new(
                "services.security.token_flow.insufficient_permissions",
                self.to_string(),
            )
            .with_arg("detail", detail),
            Self::OAuth { code, description } => {
                DisplayMessage::new("services.security.token_flow.oauth", self.to_string())
                    .with_arg("error_code", code)
                    .with_arg("detail", description)
            }
        }
    }
}

impl From<TokenFlowError> for sqlly_core::StudioError {
    fn from(value: TokenFlowError) -> Self {
        sqlly_core::StudioError::Authentication(value.to_string())
    }
}

/// A successfully acquired token set, normalized from the IdP response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquiredToken {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Absolute expiry computed from `expires_in` relative to acquisition time.
    pub access_token_expires_at: DateTime<Utc>,
    pub token_type: String,
    pub scope: Option<String>,
    /// The `id_token` JWT the IdP returned alongside the access token at
    /// interactive sign-in time. Required by callers that need the account's
    /// tenant + username + display name without re-prompting the user. Absent
    /// on refresh responses (AAD does not re-issue an id_token on a refresh).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
}

/// Raw device authorization response from the `/devicecode` endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAuthorization {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub message: Option<String>,
    pub interval_seconds: u64,
    pub expires_in_seconds: u64,
}

/// Drives Entra ID token acquisition over an injected [`HttpTransport`].
pub struct EntraTokenClient<T: HttpTransport> {
    transport: T,
    tenant_id: String,
    client_id: String,
    scope: String,
}

impl<T: HttpTransport> EntraTokenClient<T> {
    pub fn new(
        transport: T,
        tenant_id: impl Into<String>,
        client_id: impl Into<String>,
        scope: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            scope: scope.into(),
        }
    }

    fn authority_base(&self) -> String {
        format!("https://login.microsoftonline.com/{}", self.tenant_id)
    }

    fn device_code_url(&self) -> String {
        format!("{}/oauth2/v2.0/devicecode", self.authority_base())
    }

    fn token_url(&self) -> String {
        format!("{}/oauth2/v2.0/token", self.authority_base())
    }

    /// Step 1 of the device-code flow: request a device + user code.
    pub async fn start_device_authorization(&self) -> Result<DeviceAuthorization, TokenFlowError> {
        let form = [
            ("client_id", self.client_id.as_str()),
            ("scope", self.scope.as_str()),
        ];
        let response = self
            .transport
            .post_form(&self.device_code_url(), &form)
            .await?;
        if response.status >= 400 {
            return Err(map_oauth_error(&response.body));
        }
        parse_device_authorization(&response.body)
    }

    /// Step 2 of the device-code flow: poll the token endpoint once. Returns
    /// `Ok(token)` on success, or a [`TokenFlowError`] — callers loop on
    /// [`TokenFlowError::AuthorizationPending`] / [`TokenFlowError::SlowDown`].
    pub async fn poll_device_token(
        &self,
        device_code: &str,
        now: DateTime<Utc>,
    ) -> Result<AcquiredToken, TokenFlowError> {
        let form = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", self.client_id.as_str()),
            ("device_code", device_code),
        ];
        let response = self.transport.post_form(&self.token_url(), &form).await?;
        if response.status >= 400 {
            return Err(map_oauth_error(&response.body));
        }
        parse_token_response(&response.body, now)
    }

    /// Exchange an authorization code (captured from the browser redirect) for
    /// a token set.
    pub async fn exchange_authorization_code(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
        now: DateTime<Utc>,
    ) -> Result<AcquiredToken, TokenFlowError> {
        let form = [
            ("grant_type", "authorization_code"),
            ("client_id", self.client_id.as_str()),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("scope", self.scope.as_str()),
            ("code_verifier", code_verifier),
        ];
        let response = self.transport.post_form(&self.token_url(), &form).await?;
        if response.status >= 400 {
            return Err(map_oauth_error(&response.body));
        }
        parse_token_response(&response.body, now)
    }

    /// Renew an access token using a refresh token.
    pub async fn refresh(
        &self,
        refresh_token: &str,
        now: DateTime<Utc>,
    ) -> Result<AcquiredToken, TokenFlowError> {
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", self.client_id.as_str()),
            ("refresh_token", refresh_token),
            ("scope", self.scope.as_str()),
        ];
        let response = self.transport.post_form(&self.token_url(), &form).await?;
        if response.status >= 400 {
            return Err(map_oauth_error(&response.body));
        }
        parse_token_response(&response.body, now)
    }
}

fn json_object(body: &str) -> Result<BTreeMap<String, serde_json::Value>, TokenFlowError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| TokenFlowError::InvalidResponse(format!("not valid JSON: {e}")))?;
    let obj = value
        .as_object()
        .ok_or_else(|| TokenFlowError::InvalidResponse("expected a JSON object".to_string()))?;
    Ok(obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
}

fn str_field(obj: &BTreeMap<String, serde_json::Value>, key: &str) -> Option<String> {
    obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
}

pub(crate) fn parse_device_authorization(
    body: &str,
) -> Result<DeviceAuthorization, TokenFlowError> {
    let obj = json_object(body)?;
    let device_code = str_field(&obj, "device_code")
        .ok_or_else(|| TokenFlowError::InvalidResponse("missing device_code".to_string()))?;
    let user_code = str_field(&obj, "user_code")
        .ok_or_else(|| TokenFlowError::InvalidResponse("missing user_code".to_string()))?;
    let verification_uri = str_field(&obj, "verification_uri")
        .or_else(|| str_field(&obj, "verification_url"))
        .ok_or_else(|| TokenFlowError::InvalidResponse("missing verification_uri".to_string()))?;
    // Clamped so a broken IdP cannot stall polling indefinitely or spin it hot.
    let interval_seconds = obj
        .get("interval")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .clamp(1, 300);
    // Clamped like the token-response expiry: a hostile/broken IdP must not
    // be able to declare a device code that never expires (or already has).
    let expires_in_seconds = obj
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(900)
        .clamp(30, 86_400);
    Ok(DeviceAuthorization {
        device_code,
        user_code,
        verification_uri,
        message: str_field(&obj, "message"),
        interval_seconds,
        expires_in_seconds,
    })
}

/// Ceiling applied to the IdP's `expires_in`. Values above this are treated as
/// this value — `chrono::Duration::seconds` panics past its millisecond range,
/// so an unclamped hostile/broken endpoint could otherwise crash the client.
const EXPIRES_IN_MAX_SECONDS: i64 = 86_400;

/// Safety margin subtracted from the reported token lifetime so refresh happens
/// before the server-side expiry even when local and IdP clocks disagree.
const EXPIRY_SKEW_LEEWAY_SECONDS: i64 = 120;

pub(crate) fn parse_token_response(
    body: &str,
    now: DateTime<Utc>,
) -> Result<AcquiredToken, TokenFlowError> {
    let obj = json_object(body)?;
    // An error can come back with HTTP 200 in some flows; check defensively.
    if obj.contains_key("error") {
        return Err(map_oauth_error(body));
    }
    let access_token = str_field(&obj, "access_token")
        .ok_or_else(|| TokenFlowError::InvalidResponse("missing access_token".to_string()))?;
    let expires_in = obj
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);
    if expires_in <= 0 {
        return Err(TokenFlowError::InvalidResponse(format!(
            "non-positive expires_in {expires_in}"
        )));
    }
    let expires_in = expires_in.min(EXPIRES_IN_MAX_SECONDS);
    // The leeway must never dominate a legitimately short-lived token, so it is
    // only subtracted when the lifetime clearly exceeds it.
    let effective_lifetime = if expires_in > EXPIRY_SKEW_LEEWAY_SECONDS * 2 {
        expires_in - EXPIRY_SKEW_LEEWAY_SECONDS
    } else {
        expires_in
    };
    let token_type = str_field(&obj, "token_type").unwrap_or_else(|| "Bearer".to_string());
    if !token_type.eq_ignore_ascii_case("bearer") {
        return Err(TokenFlowError::InvalidResponse(format!(
            "unsupported token_type {token_type:?} (expected Bearer)"
        )));
    }
    Ok(AcquiredToken {
        access_token,
        refresh_token: str_field(&obj, "refresh_token"),
        access_token_expires_at: now + Duration::seconds(effective_lifetime),
        token_type,
        scope: str_field(&obj, "scope"),
        id_token: str_field(&obj, "id_token"),
    })
}

/// Maps an OAuth error body to a specific [`TokenFlowError`] variant.
pub(crate) fn map_oauth_error(body: &str) -> TokenFlowError {
    let obj = match json_object(body) {
        Ok(obj) => obj,
        Err(e) => return e,
    };
    let code = str_field(&obj, "error").unwrap_or_else(|| "unknown_error".to_string());
    let description = str_field(&obj, "error_description").unwrap_or_default();
    match code.as_str() {
        "authorization_pending" => TokenFlowError::AuthorizationPending,
        "slow_down" => TokenFlowError::SlowDown,
        "expired_token" | "code_expired" => TokenFlowError::Expired,
        "consent_required" | "interaction_required" => {
            TokenFlowError::ConsentRequired(if description.is_empty() {
                code
            } else {
                description
            })
        }
        "access_denied" | "insufficient_claims" | "invalid_grant"
            if description.to_ascii_lowercase().contains("permission")
                || description.to_ascii_lowercase().contains("consent") =>
        {
            TokenFlowError::InsufficientPermissions(description)
        }
        "access_denied" | "insufficient_claims" => {
            TokenFlowError::InsufficientPermissions(if description.is_empty() {
                code
            } else {
                description
            })
        }
        _ => TokenFlowError::OAuth { code, description },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// A scripted transport that returns queued responses in order, recording
    /// the URLs and forms it was called with.
    type RecordedForm = Vec<(String, String)>;

    #[derive(Clone)]
    struct ScriptedTransport {
        responses: Arc<Mutex<Vec<Result<HttpResponse, TokenFlowError>>>>,
        calls: Arc<Mutex<Vec<String>>>,
        forms: Arc<Mutex<Vec<RecordedForm>>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<Result<HttpResponse, TokenFlowError>>) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses)),
                calls: Arc::new(Mutex::new(Vec::new())),
                forms: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<HttpResponse, TokenFlowError> {
            self.calls.lock().unwrap().push(url.to_string());
            self.forms.lock().unwrap().push(
                form.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            );
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(TokenFlowError::Transport(
                    "no scripted response".to_string(),
                ));
            }
            responses.remove(0)
        }
    }

    fn client(transport: ScriptedTransport) -> EntraTokenClient<ScriptedTransport> {
        EntraTokenClient::new(
            transport,
            "tenant",
            "client",
            crate::security::provider::AZURE_SQL_SCOPE,
        )
    }

    #[tokio::test]
    async fn device_authorization_parses_codes() {
        let transport = ScriptedTransport::new(vec![Ok(HttpResponse::new(
            200,
            r#"{"device_code":"DEV","user_code":"ABC-123","verification_uri":"https://microsoft.com/devicelogin","interval":5,"expires_in":900,"message":"go"}"#,
        ))]);
        let auth = client(transport)
            .start_device_authorization()
            .await
            .unwrap();
        assert_eq!(auth.device_code, "DEV");
        assert_eq!(auth.user_code, "ABC-123");
        assert_eq!(auth.interval_seconds, 5);
    }

    #[tokio::test]
    async fn device_poll_returns_pending_then_token() {
        let now = Utc::now();
        let transport = ScriptedTransport::new(vec![
            Ok(HttpResponse::new(
                400,
                r#"{"error":"authorization_pending"}"#,
            )),
            Ok(HttpResponse::new(
                200,
                r#"{"access_token":"AT","refresh_token":"RT","expires_in":3600,"token_type":"Bearer","scope":"s"}"#,
            )),
        ]);
        let c = client(transport);
        let first = c.poll_device_token("DEV", now).await;
        assert_eq!(first, Err(TokenFlowError::AuthorizationPending));
        let token = c.poll_device_token("DEV", now).await.unwrap();
        assert_eq!(token.access_token, "AT");
        assert_eq!(token.refresh_token.as_deref(), Some("RT"));
        assert_eq!(
            token.access_token_expires_at,
            now + Duration::seconds(3600 - EXPIRY_SKEW_LEEWAY_SECONDS)
        );
    }

    #[tokio::test]
    async fn device_poll_maps_slow_down_and_expired() {
        let now = Utc::now();
        let c = client(ScriptedTransport::new(vec![Ok(HttpResponse::new(
            400,
            r#"{"error":"slow_down"}"#,
        ))]));
        assert_eq!(
            c.poll_device_token("DEV", now).await,
            Err(TokenFlowError::SlowDown)
        );

        let c2 = client(ScriptedTransport::new(vec![Ok(HttpResponse::new(
            400,
            r#"{"error":"expired_token"}"#,
        ))]));
        assert_eq!(
            c2.poll_device_token("DEV", now).await,
            Err(TokenFlowError::Expired)
        );
    }

    #[tokio::test]
    async fn authorization_code_exchange_succeeds() {
        let now = Utc::now();
        let transport = ScriptedTransport::new(vec![Ok(HttpResponse::new(
            200,
            r#"{"access_token":"AT2","expires_in":1200,"token_type":"Bearer"}"#,
        ))]);
        let transport_for_assert = transport.clone();
        let c = client(transport);
        let token = c
            .exchange_authorization_code("the-code", "http://localhost", "verifier-abc", now)
            .await
            .unwrap();
        assert_eq!(token.access_token, "AT2");
        assert_eq!(token.refresh_token, None);
        assert_eq!(
            token.access_token_expires_at,
            now + Duration::seconds(1200 - EXPIRY_SKEW_LEEWAY_SECONDS)
        );
        let forms = transport_for_assert.forms.lock().unwrap();
        assert_eq!(forms.len(), 1);
        assert_eq!(
            forms[0]
                .iter()
                .find(|(k, _)| k == "code_verifier")
                .map(|(_, v)| v.as_str()),
            Some("verifier-abc")
        );
    }

    #[tokio::test]
    async fn refresh_renews_token() {
        let now = Utc::now();
        let c = client(ScriptedTransport::new(vec![Ok(HttpResponse::new(
            200,
            r#"{"access_token":"AT3","refresh_token":"RT3","expires_in":3600,"token_type":"Bearer"}"#,
        ))]));
        let token = c.refresh("old-rt", now).await.unwrap();
        assert_eq!(token.access_token, "AT3");
        assert_eq!(token.refresh_token.as_deref(), Some("RT3"));
    }

    /// Redis Entra sign-in: a Redis connection mints its token from the
    /// signed-in account's refresh token against the Redis audience, through
    /// the same token cache SQL Server uses.
    #[tokio::test]
    async fn redis_entra_scope_mints_redis_audience_token() {
        use crate::security::authorize_url::{AZURE_REDIS_SCOPE, entra_data_scope};
        use crate::security::azure::{EntraAccountId, EntraTokenCacheRef};
        use crate::security::token_cache::{
            InMemoryTokenCacheStore, TokenCacheStore, mint_access_token_for_scope,
        };
        use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

        assert_eq!(
            entra_data_scope(DatabaseType::Redis.connection_kind()),
            AZURE_REDIS_SCOPE
        );
        assert_ne!(
            entra_data_scope(ConnectionKind::Tds),
            AZURE_REDIS_SCOPE,
            "SQL Server keeps the SQL audience"
        );
        assert!(AZURE_REDIS_SCOPE.starts_with("https://redis.azure.com/.default"));

        let now = Utc::now();
        let store = InMemoryTokenCacheStore::new();
        let cref = EntraTokenCacheRef::keychain(&EntraAccountId::new("home", "tenant"));
        store
            .store(
                &cref,
                &AcquiredToken {
                    access_token: "arm-token".into(),
                    refresh_token: Some("RT-signin".into()),
                    access_token_expires_at: now + Duration::hours(1),
                    token_type: "Bearer".into(),
                    scope: None,
                    id_token: None,
                },
            )
            .await
            .unwrap();
        let transport = ScriptedTransport::new(vec![Ok(HttpResponse::new(
            200,
            r#"{"access_token":"REDIS-AT","expires_in":3600,"token_type":"Bearer"}"#,
        ))]);
        let redis_client =
            EntraTokenClient::new(transport.clone(), "tenant", "client", AZURE_REDIS_SCOPE);
        let token = mint_access_token_for_scope(&store, &cref, &redis_client, now)
            .await
            .unwrap();
        assert_eq!(token.access_token, "REDIS-AT");
        let forms = transport.forms.lock().unwrap();
        let form = &forms[0];
        assert!(form.contains(&("scope".to_string(), AZURE_REDIS_SCOPE.to_string())));
        assert!(form.contains(&("refresh_token".to_string(), "RT-signin".to_string())));
    }

    #[test]
    fn oauth_error_maps_consent_and_permissions() {
        assert_eq!(
            map_oauth_error(r#"{"error":"consent_required","error_description":"need consent"}"#),
            TokenFlowError::ConsentRequired("need consent".to_string())
        );
        assert!(matches!(
            map_oauth_error(
                r#"{"error":"access_denied","error_description":"no permission to resource"}"#
            ),
            TokenFlowError::InsufficientPermissions(_)
        ));
        assert!(matches!(
            map_oauth_error(r#"{"error":"invalid_client","error_description":"bad"}"#),
            TokenFlowError::OAuth { .. }
        ));
    }

    #[test]
    fn invalid_json_is_reported() {
        assert!(matches!(
            parse_token_response("not json", Utc::now()),
            Err(TokenFlowError::InvalidResponse(_))
        ));
    }

    #[test]
    fn error_body_with_200_status_still_detected() {
        assert_eq!(
            parse_token_response(r#"{"error":"authorization_pending"}"#, Utc::now()),
            Err(TokenFlowError::AuthorizationPending)
        );
    }

    #[test]
    fn non_positive_expires_in_is_rejected() {
        for bad in ["0", "-1", "-9999999"] {
            let body = format!(r#"{{"access_token":"AT","expires_in":{bad}}}"#);
            assert!(
                matches!(
                    parse_token_response(&body, Utc::now()),
                    Err(TokenFlowError::InvalidResponse(_))
                ),
                "expires_in {bad} should be rejected"
            );
        }
    }

    #[test]
    fn huge_expires_in_is_clamped_not_a_panic() {
        let now = Utc::now();
        let body = format!(r#"{{"access_token":"AT","expires_in":{}}}"#, i64::MAX);
        let token = parse_token_response(&body, now).unwrap();
        assert_eq!(
            token.access_token_expires_at,
            now + Duration::seconds(EXPIRES_IN_MAX_SECONDS - EXPIRY_SKEW_LEEWAY_SECONDS)
        );
    }

    #[test]
    fn short_lifetime_keeps_full_duration_without_leeway() {
        let now = Utc::now();
        let token = parse_token_response(r#"{"access_token":"AT","expires_in":200}"#, now).unwrap();
        assert_eq!(token.access_token_expires_at, now + Duration::seconds(200));
    }

    #[test]
    fn non_bearer_token_type_is_rejected() {
        assert!(matches!(
            parse_token_response(
                r#"{"access_token":"AT","expires_in":3600,"token_type":"MAC"}"#,
                Utc::now()
            ),
            Err(TokenFlowError::InvalidResponse(_))
        ));
    }

    #[test]
    fn bearer_token_type_is_case_insensitive_and_defaulted() {
        let now = Utc::now();
        let lower =
            parse_token_response(r#"{"access_token":"AT","token_type":"bearer"}"#, now).unwrap();
        assert_eq!(lower.token_type, "bearer");
        let missing = parse_token_response(r#"{"access_token":"AT"}"#, now).unwrap();
        assert_eq!(missing.token_type, "Bearer");
    }

    #[test]
    fn device_authorization_expiry_is_clamped() {
        let auth = parse_device_authorization(
            r#"{"device_code":"D","user_code":"U","verification_uri":"https://x","expires_in":18446744073709551615}"#,
        )
        .unwrap();
        assert_eq!(auth.expires_in_seconds, 86_400);
        let tiny = parse_device_authorization(
            r#"{"device_code":"D","user_code":"U","verification_uri":"https://x","expires_in":0}"#,
        )
        .unwrap();
        assert_eq!(tiny.expires_in_seconds, 30);
    }

    #[test]
    fn device_poll_interval_is_clamped() {
        let auth = parse_device_authorization(
            r#"{"device_code":"D","user_code":"U","verification_uri":"https://x","interval":86400}"#,
        )
        .unwrap();
        assert_eq!(auth.interval_seconds, 300);
        let zero = parse_device_authorization(
            r#"{"device_code":"D","user_code":"U","verification_uri":"https://x","interval":0}"#,
        )
        .unwrap();
        assert_eq!(zero.interval_seconds, 1);
    }
}
