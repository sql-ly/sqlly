//! Browser-based Entra ID sign-in orchestration.
//!
//! Composes the [`super::authorize_url`] authorize URL with the
//! [`super::loopback_redirect`] capture server, drives the platform browser
//! to open that URL, awaits the redirect, exchanges the authorization code
//! for a token, decodes the response `id_token` into identity claims, and
//! returns the combined result.
//!
//! The browser-opener is a user-supplied closure so tests can substitute a
//! fake that just records the URL — production wires it to `gpui::App`'s
//! `cx.open_url` (or the platform `open` command on Linux/Win).
//!
//! Output: [`BrowserSigninOutcome`] bundles the [`AcquiredToken`] with the
//! [`IdTokenClaims`] identity so the caller can build an
//! [`super::azure::EntraAccountProfile`] without making another decode pass.

use std::time::Duration;
use thiserror::Error;

use super::authorize_url::{
    AZURE_BROWSER_SCOPE, COMMON_TENANT_ID, LOOPBACK_REDIRECT_PATH, build_authorize_url,
    code_challenge_s256, generate_code_verifier, generate_state, loopback_redirect_uri,
};
use super::azure::IdTokenClaims;
use super::loopback_redirect::{LoopbackCallback, LoopbackError, LoopbackPages, LoopbackServer};
use crate::security::token_flow::{AcquiredToken, EntraTokenClient, HttpTransport, TokenFlowError};

#[derive(Debug, Error)]
pub enum BrowserSigninError {
    #[error("loopback redirect listener failed: {0}")]
    Loopback(#[from] LoopbackError),
    #[error("token exchange failed: {0}")]
    Exchange(#[from] TokenFlowError),
    #[error("OAuth callback returned an error: {error}")]
    ProviderError {
        error: String,
        description: Option<String>,
    },
    /// The captured callback's `state` did not match the state this
    /// orchestration generated. Indicates either a CSRF attempt or a stale
    /// callback from a previous sign-in; both should be treated as an attack
    /// vector and abort the flow.
    #[error("state mismatch in OAuth callback (possible CSRF or stale redirect)")]
    StateMismatch {
        expected: String,
        actual: Option<String>,
    },
    #[error("sign-in returned no id_token, so identity claims could not be derived")]
    MissingIdToken,
}

impl BrowserSigninError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::Loopback(error) => DisplayMessage::new(
                "services.security.browser_signin.errors.loopback",
                format!("loopback redirect listener failed: {error}"),
            )
            .with_arg("detail", error.to_string()),
            Self::Exchange(error) => DisplayMessage::new(
                "services.security.browser_signin.errors.exchange",
                format!("token exchange failed: {error}"),
            )
            .with_arg("detail", error.to_string()),
            Self::ProviderError { error, .. } => DisplayMessage::new(
                "services.security.browser_signin.errors.provider",
                format!("OAuth callback returned an error: {error}"),
            )
            .with_arg("provider_error", error),
            Self::StateMismatch { .. } => DisplayMessage::new(
                "services.security.browser_signin.errors.state_mismatch",
                "state mismatch in OAuth callback (possible CSRF or stale redirect)",
            ),
            Self::MissingIdToken => DisplayMessage::new(
                "services.security.browser_signin.errors.missing_id_token",
                "sign-in returned no id_token, so identity claims could not be derived",
            ),
        }
    }
}

pub struct BrowserSigninOutcome {
    pub access_token: AcquiredToken,
    pub identity: IdTokenClaims,
}

