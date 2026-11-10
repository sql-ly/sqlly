//! Typed errors for the Jev decision-model client.

use crate::catalog::DecisionError;

/// Failures talking to the Jev API.
#[derive(Debug, thiserror::Error)]
pub enum JevError {
    /// The underlying HTTP client could not be constructed.
    #[error("failed to build Jev HTTP client: {0}")]
    Build(String),

    /// Connect/timeout/transport-level failure.
    #[error("could not reach Jev at {url} ({reason})")]
    Transport { url: String, reason: String },

    /// The server answered with a non-success HTTP status.
    #[error("Jev returned HTTP {status}: {body}")]
    Status { status: u16, body: String },

    /// The response body could not be decoded into the expected shape.
    #[error("could not decode Jev response: {message}")]
    Decode { message: String },

    /// The server rate limited or shed the request (HTTP 429/529), and the
    /// one retry the client performs also failed.
    #[error("Jev rate limited the request")]
    RateLimited,
}

impl JevError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::Build(detail) => DisplayMessage::new("services.ai.jev.build", self.to_string())
                .with_arg("detail", detail),
            Self::Transport { url, reason } => {
                DisplayMessage::new("services.ai.jev.transport", self.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", reason)
            }
            Self::Status { status, body } => {
                DisplayMessage::new("services.ai.jev.status", self.to_string())
                    .with_arg("status", status.to_string())
                    .with_arg("detail", body)
            }
            Self::Decode { message } => {
                DisplayMessage::new("services.ai.jev.decode", self.to_string())
                    .with_arg("detail", message)
            }
            Self::RateLimited => {
                DisplayMessage::new("services.ai.jev.rate_limited", self.to_string())
            }
        }
    }
}

impl From<JevError> for DecisionError {
    fn from(err: JevError) -> Self {
        match err {
            JevError::Build(message) => DecisionError::Transport(message),
            JevError::Transport { url, reason } => {
                DecisionError::Transport(format!("{url}: {reason}"))
            }
            JevError::Status { status, body } => DecisionError::Status { code: status, body },
            JevError::Decode { message } => DecisionError::Decode(message),
            JevError::RateLimited => DecisionError::RateLimited,
        }
    }
}
