//! LLM provider abstraction.
//!
//! The catalog never calls a specific model directly; it talks through
//! [`LlmProvider`]. A deterministic stub ships here so the whole `ask` flow is
//! testable end-to-end without an external service. Wiring a real backend
//! (Ollama, OpenAI, etc.) is a later phase — it only needs to implement this
//! trait.

use std::sync::Arc;

use async_trait::async_trait;

use crate::catalog::card::AssembledContext;

/// A prompt handed to a provider.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmPrompt {
    /// The user's natural-language question.
    pub question: String,
    /// The assembled schema context (object cards), already under budget.
    pub context: AssembledContext,
    /// Optional system preamble describing the dialect / scope.
    pub system: Option<String>,
}

/// A completed response.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LlmResponse {
    pub text: String,
    /// Free-form provider-specific metadata (model name, token usage, ...).
    pub meta: serde_json::Value,
}

impl LlmResponse {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            meta: serde_json::Value::Null,
        }
    }
}

/// Capability a model can declare.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ModelCapability {
    Chat,
    Code,
    Sql,
    Embedding,
}

/// Specification for a model known to a provider.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ModelSpec {
    pub id: String,
    pub provider: String,
    pub capabilities: Vec<ModelCapability>,
    pub context_window: Option<usize>,
    pub embedding_dimensions: Option<usize>,
}

/// Role in a chat message.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

/// One message in a chat completion request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
}

/// OpenAI-style completion request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    pub model: Option<String>,
    pub capability: Option<ModelCapability>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
    #[serde(default)]
    pub metadata: serde_json::Value,
}

/// OpenAI-style completion response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CompletionResponse {
    pub text: String,
    pub model: String,
    pub provider: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

/// A tool the model may call. `input_schema` is a JSON Schema object; it is
/// sent verbatim to native providers and rendered as text for the fallback.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// A call the model asked for. `id` is provider-assigned for native providers
/// and `"call-<n>"` for the text fallback; it must round-trip unchanged.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// A message in an agent conversation. Text and tool traffic are separate
/// variants so compaction can keep call/result pairs together.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ToolMessage {
    System(String),
    User(String),
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResults(Vec<ToolResult>),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ToolCompletionRequest {
    pub messages: Vec<ToolMessage>,
    pub tools: Vec<ToolSpec>,
    pub model: Option<String>,
    pub max_tokens: Option<usize>,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolCompletion {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub model: String,
    pub provider: String,
}

impl CompletionRequest {
    /// Convert a legacy [`LlmPrompt`] into a [`CompletionRequest`].
    pub fn from_prompt(prompt: &LlmPrompt) -> Self {
        let mut messages = Vec::with_capacity(2);
        if let Some(system) = prompt.system.as_deref()
            && !system.is_empty()
        {
            messages.push(ChatMessage {
                role: ChatRole::System,
                content: system.to_string(),
            });
        }
        let user = if prompt.context.text.is_empty() {
            prompt.question.clone()
        } else {
            format!("{}\n\n{}", prompt.question, prompt.context.text)
        };
        messages.push(ChatMessage {
            role: ChatRole::User,
            content: user,
        });
        Self {
            messages,
            model: None,
            capability: None,
            max_tokens: None,
            temperature: None,
            metadata: serde_json::Value::Null,
        }
    }
}

impl CompletionResponse {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            model: String::new(),
            provider: String::new(),
            meta: serde_json::Value::Null,
        }
    }
}

/// Provider identifier for the built-in stub LLM. Mirrors
/// [`crate::catalog::STUB_PROVIDER_ID`] on the embedding side so engine-level selection
/// assertions have a stable, shared constant to compare against.
pub const STUB_LLM_PROVIDER_ID: &str = "stub";

/// The contract for anything that can answer schema questions.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Primary new method: OpenAI-style completion.
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError>;

    /// Legacy compatibility adapter. Default implementation converts the prompt
    /// to a [`CompletionRequest`] and calls [`complete`].
    async fn ask(&self, prompt: LlmPrompt) -> Result<LlmResponse, LlmError> {
        let req = CompletionRequest::from_prompt(&prompt);
        let resp = self.complete(req).await?;
        Ok(LlmResponse {
            text: resp.text,
            meta: resp.meta,
        })
    }

    /// A stable identifier for this provider, used for diagnostics and
    /// engine-level selection assertions. Defaults to `"unknown"`; real
    /// providers (the stub, Ollama, ...) override it.
    fn provider_id(&self) -> &str {
        "unknown"
    }

    /// Models advertised by this provider. Default is empty.
    fn models(&self) -> &[ModelSpec] {
        &[]
    }

    /// Whether this provider supports `capability` for `model`.
    fn supports(&self, capability: ModelCapability, model: Option<&str>) -> bool {
        self.models()
            .iter()
            .any(|m| model.is_none_or(|id| id == m.id) && m.capabilities.contains(&capability))
    }

    /// Whether [`complete_with_tools`] uses the provider's native tool-calling
    /// wire format. Default false: the loop then uses the text protocol.
    fn supports_native_tools(&self) -> bool {
        false
    }

    /// Default: flatten [`ToolMessage`]s to plain chat via
    /// [`crate::catalog::agent::text_protocol::flatten`], call [`complete`], then parse
    /// tool calls out of the reply with [`crate::catalog::agent::text_protocol::parse`].
    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletion, LlmError> {
        let messages =
            crate::catalog::agent::text_protocol::flatten(&request.messages, &request.tools);
        let completion = self
            .complete(CompletionRequest {
                messages,
                model: request.model.clone(),
                capability: None,
                max_tokens: request.max_tokens,
                temperature: request.temperature,
                metadata: serde_json::Value::Null,
            })
            .await?;
        let mut next_id = 0usize;
        let (text, tool_calls) =
            crate::catalog::agent::text_protocol::parse(&completion.text, &mut next_id);
        Ok(ToolCompletion {
            text,
            tool_calls,
            model: completion.model,
            provider: completion.provider,
        })
    }
}

