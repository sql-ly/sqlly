use crate::cloud_auth::{CloudAuthError, CloudToken};
use std::time::Duration;

pub const SQLSERVICE_LOGIN_SCOPE: &str = "https://www.googleapis.com/auth/sqlservice.login";

pub async fn mint_cloud_sql_token() -> Result<CloudToken, CloudAuthError> {
    let provider = gcp_auth::provider()
        .await
        .map_err(|_| CloudAuthError::GcpNoCredentials)?;
    let token = provider
        .token(&[SQLSERVICE_LOGIN_SCOPE])
        .await
        .map_err(|error| CloudAuthError::GcpToken(error.to_string()))?;
    let expires_at =
        std::time::UNIX_EPOCH + Duration::from_secs(token.expires_at().timestamp().max(0) as u64);
    Ok(CloudToken {
        value: token.as_str().to_string(),
        expires_at,
    })
}

pub async fn adc_status() -> Result<String, CloudAuthError> {
    match gcp_auth::provider().await {
        Ok(_) => Ok("Application Default Credentials found".to_string()),
        Err(_) => Err(CloudAuthError::GcpNoCredentials),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn sqlservice_scope_constant() {
        assert_eq!(
            SQLSERVICE_LOGIN_SCOPE,
            "https://www.googleapis.com/auth/sqlservice.login"
        );
    }

    #[test]
    fn is_fresh_respects_skew() {
        let token = CloudToken {
            value: "x".to_string(),
            expires_at: std::time::SystemTime::now() + Duration::from_secs(120),
        };
        assert!(token.is_fresh(Duration::from_secs(60)));
        assert!(!token.is_fresh(Duration::from_secs(180)));
    }
}
