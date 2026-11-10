use async_trait::async_trait;
use sqlly_core::StudioResult;

#[derive(Debug, Clone)]
pub enum SecretRef {
    Raw(String),
    Keychain { service: String, account: String },
    EnvVar(String),
}

impl SecretRef {
    pub fn expose(&self) -> Option<String> {
        match self {
            SecretRef::Raw(s) => Some(s.clone()),
            SecretRef::EnvVar(name) => std::env::var(name).ok(),
            SecretRef::Keychain { .. } => None,
        }
    }
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    async fn store(&self, key: &str, value: &str) -> StudioResult<()>;
    async fn retrieve(&self, key: &str) -> StudioResult<Option<String>>;
    async fn delete(&self, key: &str) -> StudioResult<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_secret_can_be_exposed() {
        assert_eq!(
            SecretRef::Raw("secret".to_string()).expose().as_deref(),
            Some("secret")
        );
    }

    #[test]
    fn keychain_secret_is_not_exposed() {
        let secret = SecretRef::Keychain {
            service: "sqlly".to_string(),
            account: "local".to_string(),
        };
        assert_eq!(secret.expose(), None);
    }
}
