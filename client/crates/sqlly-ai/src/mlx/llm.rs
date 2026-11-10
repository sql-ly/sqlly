//! [`MlxSidecarLlmProvider`]: a `sqlly-catalog` [`LlmProvider`] backed by the
//! MLX sidecar.

use crate::catalog::{
    ChatRole, CompletionRequest, CompletionResponse, LlmError, LlmProvider, ModelCapability,
};
use async_trait::async_trait;

use crate::mlx::client::{ChatCompletionRequest, ChatMessage, MlxSidecarClient};

/// An [`LlmProvider`] that answers schema questions via the MLX sidecar.
pub struct MlxSidecarLlmProvider {
    client: MlxSidecarClient,
    default_model: String,
    provider_id: String,
}

impl MlxSidecarLlmProvider {
    pub fn new(client: MlxSidecarClient, default_model: impl Into<String>) -> Self {
        let default_model = default_model.into();
        let provider_id = format!("mlx:{default_model}");
        Self {
            client,
            default_model,
            provider_id,
        }
    }

    pub fn default_model(&self) -> &str {
        &self.default_model
    }
}

#[async_trait]
impl LlmProvider for MlxSidecarLlmProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let model = request.model.unwrap_or_else(|| self.default_model.clone());

        let messages: Vec<ChatMessage> = request
            .messages
            .into_iter()
            .map(|m| ChatMessage {
                role: match m.role {
                    ChatRole::System => "system".to_string(),
                    ChatRole::User => "user".to_string(),
                    ChatRole::Assistant => "assistant".to_string(),
                },
                content: m.content,
            })
            .collect();

        let chat_req = ChatCompletionRequest {
            model,
            messages,
            temperature: request.temperature,
            max_tokens: request.max_tokens,
        };

        let response = self
            .client
            .chat_completion(chat_req)
            .await
            .map_err(|err| LlmError::Other(err.to_string()))?;

        let text = response
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        let meta = if let Some(usage) = &response.usage {
            serde_json::json!({
                "provider": "mlx",
                "model": response.model,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
            })
        } else {
            serde_json::json!({
                "provider": "mlx",
                "model": response.model,
            })
        };

        Ok(CompletionResponse {
            text,
            model: response.model.clone(),
            provider: "mlx".to_string(),
            meta,
        })
    }

    fn provider_id(&self) -> &str {
        &self.provider_id
    }

    fn supports(&self, capability: ModelCapability, _model: Option<&str>) -> bool {
        match capability {
            ModelCapability::Chat | ModelCapability::Code | ModelCapability::Sql => true,
            ModelCapability::Embedding => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mlx::client::MlxSidecarClient;

    #[tokio::test]
    async fn provider_id_includes_model() {
        let client =
            MlxSidecarClient::new(&crate::mlx::config::MlxSidecarConfig::default()).unwrap();
        let provider = MlxSidecarLlmProvider::new(client, "qwen2.5-coder-7b");
        assert_eq!(provider.provider_id(), "mlx:qwen2.5-coder-7b");
    }

    #[tokio::test]
    async fn supports_chat_code_sql_not_embedding() {
        let client =
            MlxSidecarClient::new(&crate::mlx::config::MlxSidecarConfig::default()).unwrap();
        let provider = MlxSidecarLlmProvider::new(client, "test");
        assert!(provider.supports(ModelCapability::Chat, None));
        assert!(provider.supports(ModelCapability::Code, None));
        assert!(provider.supports(ModelCapability::Sql, None));
        assert!(!provider.supports(ModelCapability::Embedding, None));
    }
}
