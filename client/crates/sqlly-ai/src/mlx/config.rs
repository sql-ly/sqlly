//! Configuration for the MLX sidecar HTTP client.

use std::time::Duration;

/// Default MLX sidecar base URL.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:18123";

/// Default per-request timeout.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Default connect timeout.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5000;

/// Default max embedding batch size.
pub const DEFAULT_MAX_EMBEDDING_BATCH: usize = 16;

/// Environment variable for the MLX sidecar base URL.
pub const BASE_URL_ENV: &str = "SQLLY_MLX_BASE_URL";

/// Environment variable for the chat model override.
pub const CHAT_MODEL_ENV: &str = "SQLLY_MLX_CHAT_MODEL";

/// Environment variable for the embedding model override.
pub const EMBEDDING_MODEL_ENV: &str = "SQLLY_MLX_EMBEDDING_MODEL";

/// Environment variable for the per-request timeout (seconds).
pub const TIMEOUT_SECS_ENV: &str = "SQLLY_MLX_TIMEOUT_SECS";

/// Environment variable for the connect timeout (milliseconds).
pub const CONNECT_TIMEOUT_MS_ENV: &str = "SQLLY_MLX_CONNECT_TIMEOUT_MS";

/// Environment variable for the max embedding batch size.
pub const MAX_EMBEDDING_BATCH_ENV: &str = "SQLLY_MLX_EMBED_BATCH_SIZE";

/// Configuration for connecting to the MLX sidecar.
#[derive(Debug, Clone)]
pub struct MlxSidecarConfig {
    pub base_url: String,
    pub chat_model: Option<String>,
    pub embedding_model: Option<String>,
    pub timeout: Duration,
    pub connect_timeout: Duration,
    pub max_embedding_batch: usize,
}

impl Default for MlxSidecarConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            chat_model: None,
            embedding_model: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
            max_embedding_batch: DEFAULT_MAX_EMBEDDING_BATCH,
        }
    }
}

impl MlxSidecarConfig {
    /// Build a config from the environment, falling back to defaults.
    pub fn from_env() -> Self {
        let base_url = std::env::var(BASE_URL_ENV)
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let chat_model = std::env::var(CHAT_MODEL_ENV).ok().filter(|v| !v.is_empty());
        let embedding_model = std::env::var(EMBEDDING_MODEL_ENV)
            .ok()
            .filter(|v| !v.is_empty());
        let timeout = duration_from_env(TIMEOUT_SECS_ENV, DEFAULT_TIMEOUT_SECS);
        let connect_timeout =
            duration_ms_from_env(CONNECT_TIMEOUT_MS_ENV, DEFAULT_CONNECT_TIMEOUT_MS);
        let max_embedding_batch =
            usize_from_env(MAX_EMBEDDING_BATCH_ENV, DEFAULT_MAX_EMBEDDING_BATCH);
        Self {
            base_url,
            chat_model,
            embedding_model,
            timeout,
            connect_timeout,
            max_embedding_batch,
        }
    }
}

fn duration_from_env(var: &str, default_secs: u64) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

fn duration_ms_from_env(var: &str, default_ms: u64) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_millis)
        .unwrap_or_else(|| Duration::from_millis(default_ms))
}

fn usize_from_env(var: &str, default: usize) -> usize {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default)
}