/// Failures from a provider.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("llm provider error: {0}")]
    Other(String),
}

impl LlmError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        match self {
            Self::Other(detail) => sqlly_lang::DisplayMessage::raw(detail),
        }
    }
}

/// A deterministic, dependency-free provider used for tests and as the default
/// when no real model is configured.
///
/// It doesn't understand SQL, but it produces a stable, structured echo of the
/// question and the objects it was given context for — enough to verify the
/// whole pipeline (sync → rank → assemble → ask) works.
pub struct StubLlmProvider;

impl StubLlmProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for StubLlmProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LlmProvider for StubLlmProvider {
    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut lines = Vec::new();
        for msg in &request.messages {
            let role = match msg.role {
                ChatRole::System => "system",
                ChatRole::User => "user",
                ChatRole::Assistant => "assistant",
            };
            lines.push(format!("[{role}] {}", msg.content));
        }
        if let Some(cap) = &request.capability {
            lines.push(format!("[capability] {cap:?}"));
        }
        lines.push(
            "[answer] I can reason about the listed objects. (stub provider — configure a real \
             LlmProvider for generated SQL.)"
                .to_string(),
        );
        let text = lines.join("\n");

        Ok(CompletionResponse {
            text,
            model: request.model.unwrap_or_else(|| "stub".to_string()),
            provider: "stub".to_string(),
            meta: serde_json::json!({
                "provider": "stub",
                "messages": request.messages.len(),
            }),
        })
    }

    /// Override the default `ask` adapter so tests that assert the legacy
    /// stub output format continue to pass during migration.
    async fn ask(&self, prompt: LlmPrompt) -> Result<LlmResponse, LlmError> {
        let mut lines = Vec::new();
        if let Some(sys) = &prompt.system {
            lines.push(format!("[system] {sys}"));
        }
        lines.push(format!("[question] {}", prompt.question));
        lines.push(format!(
            "[context] {} object(s), ~{} tokens",
            prompt.context.object_keys.len(),
            prompt.context.tokens
        ));
        for key in &prompt.context.object_keys {
            lines.push(format!("  - {key}"));
        }
        lines.push(
            "[answer] I can reason about the listed objects. (stub provider — configure a real \
             LlmProvider for generated SQL.)"
                .to_string(),
        );
        let text = lines.join("\n");

        Ok(LlmResponse {
            meta: serde_json::json!({
                "provider": "stub",
                "objects": prompt.context.object_keys.len(),
                "context_tokens": prompt.context.tokens,
            }),
            text,
        })
    }

    fn provider_id(&self) -> &str {
        STUB_LLM_PROVIDER_ID
    }
}

/// A type-erased provider handle convenient for storage in engine state.
pub type DynLlmProvider = Arc<dyn LlmProvider>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::card::{assemble_context, estimate_tokens};
    use crate::catalog::store::{CatalogStore, StoredObject};
    use crate::catalog::wire::{
        CatalogColumn, CatalogObject as WireObject, CatalogObjectKind, CatalogSource, SourceKind,
    };

    fn prompt(question: &str, objs: &[StoredObject]) -> LlmPrompt {
        let ctx = assemble_context(objs, 10_000);
        LlmPrompt {
            question: question.into(),
            context: ctx,
            system: Some("You are a SQL schema assistant.".into()),
        }
    }

    #[tokio::test]
    async fn stub_returns_deterministic_response() {
        let store = CatalogStore::open_in_memory().unwrap();
        let obj = WireObject {
            schema: Some("dbo".into()),
            name: "users".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![CatalogColumn {
                name: "id".into(),
                type_name: Some("INT".into()),
                ordinal: 1,
                nullable: false,
            }],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        };
        store.sync(&[obj]).unwrap();
        let objs = store.live_objects().unwrap();

        let stub = StubLlmProvider::new();
        let r1 = stub
            .ask(prompt("what columns does users have", &objs))
            .await
            .unwrap();
        let r2 = stub
            .ask(prompt("what columns does users have", &objs))
            .await
            .unwrap();
        assert_eq!(r1.text, r2.text);
        assert!(r1.text.contains("[question] what columns does users have"));
        assert!(r1.text.contains("dbo.users"));
        assert_eq!(r1.meta["provider"], "stub");
    }

    #[tokio::test]
    async fn estimate_tokens_nonzero_for_context() {
        let store = CatalogStore::open_in_memory().unwrap();
        let obj = WireObject {
            schema: Some("dbo".into()),
            name: "users".into(),
            kind: CatalogObjectKind::Table,
            columns: vec![make_column("id", "INT", 1)],
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        };
        store.sync(&[obj]).unwrap();
        let objs = store.live_objects().unwrap();
        let ctx = assemble_context(&objs, 10_000);
        assert!(ctx.tokens > 0);
        assert!(estimate_tokens(&ctx.text) > 0);
    }

    fn make_column(name: &str, ty: &str, ord: i64) -> CatalogColumn {
        CatalogColumn {
            name: name.into(),
            type_name: Some(ty.into()),
            ordinal: ord,
            nullable: false,
        }
    }
}