/// Runs the full browser-sign-in orchestration end-to-end. Tests substitute a
/// fake browser opener and a scripted [`HttpTransport`]; production wires
/// `cx.open_url` and the `reqwest`-backed transport.
///
/// Requires `H: Clone` because [`EntraTokenClient`] takes ownership of the
/// transport; the lightweight scripted transport used in tests is `Clone`
/// and the `reqwest` transport wraps a `reqwest::Client` (also `Clone`).
#[allow(clippy::too_many_arguments)]
pub async fn complete_browser_signin<H: HttpTransport + Clone>(
    transport: &H,
    client_id: &str,
    tenant_id: Option<&str>,
    open_browser: impl FnOnce(&str) + Send,
    idle_timeout: Duration,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<BrowserSigninOutcome, BrowserSigninError> {
    complete_browser_signin_with_pages(
        transport,
        client_id,
        tenant_id,
        open_browser,
        idle_timeout,
        now,
        LoopbackPages::english(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn complete_browser_signin_with_pages<H: HttpTransport + Clone>(
    transport: &H,
    client_id: &str,
    tenant_id: Option<&str>,
    open_browser: impl FnOnce(&str) + Send,
    idle_timeout: Duration,
    now: chrono::DateTime<chrono::Utc>,
    pages: LoopbackPages,
) -> Result<BrowserSigninOutcome, BrowserSigninError> {
    let tenant = tenant_id.unwrap_or(COMMON_TENANT_ID);
    let state = generate_state();
    let verifier = generate_code_verifier();
    let challenge = code_challenge_s256(&verifier);
    let server =
        LoopbackServer::bind_with_pages(LOOPBACK_REDIRECT_PATH, state.clone(), idle_timeout, pages)
            .await?;
    let redirect_uri = loopback_redirect_uri(server.port());
    let authorize_url = build_authorize_url(
        tenant,
        client_id,
        &redirect_uri,
        AZURE_BROWSER_SCOPE,
        &state,
        &challenge,
    );
    open_browser(&authorize_url);
    let callback = server.wait_for_callback().await?;
    let code = match callback {
        LoopbackCallback::Code {
            code,
            state: callback_state,
        } => {
            if callback_state.as_deref() != Some(state.as_str()) {
                return Err(BrowserSigninError::StateMismatch {
                    expected: state,
                    actual: callback_state,
                });
            }
            code
        }
        LoopbackCallback::Error { error, description } => {
            return Err(BrowserSigninError::ProviderError { error, description });
        }
    };
    let token = EntraTokenClient::new(transport.clone(), tenant, client_id, AZURE_BROWSER_SCOPE)
        .exchange_authorization_code(&code, &redirect_uri, &verifier, now)
        .await?;
    let id_token = match token.id_token.clone() {
        Some(jwt) => jwt,
        None => return Err(BrowserSigninError::MissingIdToken),
    };
    let identity =
        IdTokenClaims::decode(&id_token).ok_or_else(|| BrowserSigninError::ProviderError {
            error: "invalid_id_token".into(),
            description: Some("id_token was not a valid JWT".into()),
        })?;
    Ok(BrowserSigninOutcome {
        access_token: token,
        identity,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::authorize_url::AZURE_PUBLIC_CLIENT_ID;
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A scripted transport shared by the orchestrator test. Clones share
    /// both the call log and the response queue so the cloned instance used
    /// by `EntraTokenClient::new` also sees the next response — the queue
    /// is consumed in the order it was queued.
    type RecordedForm = Vec<(String, String)>;

    #[derive(Clone)]
    struct ScriptedTransport {
        calls: Arc<StdMutex<Vec<String>>>,
        forms: Arc<StdMutex<Vec<RecordedForm>>>,
        responses: Arc<StdMutex<Vec<crate::security::token_flow::HttpResponse>>>,
    }

    impl ScriptedTransport {
        fn new(responses: Vec<crate::security::token_flow::HttpResponse>) -> Self {
            Self {
                calls: Arc::new(StdMutex::new(Vec::new())),
                forms: Arc::new(StdMutex::new(Vec::new())),
                responses: Arc::new(StdMutex::new(responses)),
            }
        }
    }

    #[async_trait::async_trait]
    impl HttpTransport for ScriptedTransport {
        async fn post_form(
            &self,
            url: &str,
            form: &[(&str, &str)],
        ) -> Result<crate::security::token_flow::HttpResponse, TokenFlowError> {
            self.calls.lock().unwrap().push(url.to_string());
            self.forms.lock().unwrap().push(
                form.iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
            );
            let mut queue = self.responses.lock().unwrap();
            if queue.is_empty() {
                return Err(TokenFlowError::Transport("no scripted response".into()));
            }
            Ok(queue.remove(0))
        }
    }

    fn make_test_id_token() -> String {
        use base64::Engine;
        let payload = json!({
            "tid": "tenant-123",
            "oid": "object-123",
            "preferred_username": "alex@contoso.com",
            "name": "Alex"
        })
        .to_string();
        let header_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"RS256\"}");
        let payload_b64 =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header_b64}.{payload_b64}.{sig}")
    }

    #[tokio::test]
    async fn orchestration_opens_browser_with_loopback_redirect_uri() {
        // The simpler test: verify the URL passed to open_browser is
        // well-formed and contains the loopback redirect port + state nonce.
        let transport =
            ScriptedTransport::new(vec![crate::security::token_flow::HttpResponse::new(
                200,
                json!({
                    "access_token": "AT",
                    "expires_in": 3600,
                    "token_type": "Bearer",
                    "id_token": make_test_id_token(),
                })
                .to_string(),
            )]);
        let captured_url = Arc::new(StdMutex::new(None::<String>));
        let captured_url_for_closure = Arc::clone(&captured_url);
        let transport_for_orchestrator = transport.clone();
        // Spawn the orchestrator. It will call open_browser once the
        // listener is bound; we then issue the redirect in parallel to
        // complete the flow and avoid the timer firing.
        let orch = tokio::spawn(async move {
            complete_browser_signin(
                &transport_for_orchestrator,
                AZURE_PUBLIC_CLIENT_ID,
                None,
                move |url| {
                    *captured_url_for_closure.lock().unwrap() = Some(url.to_string());
                },
                Duration::from_secs(2),
                chrono::Utc::now(),
            )
            .await
        });
        // Wait briefly for the loopback listener to bind and the URL to be
        // captured.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let url = captured_url
            .lock()
            .unwrap()
            .clone()
            .expect("open_browser called");
        assert!(url.starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/authorize?"));
        assert!(url.contains(&format!("client_id={AZURE_PUBLIC_CLIENT_ID}")));
        assert!(url.contains("response_type=code"));
        assert!(url.contains("scope="));
        assert!(url.contains("prompt=select_account"));
        assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A"));
        assert!(url.contains("/auth"));
        assert!(url.contains("state="));
        assert!(url.contains("code_challenge="));
        assert!(url.contains("code_challenge_method=S256"));
        // The orchestrator is still awaiting the callback; we let it
        // time out to avoid leaving an orphaned accept task.
        let _ = tokio::time::timeout(Duration::from_secs(3), orch).await;
    }

    #[test]
    fn state_round_trips_through_authorize_url_format() {
        // Defensive: if the URL ever drops the state param, the CSRF
        // defense is broken. Catches accidental refactors of
        // `build_authorize_url`.
        let s = generate_state();
        let url = build_authorize_url(
            COMMON_TENANT_ID,
            "id",
            "http://127.0.0.1:1/auth",
            "scope",
            &s,
            "challenge",
        );
        assert!(url.contains(&format!("state={s}")));
    }

    #[tokio::test]
    async fn full_orchestration_returns_token_and_claims() {
        // Drives the entire pipeline end to end: a single token-endpoint
        // response is queued; the orchestrator spawns the loopback listener,
        // opens a fake browser which POSTs the redirect, then awaits the
        // token exchange and decodes the id_token claims.
        let transport =
            ScriptedTransport::new(vec![crate::security::token_flow::HttpResponse::new(
                200,
                json!({
                    "access_token": "AT-123",
                    "expires_in": 3600,
                    "token_type": "Bearer",
                    "id_token": make_test_id_token(),
                })
                .to_string(),
            )]);
        let transport_for_orch = transport.clone();
        let (captured_url_tx, captured_url_rx) = tokio::sync::oneshot::channel();
        let orchestrator = tokio::spawn(async move {
            complete_browser_signin(
                &transport_for_orch,
                AZURE_PUBLIC_CLIENT_ID,
                None,
                |url| {
                    let _ = captured_url_tx.send(url.to_string());
                },
                Duration::from_secs(3),
                chrono::Utc::now(),
            )
            .await
        });
        // Await the URL the orchestrator passed to open_browser; from it we
        // extract the redirect port + state nonce, then replay the
        // browser's redirect to the loopback server.
        let url = tokio::time::timeout(Duration::from_secs(2), captured_url_rx)
            .await
            .expect("orchestrator called open_browser in time")
            .expect("channel send");
        let redirect_uri = extract_param(&url, "redirect_uri").expect("redirect_uri");
        let state = extract_param(&url, "state").expect("state");
        let code_challenge = extract_param(&url, "code_challenge").expect("code_challenge");
        let port = loopback_port_from_url(&redirect_uri);
        let browser_request = format!(
            "GET /auth?code=AUTH-CODE&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
        );
        let response_task = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            stream.write_all(browser_request.as_bytes()).await.unwrap();
            let mut response = String::new();
            tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
                .await
                .unwrap();
            response
        });
        let outcome = orchestrator
            .await
            .unwrap()
            .expect("orchestration succeeded");
        let response = response_task.await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(outcome.access_token.access_token == "AT-123");
        assert!(outcome.access_token.id_token.is_some());
        assert_eq!(outcome.identity.tid, "tenant-123");
        assert_eq!(outcome.identity.oid, "object-123");
        assert_eq!(outcome.identity.username(), "alex@contoso.com");
        assert_eq!(outcome.identity.name.as_deref(), Some("Alex"));
        // The transport saw exactly one POST — to the token endpoint.
        let calls = transport.calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].ends_with("/oauth2/v2.0/token"));
        let forms = transport.forms.lock().unwrap();
        assert_eq!(forms.len(), 1);
        let posted_verifier = forms[0]
            .iter()
            .find(|(k, _)| k == "code_verifier")
            .map(|(_, v)| v.as_str())
            .expect("token exchange must include code_verifier");
        assert_eq!(
            crate::security::authorize_url::code_challenge_s256(posted_verifier),
            code_challenge
        );
    }

    fn extract_param(url: &str, name: &str) -> Option<String> {
        let query = url.split_once('?')?.1;
        query.split('&').find_map(|pair| {
            let (k, v) = pair.split_once('=')?;
            if k == name { percent_decode(v) } else { None }
        })
    }

    fn percent_decode(s: &str) -> Option<String> {
        let bytes = s.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' && i + 2 < bytes.len() {
                let hi = hex_test(bytes[i + 1])?;
                let lo = hex_test(bytes[i + 2])?;
                out.push(hi * 16 + lo);
                i += 3;
                continue;
            }
            out.push(bytes[i]);
            i += 1;
        }
        Some(String::from_utf8_lossy(&out).into_owned())
    }

    fn hex_test(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            b'A'..=b'F' => Some(b - b'A' + 10),
            _ => None,
        }
    }

    fn loopback_port_from_url(url: &str) -> u16 {
        // Expect `http://127.0.0.1:NNNN/auth`.
        let after_scheme = url.strip_prefix("http://").unwrap();
        let host_port = after_scheme.split('/').next().unwrap();
        let port = host_port.split(':').nth(1).unwrap();
        port.parse().unwrap()
    }

    #[tokio::test]
    async fn state_mismatch_returns_security_error_without_calling_token_endpoint() {
        let transport = ScriptedTransport::new(Vec::new());
        let transport_for_orch = transport.clone();
        let (captured_url_tx, captured_url_rx) = tokio::sync::oneshot::channel();
        let orchestrator = tokio::spawn(async move {
            complete_browser_signin(
                &transport_for_orch,
                AZURE_PUBLIC_CLIENT_ID,
                None,
                |url| {
                    let _ = captured_url_tx.send(url.to_string());
                },
                Duration::from_secs(3),
                chrono::Utc::now(),
            )
            .await
        });
        let url = tokio::time::timeout(Duration::from_secs(2), captured_url_rx)
            .await
            .expect("url delivered")
            .expect("channel send");
        let redirect_uri = extract_param(&url, "redirect_uri").unwrap();
        let port = loopback_port_from_url(&redirect_uri);
        // Send a callback with a wrong state — must trigger StateMismatch.
        let browser_request = "GET /auth?code=AUTH-CODE&state=NOT-THE-RIGHT-STATE HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n".to_string();
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            let _ = stream.write_all(browser_request.as_bytes()).await;
            let mut response = String::new();
            let _ = tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response).await;
        });
        let result = orchestrator.await.unwrap();
        assert!(matches!(
            result,
            Err(BrowserSigninError::StateMismatch { .. })
        ));
        // No token endpoint call should have happened — the security path
        // refuses before exchange.
        let calls = transport.calls.lock().unwrap().clone();
        assert!(
            calls.is_empty(),
            "expected no transport calls, got {calls:?}"
        );
    }
}
