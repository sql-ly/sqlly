use async_trait::async_trait;
use sqlly_core::StudioResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthType {
    SqlAuth,
    WindowsAuth,
    EntraId,
}

#[async_trait]
pub trait AuthProvider: Send + Sync {
    fn auth_type(&self) -> AuthType;
    async fn authenticate(&self, credentials: &AuthCredentials) -> StudioResult<AuthResult>;
    fn is_available(&self) -> bool;
}

#[derive(Debug, Clone)]
pub struct AuthCredentials {
    pub username: Option<String>,
    pub password: Option<String>,
    pub tenant_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthResult {
    pub token: Option<String>,
    pub username: String,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}
