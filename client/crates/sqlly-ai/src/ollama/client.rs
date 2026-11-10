//! HTTP client and configuration for a local [Ollama](https://ollama.com)
//! runtime.
//!
//! This is the single choke point that actually talks to Ollama over its native
//! local HTTP API. The two providers ([`crate::ollama::OllamaLlmProvider`] and
//! [`crate::ollama::OllamaEmbeddingProvider`]) are thin adapters on top of it that
//! implement the `sqlly-catalog` provider traits.
//!
//! Design notes:
//! - One `reqwest::Client` is reused per provider so we amortize connection
//!   setup across the many small calls a schema-intelligence session makes.
//! - We use the *native* Ollama endpoints (`/api/chat`, `/api/embeddings`,
//!   `/api/version`) rather than the OpenAI-compatible `/v1` flavor, because
//!   they are the most broadly supported across Ollama versions.
//! - TLS is `rustls` only (no OpenSSL/native-tls) per project policy, though
//!   the default loopback base URL is plain HTTP.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Default Ollama base URL: the loopback address the daemon listens on.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

/// Default per-request timeout. Generation and embedding-cache builds can take
/// many seconds against a local model, so this is generous.
pub const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Default timeout for the construction-time embedding dimensionality probe.
/// This is deliberately *much* shorter than [`DEFAULT_TIMEOUT_SECS`]: the probe
/// runs on the engine-startup path (it blocks `CatalogState::new`), so an
/// unreachable or wedged runtime must fail fast and fall back to the stub
/// rather than stalling the whole engine for the full request timeout.
pub const DEFAULT_PROBE_TIMEOUT_SECS: u64 = 15;

/// Environment variable overriding the Ollama base URL.
pub const BASE_URL_ENV: &str = "SQLLY_OLLAMA_BASE_URL";

/// Environment variable overriding the per-request timeout (seconds).
pub const TIMEOUT_SECS_ENV: &str = "SQLLY_OLLAMA_TIMEOUT_SECS";

/// Environment variable overriding the embedding-probe timeout (seconds).
pub const PROBE_TIMEOUT_SECS_ENV: &str = "SQLLY_OLLAMA_PROBE_TIMEOUT_SECS";

/// Connection configuration for the Ollama runtime.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Base URL of the Ollama HTTP API, e.g. `http://127.0.0.1:11434`.
    pub base_url: String,
    /// Per-request timeout for normal generation/embedding calls.
    pub timeout: Duration,
    /// Timeout for the construction-time dimensionality probe (kept short so a
    /// dead runtime does not stall engine startup).
    pub probe_timeout: Duration,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            probe_timeout: Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS),
        }
    }
}

impl OllamaConfig {
    /// Build a config from the environment, falling back to the loopback
    /// defaults. Never fails: malformed values are ignored in favor of the
    /// default so a misconfigured environment cannot wedge engine startup.
    pub fn from_env() -> Self {
        let base_url = std::env::var(BASE_URL_ENV)
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
        let timeout = duration_from_env(TIMEOUT_SECS_ENV, DEFAULT_TIMEOUT_SECS);
        let probe_timeout = duration_from_env(PROBE_TIMEOUT_SECS_ENV, DEFAULT_PROBE_TIMEOUT_SECS);
        Self {
            base_url,
            timeout,
            probe_timeout,
        }
    }
}

/// Parse a positive-seconds duration from `var`, falling back to `default_secs`.
fn duration_from_env(var: &str, default_secs: u64) -> Duration {
    std::env::var(var)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(default_secs))
}

/// Failures talking to the Ollama runtime. Messages are intentionally
/// actionable — they name the URL and (where relevant) the likely fix — because
/// they surface to the user as the reason a schema request failed.
#[derive(Debug, thiserror::Error)]
pub enum OllamaError {
    /// The underlying HTTP client could not be constructed.
    #[error("failed to build Ollama HTTP client: {0}")]
    Build(String),

    /// Connect/timeout/transport-level failure. Usually means Ollama is not
    /// running at `base_url`.
    #[error("could not reach Ollama at {url} ({reason}); is the local Ollama runtime running?")]
    Transport { url: String, reason: String },

    /// The server answered with a non-success HTTP status. A 404 here typically
    /// means the requested model is not installed.
    #[error("Ollama returned HTTP {status} for {url}: {body}")]
    Status {
        status: u16,
        url: String,
        body: String,
    },

