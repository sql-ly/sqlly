//! HTTP client for the MLX sidecar OpenAI-compatible API.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::mlx::config::MlxSidecarConfig;
use crate::mlx::error::MlxError;

/// A reusable client bound to one MLX sidecar base URL.
#[derive(Clone, Debug)]
pub struct MlxSidecarClient {
    http: reqwest::Client,
    base_url: String,
}

/// Health response from the sidecar.
#[derive(Debug, Clone, Deserialize)]
pub struct MlxSidecarHealth {
    pub status: String,
    pub provider: String,
    pub version: String,
    #[serde(default)]
    pub loaded_models: Vec<String>,
    #[serde(default)]
    pub errors: Vec<String>,
}

impl MlxSidecarHealth {
    pub fn is_healthy(&self) -> bool {
        self.status == "ok"
    }
}

/// One model advertised by the sidecar.
#[derive(Debug, Clone, Deserialize)]
pub struct MlxModelInfo {
    pub id: String,
    pub object: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub context_window: Option<usize>,
    #[serde(default)]
    pub embedding_dimensions: Option<usize>,
}

/// Models list response.
#[derive(Debug, Clone, Deserialize)]
struct ModelsResponse {
    #[serde(default)]
    data: Vec<MlxModelInfo>,
}

/// Chat completion request.
#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<usize>,
}

/// One chat message.
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

