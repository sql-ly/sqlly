//! [`OllamaLlmProvider`]: a `sqlly-catalog` [`LlmProvider`] backed by a local
//! Ollama chat model.

use crate::catalog::{CompletionRequest, CompletionResponse, LlmError, LlmProvider};
use async_trait::async_trait;

use crate::ollama::client::{
    ChatMessage, ChatOptions, ChatRequest, OllamaClient, OllamaConfig, OllamaError,
};

/// Default context window hint sent to the model. Schema answers are small, but
/// the assembled context can be a few thousand tokens, so we ask for a roomy
/// window. Models cap this to their own maximum.
pub const DEFAULT_NUM_CTX: u32 = 8192;

/// Low temperature: schema Q&A and SQL generation want deterministic, grounded
/// answers, not creativity.
pub const DEFAULT_TEMPERATURE: f32 = 0.1;

fn output_token_limit(max_tokens: Option<usize>) -> Option<u32> {
    max_tokens.map(|limit| u32::try_from(limit).unwrap_or(u32::MAX))
}

/// An [`LlmProvider`] that answers schema questions via a local Ollama model.
pub struct OllamaLlmProvider {
    client: OllamaClient,
    model: String,
    num_ctx: u32,
    temperature: f32,
    provider_id: String,
}

impl OllamaLlmProvider {
    /// Construct a provider from config + model name. No network I/O happens
    /// here; the first request is what actually contacts Ollama.
    pub fn new(config: &OllamaConfig, model: impl Into<String>) -> Result<Self, OllamaError> {
        Ok(Self::with_client(OllamaClient::new(config)?, model))
    }

    /// Construct from an existing client (used by tests and callers that share a
    /// client).
    pub fn with_client(client: OllamaClient, model: impl Into<String>) -> Self {
        let model = model.into();
        let provider_id = format!("ollama:{model}");
        Self {
            client,
            model,
            num_ctx: DEFAULT_NUM_CTX,
            temperature: DEFAULT_TEMPERATURE,
            provider_id,
        }
    }

    /// Override the context-window hint.
    pub fn with_num_ctx(mut self, num_ctx: u32) -> Self {
        self.num_ctx = num_ctx;
        self
    }

    /// Override the sampling temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature;
        self
    }

    /// The model name this provider targets.
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[async_trait]
impl LlmProvider for OllamaLlmProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let messages: Vec<ChatMessage> = request
            .messages
            .into_iter()
            .map(|m| ChatMessage {
                role: match m.role {
                    crate::catalog::llm::ChatRole::System => "system".to_string(),
                    crate::catalog::llm::ChatRole::User => "user".to_string(),
                    crate::catalog::llm::ChatRole::Assistant => "assistant".to_string(),
                },
                content: m.content,
            })
            .collect();

        let chat_req = ChatRequest {
            model: request.model.unwrap_or_else(|| self.model.clone()),
            messages,
            stream: false,
            options: Some(ChatOptions {
                temperature: request.temperature.or(Some(self.temperature)),
                num_ctx: Some(self.num_ctx),
                num_predict: output_token_limit(request.max_tokens),
            }),
        };

        let response = self
            .client
            .chat(chat_req)
            .await
            .map_err(|err| LlmError::Other(err.to_string()))?;

        let text = response.message.content;
        let meta = serde_json::json!({
            "provider": "ollama",
            "model": self.model,
            "prompt_eval_count": response.prompt_eval_count,
            "eval_count": response.eval_count,
            "total_duration": response.total_duration,
        });

        Ok(CompletionResponse {
            text,
            model: self.model.clone(),
            provider: "ollama".to_string(),
            meta,
        })
    }

    fn provider_id(&self) -> &str {
        &self.provider_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_token_limit_preserves_and_saturates_request_value() {
        assert_eq!(output_token_limit(None), None);
        assert_eq!(output_token_limit(Some(2048)), Some(2048));
        assert_eq!(output_token_limit(Some(usize::MAX)), Some(u32::MAX));
    }
}
