//! Browser (wasm) variant of [`crate::azure_http`]. The browser preview has
//! no outbound HTTP client for the Azure/Entra flows (they need loopback
//! redirects and cross-origin token endpoints), so every request fails with
//! an explicit notice pointing at the desktop app. The type keeps the same
//! name and surface so the rest of the app wires up identically.

use sqlly_services::security::token_flow::{HttpResponse, HttpTransport, TokenFlowError};

fn notice() -> String {
    crate::localization::text("connections.azure.browser_signin_discovery_unavailable")
}

/// Stand-in transport for the browser build. Construction always succeeds;
/// every request returns an explicit "not available in the browser" error.
#[derive(Clone, Debug)]
pub struct ReqwestHttpTransport;

impl ReqwestHttpTransport {
    /// Mirror of the native `build_default()`; never fails in the browser.
    pub fn build_default() -> Result<Self, TokenFlowError> {
        Ok(Self)
    }

    /// Public-IP lookup is part of the Azure firewall flow, which is
    /// desktop-only.
    pub async fn lookup_public_ip(&self) -> Result<String, String> {
        Err(notice())
    }
}

#[async_trait::async_trait]
impl HttpTransport for ReqwestHttpTransport {
    async fn post_form(
        &self,
        _url: &str,
        _form: &[(&str, &str)],
    ) -> Result<HttpResponse, TokenFlowError> {
        Err(TokenFlowError::Transport(notice()))
    }

    async fn get_with_bearer(
        &self,
        _url: &str,
        _access_token: &str,
    ) -> Result<HttpResponse, TokenFlowError> {
        Err(TokenFlowError::Transport(notice()))
    }

    async fn put_json_with_bearer(
        &self,
        _url: &str,
        _access_token: &str,
        _body_json: &str,
    ) -> Result<HttpResponse, TokenFlowError> {
        Err(TokenFlowError::Transport(notice()))
    }
}