    /// The response body could not be decoded into the expected shape.
    #[error("could not decode Ollama response from {url}: {message}")]
    Decode { url: String, message: String },

    /// The response was well-formed but empty where content was required.
    #[error("Ollama returned an empty result: {0}")]
    Empty(String),
}

impl OllamaError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::Build(detail) => {
                DisplayMessage::new("services.ai.ollama.build", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Transport { url, reason } => {
                DisplayMessage::new("services.ai.ollama.transport", self.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", reason)
            }
            Self::Status { status, url, body } => {
                DisplayMessage::new("services.ai.ollama.status", self.to_string())
                    .with_arg("status", status.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", body)
            }
            Self::Decode { url, message } => {
                DisplayMessage::new("services.ai.ollama.decode", self.to_string())
                    .with_arg("url", url)
                    .with_arg("detail", message)
            }
            Self::Empty(detail) => {
                DisplayMessage::new("services.ai.ollama.empty", self.to_string())
                    .with_arg("detail", detail)
            }
        }
    }
}

/// A reusable client bound to one Ollama base URL.
#[derive(Clone, Debug)]
pub struct OllamaClient {
    http: reqwest::Client,
    base_url: String,
}

impl OllamaClient {
    /// Construct a client from config. Does not perform any network I/O; it only
    /// builds the underlying `reqwest::Client`. Uses [`OllamaConfig::timeout`].
    pub fn new(config: &OllamaConfig) -> Result<Self, OllamaError> {
        Self::with_timeout(config, config.timeout)
    }

    /// Construct a client targeting `config.base_url` but with an explicit
    /// per-request `timeout` (used for the short-lived dimensionality probe).
    pub fn with_timeout(config: &OllamaConfig, timeout: Duration) -> Result<Self, OllamaError> {
        ensure_rustls_provider();
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|err| OllamaError::Build(err.to_string()))?;
        Ok(Self {
            http,
            base_url: normalize_base_url(&config.base_url),
        })
    }

