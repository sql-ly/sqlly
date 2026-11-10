//! Typed errors for the MLX sidecar provider.

/// Failures talking to the MLX sidecar.
#[derive(Debug, thiserror::Error)]
pub enum MlxError {
    /// The underlying HTTP client could not be constructed.
    #[error("failed to build MLX HTTP client: {0}")]
    Build(String),

    /// Connect/timeout/transport-level failure. Usually means the sidecar is
    /// not running.
    #[error("could not reach MLX sidecar at {url} ({reason}); is the sidecar running?")]
    Transport { url: String, reason: String },

    /// The server answered with a non-success HTTP status.
    #[error("MLX sidecar returned HTTP {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },

    /// The response body could not be decoded into the expected shape.
    #[error("could not decode MLX sidecar response from {url}: {message}")]
    Decode { url: String, message: String },

    /// The response was well-formed but empty where content was required.
    #[error("MLX sidecar returned an empty result: {0}")]
    Empty(String),

    /// The requested model does not support the requested capability.
    #[error("model '{model}' does not support capability {capability:?}")]
    UnsupportedCapability { model: String, capability: String },

    /// The requested model was not found.
    #[error("model '{model}' not found on MLX sidecar")]
    ModelNotFound { model: String },

    /// Embedding dimension mismatch.
    #[error("embedding dimension mismatch for model '{model}': expected {expected}, got {actual}")]
    DimensionMismatch {
        model: String,
        expected: usize,
        actual: usize,
    },
}

impl MlxError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::Build(detail) => DisplayMessage::new("services.ai.mlx.build", self.to_string())
                .with_arg("detail", detail),
            Self::Transport { url, reason } => {
                DisplayMessage::new("services.ai.mlx.transport", self.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", reason)
            }
            Self::Status { status, url, body } => {
                DisplayMessage::new("services.ai.mlx.status", self.to_string())
                    .with_arg("status", status.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", body)
            }
            Self::Decode { url, message } => {
                DisplayMessage::new("services.ai.mlx.decode", self.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", message)
            }
            Self::Empty(detail) => DisplayMessage::new("services.ai.mlx.empty", self.to_string())
                .with_arg("detail", detail),
            Self::UnsupportedCapability { model, capability } => {
                DisplayMessage::new("services.ai.mlx.unsupported_capability", self.to_string())
                    .with_arg("model", model)
                    .with_arg("capability", capability)
            }
            Self::ModelNotFound { model } => {
                DisplayMessage::new("services.ai.mlx.model_not_found", self.to_string())
                    .with_arg("model", model)
            }
            Self::DimensionMismatch {
                model,
                expected,
                actual,
            } => DisplayMessage::new("services.ai.mlx.dimension_mismatch", self.to_string())
                .with_arg("model", model)
                .with_arg("expected", expected.to_string())
                .with_arg("actual", actual.to_string()),
        }
    }
}