/// Chat completion response.
#[derive(Debug, Clone, Deserialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub model: String,
    #[serde(default)]
    pub choices: Vec<ChatChoice>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatChoice {
    pub index: usize,
    pub message: ChatMessageResponse,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessageResponse {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageInfo {
    #[serde(default)]
    pub prompt_tokens: usize,
    #[serde(default)]
    pub completion_tokens: usize,
    #[serde(default)]
    pub total_tokens: usize,
}

/// Embedding request.
#[derive(Debug, Clone, Serialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: Vec<String>,
}

/// Embedding response.
#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingResponse {
    pub object: String,
    pub model: String,
    #[serde(default)]
    pub data: Vec<EmbeddingData>,
    #[serde(default)]
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbeddingData {
    pub object: String,
    pub index: usize,
    pub embedding: Vec<f32>,
}

impl MlxSidecarClient {
    /// Construct a client from config.
    pub fn new(config: &MlxSidecarConfig) -> Result<Self, MlxError> {
        ensure_rustls_provider();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .connect_timeout(config.connect_timeout)
            .build()
            .map_err(|err| MlxError::Build(err.to_string()))?;
        Ok(Self {
            http,
            base_url: normalize_base_url(&config.base_url),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// `GET /health` — lightweight liveness probe.
    pub async fn health(&self) -> Result<MlxSidecarHealth, MlxError> {
        self.get_json("/health").await
    }

    /// `GET /v1/models` — list available models.
    pub async fn models(&self) -> Result<Vec<MlxModelInfo>, MlxError> {
        let resp: ModelsResponse = self.get_json("/v1/models").await?;
        Ok(resp.data)
    }

    /// `POST /v1/chat/completions`.
    pub async fn chat_completion(
        &self,
        request: ChatCompletionRequest,
    ) -> Result<ChatCompletionResponse, MlxError> {
        self.post_json("/v1/chat/completions", &request).await
    }

    /// `POST /v1/embeddings`.
    pub async fn embed_batch(
        &self,
        model: &str,
        inputs: Vec<String>,
    ) -> Result<EmbeddingResponse, MlxError> {
        let request = EmbeddingRequest {
            model: model.to_string(),
            input: inputs,
        };
        self.post_json("/v1/embeddings", &request).await
    }

    async fn get_json<R: DeserializeOwned>(&self, path: &str) -> Result<R, MlxError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .map_err(|err| MlxError::Transport {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let resp = ensure_success(resp, &url).await?;
        decode(resp, &url).await
    }

    async fn post_json<B: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<R, MlxError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .http
            .post(&url)
            .json(body)
            .send()
            .await
            .map_err(|err| MlxError::Transport {
                url: url.clone(),
                reason: err.to_string(),
            })?;
        let resp = ensure_success(resp, &url).await?;
        decode(resp, &url).await
    }
}

fn normalize_base_url(base: &str) -> String {
    base.trim_end_matches('/').to_string()
}

async fn ensure_success(resp: reqwest::Response, url: &str) -> Result<reqwest::Response, MlxError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(MlxError::Status {
        status: status.as_u16(),
        url: url.to_string(),
        body: truncate(&body, 500),
    })
}

async fn decode<R: DeserializeOwned>(resp: reqwest::Response, url: &str) -> Result<R, MlxError> {
    resp.json::<R>().await.map_err(|err| MlxError::Decode {
        url: url.to_string(),
        message: err.to_string(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Install the process-default rustls crypto provider (ring — the only
/// provider compiled into this workspace) before any TLS client is built.
/// reqwest's `rustls-no-provider` feature requires an installed default;
/// calling this more than once is harmless.
fn ensure_rustls_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn mock_server(response: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await.unwrap();
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        url
    }

    #[tokio::test]
    async fn health_parses_ok_response() {
        let body = r#"{"status":"ok","provider":"mlx","version":"0.1.0","loaded_models":["m1"],"errors":[]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let url = mock_server(Box::leak(resp.into_boxed_str())).await;
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: url,
            ..Default::default()
        })
        .unwrap();
        let health = client.health().await.unwrap();
        assert!(health.is_healthy());
        assert_eq!(health.version, "0.1.0");
        assert_eq!(health.loaded_models, vec!["m1"]);
    }

    #[tokio::test]
    async fn models_parses_response() {
        let body = r#"{"data":[{"id":"m1","object":"model","provider":"mlx","capabilities":["chat"],"context_window":32768}]}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let url = mock_server(Box::leak(resp.into_boxed_str())).await;
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: url,
            ..Default::default()
        })
        .unwrap();
        let models = client.models().await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "m1");
        assert_eq!(models[0].capabilities, vec!["chat"]);
    }

    #[tokio::test]
    async fn chat_completion_parses_response() {
        let body = r#"{"id":"c1","object":"chat.completion","model":"m1","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let url = mock_server(Box::leak(resp.into_boxed_str())).await;
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: url,
            ..Default::default()
        })
        .unwrap();
        let result = client
            .chat_completion(ChatCompletionRequest {
                model: "m1".to_string(),
                messages: vec![ChatMessage {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                }],
                temperature: None,
                max_tokens: None,
            })
            .await
            .unwrap();
        assert_eq!(result.choices[0].message.content, "hello");
    }

    #[tokio::test]
    async fn embed_batch_parses_response() {
        let body = r#"{"object":"list","model":"m1","data":[{"object":"embedding","index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":1,"total_tokens":1}}"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n{}",
            body.len(),
            body
        );
        let url = mock_server(Box::leak(resp.into_boxed_str())).await;
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: url,
            ..Default::default()
        })
        .unwrap();
        let result = client
            .embed_batch("m1", vec!["test".to_string()])
            .await
            .unwrap();
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].embedding, vec![0.1, 0.2]);
    }

    #[tokio::test]
    async fn connection_refused_returns_transport_error() {
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: "http://127.0.0.1:1".to_string(),
            ..Default::default()
        })
        .unwrap();
        let err = client.health().await.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("could not reach"),
            "expected transport error but got: {err_str}"
        );
    }

    #[tokio::test]
    async fn non_200_returns_status_error() {
        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
        let url = mock_server(Box::leak(resp.to_string().into_boxed_str())).await;
        let client = MlxSidecarClient::new(&MlxSidecarConfig {
            base_url: url,
            ..Default::default()
        })
        .unwrap();
        let err = client.health().await.unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("404"),
            "expected status error but got: {err_str}"
        );
    }
}
