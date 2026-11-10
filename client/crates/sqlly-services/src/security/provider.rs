use crate::security::auth::{AuthCredentials, AuthProvider, AuthResult, AuthType};
use async_trait::async_trait;
use sqlly_core::{StudioError, StudioResult};

pub struct SqlAuthProvider;

pub const AZURE_SQL_SCOPE: &str = "https://database.windows.net//.default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntraAuthFlowKind {
    Browser,
    DeviceCode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntraAuthConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub redirect_uri: String,
    pub scope: String,
}

impl EntraAuthConfig {
    pub fn azure_sql(tenant_id: impl Into<String>, client_id: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            client_id: client_id.into(),
            redirect_uri: "http://localhost".to_string(),
            scope: AZURE_SQL_SCOPE.to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserAuthRequest {
    pub authorization_url: String,
    pub redirect_uri: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCodeChallenge {
    pub verification_uri: String,
    pub user_code: String,
    pub message: String,
    pub interval_seconds: u64,
    pub expires_in_seconds: u64,
}

#[derive(Debug, Clone)]
pub struct EntraIdAuthProvider {
    config: EntraAuthConfig,
    flow: EntraAuthFlowKind,
}

impl EntraIdAuthProvider {
    pub fn browser(config: EntraAuthConfig) -> Self {
        Self {
            config,
            flow: EntraAuthFlowKind::Browser,
        }
    }

    pub fn device_code(config: EntraAuthConfig) -> Self {
        Self {
            config,
            flow: EntraAuthFlowKind::DeviceCode,
        }
    }

    pub fn flow(&self) -> EntraAuthFlowKind {
        self.flow
    }

    pub fn browser_request(&self, state: &str) -> BrowserAuthRequest {
        BrowserAuthRequest {
            authorization_url: format!(
                "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize?client_id={}&response_type=code&redirect_uri={}&scope={}&state={}",
                encode_component(&self.config.tenant_id),
                encode_component(&self.config.client_id),
                encode_component(&self.config.redirect_uri),
                encode_component(&self.config.scope),
                encode_component(state)
            ),
            redirect_uri: self.config.redirect_uri.clone(),
        }
    }

    pub fn device_code_challenge(&self, user_code: impl Into<String>) -> DeviceCodeChallenge {
        let user_code = user_code.into();
        DeviceCodeChallenge {
            verification_uri: format!(
                "https://microsoft.com/devicelogin?tenant={}",
                encode_component(&self.config.tenant_id)
            ),
            message: format!(
                "Open https://microsoft.com/devicelogin and enter code {user_code} to authenticate Azure SQL."
            ),
            user_code,
            interval_seconds: 5,
            expires_in_seconds: 900,
        }
    }
}

#[async_trait]
impl AuthProvider for SqlAuthProvider {
    fn auth_type(&self) -> AuthType {
        AuthType::SqlAuth
    }

    async fn authenticate(&self, credentials: &AuthCredentials) -> StudioResult<AuthResult> {
        let username = credentials.username.clone().ok_or_else(|| {
            StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
                "services.security.sql_auth.username_required",
                "Username required for SQL auth",
            ))
        })?;
        let password = credentials.password.as_deref().ok_or_else(|| {
            StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
                "services.security.sql_auth.password_required",
                "Password required for SQL auth",
            ))
        })?;
        if password.is_empty() {
            return Err(StudioError::AuthenticationMessage(
                sqlly_core::DisplayMessage::new(
                    "services.security.sql_auth.password_required",
                    "Password required for SQL auth",
                ),
            ));
        }
        Ok(AuthResult {
            token: None,
            username,
            expires_at: None,
        })
    }

    fn is_available(&self) -> bool {
        true
    }
}

#[async_trait]
impl AuthProvider for EntraIdAuthProvider {
    fn auth_type(&self) -> AuthType {
        AuthType::EntraId
    }

    async fn authenticate(&self, credentials: &AuthCredentials) -> StudioResult<AuthResult> {
        let token = credentials.password.clone().ok_or_else(|| {
            StudioError::Authentication(
                "Entra ID authentication requires an access token from browser or device code flow"
                    .to_string(),
            )
        })?;
        if token.is_empty() {
            return Err(StudioError::Authentication(
                "Entra ID access token cannot be empty".to_string(),
            ));
        }
        Ok(AuthResult {
            token: Some(token),
            username: credentials
                .username
                .clone()
                .unwrap_or_else(|| "entra-id-user".to_string()),
            expires_at: None,
        })
    }

    fn is_available(&self) -> bool {
        true
    }
}

fn encode_component(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sql_auth_requires_password() {
        let result = SqlAuthProvider
            .authenticate(&AuthCredentials {
                username: Some("sa".to_string()),
                password: None,
                tenant_id: None,
            })
            .await;

        assert!(result.is_err());
    }

    #[test]
    fn entra_browser_request_targets_azure_sql_scope() {
        let provider =
            EntraIdAuthProvider::browser(EntraAuthConfig::azure_sql("common", "client id"));
        let request = provider.browser_request("state value");
        assert_eq!(provider.flow(), EntraAuthFlowKind::Browser);
        assert!(
            request
                .authorization_url
                .contains("scope=https%3A%2F%2Fdatabase.windows.net%2F%2F.default")
        );
        assert!(request.authorization_url.contains("client_id=client%20id"));
    }

    #[test]
    fn entra_device_code_builds_fallback_prompt() {
        let provider =
            EntraIdAuthProvider::device_code(EntraAuthConfig::azure_sql("tenant", "client"));
        let challenge = provider.device_code_challenge("ABC-123");
        assert_eq!(provider.flow(), EntraAuthFlowKind::DeviceCode);
        assert!(challenge.message.contains("ABC-123"));
        assert!(challenge.verification_uri.contains("tenant=tenant"));
    }

    #[tokio::test]
    async fn entra_auth_requires_flow_token() {
        let result = EntraIdAuthProvider::browser(EntraAuthConfig::azure_sql("tenant", "client"))
            .authenticate(&AuthCredentials {
                username: Some("user@example.com".to_string()),
                password: None,
                tenant_id: Some("tenant".to_string()),
            })
            .await;
        assert!(result.is_err());
    }
}