    /// The base URL this client targets (trailing slash stripped).
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /api/version` — a lightweight liveness probe.
    pub async fn version(&self) -> Result<String, OllamaError> {
        let url = format!("{}/api/version", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| OllamaError::Transport {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let resp = ensure_success(resp, &url).await?;
        let parsed: VersionResponse = decode(resp, &url).await?;
        Ok(parsed.version)
    }

    /// `POST /api/chat` with `stream: false` — a single, non-streaming chat
    /// completion.
    pub async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, OllamaError> {
        self.post_json("/api/chat", &request).await
    }

    /// `GET /api/tags` — the models installed in the runtime, by name.
    pub async fn list_models(&self) -> Result<Vec<String>, OllamaError> {
        let url = format!("{}/api/tags", self.base_url);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| OllamaError::Transport {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let resp = ensure_success(resp, &url).await?;
        let parsed: TagsResponse = decode(resp, &url).await?;
        Ok(parsed.models.into_iter().map(|m| m.name).collect())
    }

    /// `POST /api/embed` — embed one or more inputs in a single round trip,
    /// returning one vector per input in order.
    ///
    /// This is the modern batch endpoint (`input` accepts an array and the
    /// response is `embeddings`), which lets a single request amortize many
    /// texts instead of paying a round trip per item like the legacy
    /// single-prompt `/api/embeddings`.
    pub async fn embed_batch(
        &self,
        model: &str,
        inputs: &[&str],
    ) -> Result<Vec<Vec<f32>>, OllamaError> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let body = EmbedRequest {
            model,
            input: inputs,
        };
        let resp: EmbedResponse = self.post_json("/api/embed", &body).await?;
        Ok(resp.embeddings)
    }

    /// Embed a single prompt, returning its vector. Implemented in terms of the
    /// batch endpoint so there is a single embedding code path.
    pub async fn embed(&self, model: &str, prompt: &str) -> Result<Vec<f32>, OllamaError> {
        let url = format!("{}/api/embed", self.base_url);
        let mut batch = self.embed_batch(model, &[prompt]).await?;
        if batch.is_empty() {
            return Err(OllamaError::Empty(format!(
                "embedding model '{model}' returned no vectors from {url}"
            )));
        }
        Ok(batch.swap_remove(0))
    }

    /// Shared JSON POST helper: serialize `body`, check status, decode `R`.
    async fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, OllamaError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|err| OllamaError::Transport {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let resp = ensure_success(resp, &url).await?;
        decode(resp, &url).await
    }
}

/// Strip a single trailing slash so `{base}{path}` never doubles up.
fn normalize_base_url(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

/// Turn a non-2xx response into a [`OllamaError::Status`], reading (and
/// truncating) the body for diagnostics.
async fn ensure_success(
    resp: reqwest::Response,
    url: &str,
) -> Result<reqwest::Response, OllamaError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(OllamaError::Status {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate(&body, 500),
    })
}

/// Decode a successful response body into `R`.
async fn decode<R: DeserializeOwned>(resp: reqwest::Response, url: &str) -> Result<R, OllamaError> {
    resp.json::<R>().await.map_err(|err| OllamaError::Decode {
        url: url.to_string(),
        message: err.to_string(),
    })
}

/// Truncate a string to `max` chars (for bounded error bodies).
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// A chat request body for `/api/chat`.
#[derive(Debug, Clone, Serialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    /// Always `false`: we consume a single, complete response.
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<ChatOptions>,
}

/// One chat message (role + content).
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }
}

/// Generation options passed under `options` in the chat request.
#[derive(Debug, Clone, Serialize)]
pub struct ChatOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_ctx: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub num_predict: Option<u32>,
}

/// The response shape from `/api/chat` (with `stream: false`). Counters are
/// optional because not every model/version reports them.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatResponse {
    #[serde(default)]
    pub message: ChatResponseMessage,
    #[serde(default)]
    pub prompt_eval_count: Option<u64>,
    #[serde(default)]
    pub eval_count: Option<u64>,
    #[serde(default)]
    pub total_duration: Option<u64>,
}

/// The `message` object inside a chat response.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatResponseMessage {
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a [&'a str],
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
}

#[derive(Debug, Deserialize)]
struct VersionResponse {
    #[serde(default)]
    version: String,
}

#[derive(serde::Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(serde::Deserialize)]
struct TagModel {
    name: String,
}

/// Install the process-default rustls crypto provider (ring — the only
/// provider compiled into this workspace) before any TLS client is built.
/// reqwest's `rustls-no-provider` feature requires an installed default;
/// calling this more than once is harmless. Shared with the other native
/// HTTP providers (`crate::jev`) via a re-export in `crate::ollama`.
pub(crate) fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the env-mutating tests in this module: they share the same
    /// process-global `SQLLY_OLLAMA_*` variables, so they must not run
    /// concurrently (the test harness runs tests in parallel by default).

    #[test]
    fn normalize_base_url_strips_trailing_slash() {
        assert_eq!(normalize_base_url("http://x:1/"), "http://x:1");
        assert_eq!(normalize_base_url("http://x:1"), "http://x:1");
    }

    #[test]
    fn truncate_bounds_length() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel…");
    }

    #[test]
    fn chat_options_serialize_output_token_limit() {
        let options = ChatOptions {
            temperature: None,
            num_ctx: Some(8192),
            num_predict: Some(512),
        };
        let json = serde_json::to_value(options).unwrap();
        assert_eq!(json["num_ctx"], 8192);
        assert_eq!(json["num_predict"], 512);
        assert!(json.get("temperature").is_none());
    }

    #[test]
    fn from_env_defaults_when_unset() {
        // Clear the relevant vars so this test is hermetic; the guard
        // restores the host values on drop.
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.remove(BASE_URL_ENV);
        env.remove(TIMEOUT_SECS_ENV);
        env.remove(PROBE_TIMEOUT_SECS_ENV);

        let config = OllamaConfig::from_env();
        assert_eq!(config.base_url, DEFAULT_BASE_URL);
        assert_eq!(config.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
        assert_eq!(
            config.probe_timeout,
            Duration::from_secs(DEFAULT_PROBE_TIMEOUT_SECS)
        );
    }

    #[test]
    fn from_env_reads_probe_timeout_override() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set(PROBE_TIMEOUT_SECS_ENV, "3");

        let config = OllamaConfig::from_env();
        assert_eq!(config.probe_timeout, Duration::from_secs(3));
    }
}
