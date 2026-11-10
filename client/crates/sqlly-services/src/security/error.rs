use thiserror::Error;

pub type SecurityResult<T> = Result<T, SecurityError>;

#[derive(Debug, Error)]
pub enum SecurityError {
    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Secret store unavailable: {0}")]
    StoreUnavailable(String),

    #[error("Secret not found: {0}")]
    SecretNotFound(String),

    #[error("{0}")]
    Other(String),
}

impl SecurityError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::AuthFailed(detail) => DisplayMessage::new(
                "services.security.errors.authentication_failed",
                format!("Authentication failed: {detail}"),
            )
            .with_arg("detail", detail),
            Self::StoreUnavailable(detail) => DisplayMessage::new(
                "services.security.errors.store_unavailable",
                format!("Secret store unavailable: {detail}"),
            )
            .with_arg("detail", detail),
            Self::SecretNotFound(detail) => DisplayMessage::new(
                "services.security.errors.secret_not_found",
                format!("Secret not found: {detail}"),
            )
            .with_arg("detail", detail),
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
