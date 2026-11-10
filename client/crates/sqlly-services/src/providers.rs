//! Environment-driven AI provider selection, shared by the CLI engine and any
//! direct `Services` consumer (GPUI, tests, future workers). One factory so
//! provider behavior can never drift between the engine subprocess and the
//! in-process services path.
//!
//! Both providers default to the deterministic catalog stubs. Selecting
//! `ollama` or `mlx` (via [`LLM_PROVIDER_ENV`] / [`EMBEDDING_PROVIDER_ENV`])
//! builds the matching provider from the corresponding `*_MODEL` variable and
//! config. Any problem — unknown provider value, missing model name, or a
//! failed construction (e.g. Ollama not running, MLX sidecar missing
//! `sentence_transformers`, model not pulled) — is logged, captured in the
//! returned [`ProviderDiagnostics`], and the stub is used instead. Callers
//! must always boot regardless of local-AI configuration; provider call
//! failures surface later per request or as a fallback note, not as a dead
//! process.

use std::sync::Arc;

use sqlly_ai::catalog::{
    DynEmbeddingProvider, DynLlmProvider, DynVectorStore, StubEmbeddingProvider, StubLlmProvider,
};
use sqlly_ai::mlx::{MlxSidecarConfig, MlxSidecarEmbeddingProvider, MlxSidecarLlmProvider};
use sqlly_ai::ollama::{OllamaConfig, OllamaEmbeddingProvider, OllamaLlmProvider};

/// Env vars selecting the LLM / embedding providers. Each accepts `stub` (the
/// deterministic default), `ollama`, or `mlx`. When a real provider is
/// selected the matching `*_MODEL` variable must name an installed local
/// model; if it is missing — or the provider cannot be reached/initialized —
/// a warning is logged and the stub is used so the caller always boots.
pub const LLM_PROVIDER_ENV: &str = "SQLLY_LLM_PROVIDER";
pub const LLM_MODEL_ENV: &str = "SQLLY_LLM_MODEL";
pub const EMBEDDING_PROVIDER_ENV: &str = "SQLLY_EMBEDDING_PROVIDER";
pub const EMBEDDING_MODEL_ENV: &str = "SQLLY_EMBEDDING_MODEL";

/// Build one explicitly-selected local chat provider for interactive tools
/// such as the AI Playground. Unlike the environment factory, this never
/// silently substitutes a stub: a user's provider/model choice either creates
/// that provider or returns an actionable error.
pub fn build_local_llm_provider(provider: &str, model: &str) -> Result<DynLlmProvider, String> {
    let model = model.trim();
    if model.is_empty() {
        return Err("Choose a model before running the prompt.".to_string());
    }
    match provider.trim().to_ascii_lowercase().as_str() {
        "ollama" => OllamaLlmProvider::new(&OllamaConfig::from_env(), model.to_string())
            .map(|provider| Arc::new(provider) as DynLlmProvider)
            .map_err(|error| format!("Could not configure Ollama: {error}")),
        "mlx" => sqlly_ai::mlx::MlxSidecarClient::new(&MlxSidecarConfig::from_env())
            .map(|client| {
                Arc::new(MlxSidecarLlmProvider::new(client, model.to_string())) as DynLlmProvider
            })
            .map_err(|error| format!("Could not configure MLX: {error}")),
        other => Err(format!(
            "Unsupported local AI provider '{other}'. Choose Ollama or MLX."
        )),
    }
}

/// Which concrete provider a configured slot resolved to, plus why it fell
/// back when the requested provider could not be constructed. Lets callers
/// tell the user "you asked for MLX but it isn't available, so the
/// deterministic stub was used" instead of silently degrading.
#[derive(Clone, Debug, Default)]
pub struct ProviderDiagnostics {
    /// The provider the environment requested (e.g. `"mlx"`), if any.
    pub llm_requested: Option<String>,
    /// The provider actually in use (`"mlx"`, `"ollama"`, or `"stub"`).
    pub llm_active: String,
    /// Why the LLM provider fell back, when `llm_active` != requested.
    pub llm_fallback_reason: Option<String>,
    pub embedding_requested: Option<String>,
    pub embedding_active: String,
    pub embedding_fallback_reason: Option<String>,
}

impl ProviderDiagnostics {
    /// Diagnostics for a stub-only stack that never consulted the
    /// environment: nothing requested, stubs active, no fallback.
    pub fn stub_default() -> Self {
        Self {
            llm_requested: None,
            llm_active: "stub".to_string(),
            llm_fallback_reason: None,
            embedding_requested: None,
            embedding_active: "stub".to_string(),
            embedding_fallback_reason: None,
        }
    }

    pub fn embedding_fallback_message(&self) -> Option<sqlly_core::DisplayMessage> {
        Self::fallback_message(
            true,
            self.embedding_requested.as_deref(),
            &self.embedding_active,
            self.embedding_fallback_reason.as_deref(),
        )
    }

    pub fn embedding_fallback_note(&self) -> Option<String> {
        self.embedding_fallback_message()
            .map(sqlly_core::DisplayMessage::into_fallback)
    }

    pub fn llm_fallback_message(&self) -> Option<sqlly_core::DisplayMessage> {
        Self::fallback_message(
            false,
            self.llm_requested.as_deref(),
            &self.llm_active,
            self.llm_fallback_reason.as_deref(),
        )
    }

    /// English compatibility fallback; localized callers use
    /// [`Self::llm_fallback_message`].
    pub fn llm_fallback_note(&self) -> Option<String> {
        self.llm_fallback_message()
            .map(sqlly_core::DisplayMessage::into_fallback)
    }

    fn fallback_message(
        embedding: bool,
        requested: Option<&str>,
        active: &str,
        reason: Option<&str>,
    ) -> Option<sqlly_core::DisplayMessage> {
        use sqlly_core::DisplayMessage;
        let requested = requested?;
        if requested == "stub" || requested == active {
            return None;
        }
        let kind = if embedding { "Embedding" } else { "LLM" };
        let key = match (embedding, reason.is_some()) {
            (true, false) => "services.providers.embedding_fallback",
            (true, true) => "services.providers.embedding_fallback_with_reason",
            (false, false) => "services.providers.llm_fallback",
            (false, true) => "services.providers.llm_fallback_with_reason",
        };
        let mut note = format!(
            "{kind} provider '{requested}' was requested but is unavailable; using '{active}' instead."
        );
        if let Some(reason) = reason {
            note.push_str(&format!(" ({reason})"));
        }
        let message = DisplayMessage::new(key, note)
            .with_arg("requested_provider", requested)
            .with_arg("active_provider", active);
        Some(match reason {
            Some(reason) => message.with_arg("detail", reason),
            None => message,
        })
    }
}

/// Construct the schema-intelligence providers from the environment. See the
/// module docs for the selection and fallback rules.
pub async fn build_catalog_providers_from_env() -> (
    DynLlmProvider,
    DynEmbeddingProvider,
    DynVectorStore,
    ProviderDiagnostics,
) {
    let ollama_config = OllamaConfig::from_env();
    let mlx_config = MlxSidecarConfig::from_env();

    let mut diagnostics = ProviderDiagnostics {
        llm_requested: std::env::var(LLM_PROVIDER_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty()),
        embedding_requested: std::env::var(EMBEDDING_PROVIDER_ENV)
            .ok()
            .filter(|v| !v.trim().is_empty()),
        ..ProviderDiagnostics::default()
    };

    let llm: DynLlmProvider = match std::env::var(LLM_PROVIDER_ENV).ok().as_deref() {
        Some("mlx") => {
            let mlx_chat_model = std::env::var(LLM_MODEL_ENV)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| mlx_config.chat_model.clone());
            match mlx_chat_model {
                Some(model) => match sqlly_ai::mlx::MlxSidecarClient::new(&mlx_config) {
                    Ok(client) => {
                        tracing::info!(model = %model, "using MLX LLM provider");
                        diagnostics.llm_active = "mlx".to_string();
                        Arc::new(MlxSidecarLlmProvider::new(client, model))
                    }
                    Err(err) => {
                        tracing::warn!(
                            model = %model,
                            error = %err,
                            "failed to build MLX LLM client; falling back to stub"
                        );
                        diagnostics.llm_active = "stub".to_string();
                        diagnostics.llm_fallback_reason =
                            Some(format!("failed to build MLX LLM client: {err}"));
                        Arc::new(StubLlmProvider::new())
                    }
                },
                None => {
                    tracing::warn!(
                        "{LLM_PROVIDER_ENV}=mlx but no chat model is set (set {LLM_MODEL_ENV} or SQLLY_MLX_CHAT_MODEL); falling back to stub"
                    );
                    diagnostics.llm_active = "stub".to_string();
                    diagnostics.llm_fallback_reason =
                        Some("no MLX chat model configured".to_string());
                    Arc::new(StubLlmProvider::new())
                }
            }
        }
        Some("ollama") => match std::env::var(LLM_MODEL_ENV) {
            Ok(model) if !model.trim().is_empty() => {
                match OllamaLlmProvider::new(&ollama_config, model.clone()) {
                    Ok(provider) => {
                        tracing::info!(model = %model, "using Ollama LLM provider");
                        diagnostics.llm_active = "ollama".to_string();
                        Arc::new(provider)
                    }
                    Err(err) => {
                        tracing::warn!(
                            model = %model,
                            error = %err,
                            "failed to initialize Ollama LLM provider; falling back to stub"
                        );
                        diagnostics.llm_active = "stub".to_string();
                        diagnostics.llm_fallback_reason =
                            Some(format!("failed to initialize Ollama LLM provider: {err}"));
                        Arc::new(StubLlmProvider::new())
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "{LLM_PROVIDER_ENV}=ollama but {LLM_MODEL_ENV} is unset; falling back to stub"
                );
                diagnostics.llm_active = "stub".to_string();
                diagnostics.llm_fallback_reason = Some("no Ollama model configured".to_string());
                Arc::new(StubLlmProvider::new())
            }
        },
        Some("stub") | None => {
            diagnostics.llm_active = "stub".to_string();
            Arc::new(StubLlmProvider::new())
        }
        Some(other) => {
            tracing::warn!("unknown {LLM_PROVIDER_ENV}={other:?}; falling back to stub");
            diagnostics.llm_active = "stub".to_string();
            diagnostics.llm_fallback_reason = Some(format!("unknown LLM provider {other:?}"));
            Arc::new(StubLlmProvider::new())
        }
    };

    let embedding: DynEmbeddingProvider = match std::env::var(EMBEDDING_PROVIDER_ENV)
        .ok()
        .as_deref()
    {
        Some("mlx") => {
            let mlx_embedding_model = std::env::var(EMBEDDING_MODEL_ENV)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| mlx_config.embedding_model.clone());
            match mlx_embedding_model {
                Some(model) => {
                    match MlxSidecarEmbeddingProvider::connect(&mlx_config, model.clone()).await {
                        Ok(provider) => {
                            tracing::info!(model = %model, "using MLX embedding provider");
                            diagnostics.embedding_active = "mlx".to_string();
                            Arc::new(provider)
                        }
                        Err(err) => {
                            tracing::warn!(
                                model = %model,
                                error = %err,
                                "failed to initialize MLX embedding provider; falling back to stub"
                            );
                            diagnostics.embedding_active = "stub".to_string();
                            diagnostics.embedding_fallback_reason = Some(format!(
                                "failed to initialize MLX embedding provider: {err}"
                            ));
                            Arc::new(StubEmbeddingProvider::new())
                        }
                    }
                }
                None => {
                    tracing::warn!(
                        "{EMBEDDING_PROVIDER_ENV}=mlx but no embedding model is set (set {EMBEDDING_MODEL_ENV} or SQLLY_MLX_EMBEDDING_MODEL); falling back to stub"
                    );
                    diagnostics.embedding_active = "stub".to_string();
                    diagnostics.embedding_fallback_reason =
                        Some("no MLX embedding model configured".to_string());
                    Arc::new(StubEmbeddingProvider::new())
                }
            }
        }
        Some("ollama") => match std::env::var(EMBEDDING_MODEL_ENV) {
            Ok(model) if !model.trim().is_empty() => {
                match OllamaEmbeddingProvider::connect(&ollama_config, model.clone()).await {
                    Ok(provider) => {
                        tracing::info!(model = %model, "using Ollama embedding provider");
                        diagnostics.embedding_active = "ollama".to_string();
                        Arc::new(provider)
                    }
                    Err(err) => {
                        tracing::warn!(
                            model = %model,
                            error = %err,
                            "failed to initialize Ollama embedding provider; falling back to stub"
                        );
                        diagnostics.embedding_active = "stub".to_string();
                        diagnostics.embedding_fallback_reason = Some(format!(
                            "failed to initialize Ollama embedding provider: {err}"
                        ));
                        Arc::new(StubEmbeddingProvider::new())
                    }
                }
            }
            _ => {
                tracing::warn!(
                    "{EMBEDDING_PROVIDER_ENV}=ollama but {EMBEDDING_MODEL_ENV} is unset; falling back to stub"
                );
                diagnostics.embedding_active = "stub".to_string();
                diagnostics.embedding_fallback_reason =
                    Some("no Ollama embedding model configured".to_string());
                Arc::new(StubEmbeddingProvider::new())
            }
        },
        Some("stub") | None => {
            diagnostics.embedding_active = "stub".to_string();
            Arc::new(StubEmbeddingProvider::new())
        }
        Some(other) => {
            tracing::warn!("unknown {EMBEDDING_PROVIDER_ENV}={other:?}; falling back to stub");
            diagnostics.embedding_active = "stub".to_string();
            diagnostics.embedding_fallback_reason =
                Some(format!("unknown embedding provider {other:?}"));
            Arc::new(StubEmbeddingProvider::new())
        }
    };

    let vector_store: DynVectorStore = Arc::new(sqlly_ai::catalog::SqliteVectorStore::new(
        sqlly_ai::catalog::CatalogStore::open_in_memory()
            .expect("in-memory catalog store always opens"),
    ));
    (llm, embedding, vector_store, diagnostics)
}

/// A point-in-time picture of the AI provider stack: what was requested, what
/// is active, why anything fell back, and — when an Ollama-backed provider is
/// requested or active — whether the runtime answers right now and which
/// models it has installed. Probing happens on request with a short timeout,
/// never at engine boot.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ProviderStatusReport {
    pub llm_requested: Option<String>,
    pub llm_active: String,
    pub llm_fallback_reason: Option<String>,
    pub embedding_requested: Option<String>,
    pub embedding_active: String,
    pub embedding_fallback_reason: Option<String>,
    /// Whether the local AI runtime answered the probe; `None` when no
    /// runtime-backed provider is requested or active (stub-only stacks have
    /// nothing to probe).
    pub runtime_reachable: Option<bool>,
    pub runtime_version: Option<String>,
    pub installed_models: Vec<String>,
    /// The probe failure, when the runtime did not answer.
    pub runtime_detail: Option<String>,
}

/// Build the provider status for `diagnostics`, probing the Ollama runtime
/// (3s timeout) when any requested or active provider is Ollama-backed.
pub async fn provider_status(diagnostics: &ProviderDiagnostics) -> ProviderStatusReport {
    let mut report = ProviderStatusReport {
        llm_requested: diagnostics.llm_requested.clone(),
        llm_active: diagnostics.llm_active.clone(),
        llm_fallback_reason: diagnostics.llm_fallback_reason.clone(),
        embedding_requested: diagnostics.embedding_requested.clone(),
        embedding_active: diagnostics.embedding_active.clone(),
        embedding_fallback_reason: diagnostics.embedding_fallback_reason.clone(),
        ..ProviderStatusReport::default()
    };
    let ollama_relevant = [
        diagnostics.llm_requested.as_deref(),
        Some(diagnostics.llm_active.as_str()),
        diagnostics.embedding_requested.as_deref(),
        Some(diagnostics.embedding_active.as_str()),
    ]
    .into_iter()
    .flatten()
    .any(|p| p.eq_ignore_ascii_case("ollama"));
    if !ollama_relevant {
        return report;
    }
    let config = sqlly_ai::ollama::OllamaConfig::from_env();
    match sqlly_ai::ollama::OllamaClient::with_timeout(&config, std::time::Duration::from_secs(3)) {
        Ok(client) => match client.version().await {
            Ok(version) => {
                report.runtime_reachable = Some(true);
                report.runtime_version = Some(version);
                report.installed_models = client.list_models().await.unwrap_or_default();
            }
            Err(error) => {
                report.runtime_reachable = Some(false);
                report.runtime_detail = Some(error.to_string());
            }
        },
        Err(error) => {
            report.runtime_reachable = Some(false);
            report.runtime_detail = Some(error.to_string());
        }
    }
    report
}

/// Wire format for a cloud chat API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudWire {
    OpenAiChat,
    AnthropicMessages,
    GeminiGenerateContent,
}

/// Reasoning/thinking controls mapped from a provider profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudReasoning {
    None,
    OpenAiEffort(&'static str),
    AnthropicEffort(&'static str),
    AnthropicBudget(u32),
    GeminiBudget(i32),
    GeminiLevel(&'static str),
}

/// Fully-resolved cloud provider endpoint and auth, built by the GPUI layer.
#[derive(Debug, Clone)]
pub struct CloudProviderSpec {
    pub wire: CloudWire,
    pub endpoint: String,
    pub headers: Vec<(String, String)>,
    pub model: String,
    pub provider_id: String,
    pub reasoning: CloudReasoning,
}

/// An [`LlmProvider`] backed by a cloud chat API. Explicit opt-in only: the
/// caller decides (and tells the user) that prompts and schema context leave
/// the machine. Auth headers are supplied in the spec and never logged.
pub struct CloudLlmProvider {
    spec: CloudProviderSpec,
    client: reqwest::Client,
}

impl CloudLlmProvider {
    pub fn new(spec: CloudProviderSpec) -> Result<Self, String> {
        if spec.model.trim().is_empty() {
            return Err("No model is selected for the cloud provider".to_string());
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(90))
            .build()
            .map_err(|error| format!("Could not create the cloud AI client: {error}"))?;
        Ok(Self { spec, client })
    }

    pub fn build_body(&self, request: &sqlly_ai::catalog::CompletionRequest) -> serde_json::Value {
        self.body(request)
    }

    pub fn extract_response_text(&self, value: &serde_json::Value) -> Option<String> {
        self.extract_text(value)
    }

    fn body(&self, request: &sqlly_ai::catalog::CompletionRequest) -> serde_json::Value {
        use sqlly_ai::catalog::ChatRole;
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.spec.model.clone());
        let mut max_tokens = request.max_tokens.unwrap_or(1024);
        match self.spec.wire {
            CloudWire::OpenAiChat => {
                let messages: Vec<serde_json::Value> = request
                    .messages
                    .iter()
                    .map(|message| {
                        serde_json::json!({
                            "role": match message.role {
                                ChatRole::System => "system",
                                ChatRole::User => "user",
                                ChatRole::Assistant => "assistant",
                            },
                            "content": message.content,
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({
                    "model": model,
                    "messages": messages,
                    "max_tokens": max_tokens,
                });
                if let Some(temperature) = request.temperature {
                    body["temperature"] = serde_json::json!(temperature);
                }
                if let CloudReasoning::OpenAiEffort(effort) = &self.spec.reasoning {
                    body["reasoning_effort"] = serde_json::json!(effort);
                }
                body
            }
            CloudWire::AnthropicMessages => {
                if let CloudReasoning::AnthropicBudget(budget) = self.spec.reasoning
                    && max_tokens <= budget as usize
                {
                    max_tokens = budget as usize + 1024;
                }
                let system: Vec<&str> = request
                    .messages
                    .iter()
                    .filter(|m| m.role == ChatRole::System)
                    .map(|m| m.content.as_str())
                    .collect();
                let messages: Vec<serde_json::Value> = request
                    .messages
                    .iter()
                    .filter(|m| m.role != ChatRole::System)
                    .map(|message| {
                        serde_json::json!({
                            "role": match message.role {
                                ChatRole::Assistant => "assistant",
                                _ => "user",
                            },
                            "content": message.content,
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({
                    "model": model,
                    "max_tokens": max_tokens,
                    "messages": messages,
                });
                if !system.is_empty() {
                    body["system"] = serde_json::json!(system.join("\n\n"));
                }
                if let Some(temperature) = request.temperature {
                    body["temperature"] = serde_json::json!(temperature);
                }
                match &self.spec.reasoning {
                    CloudReasoning::AnthropicEffort(effort) => {
                        body["thinking"] = serde_json::json!({"type": "adaptive"});
                        body["output_config"] = serde_json::json!({"effort": effort});
                    }
                    CloudReasoning::AnthropicBudget(budget) => {
                        body["thinking"] =
                            serde_json::json!({"type": "enabled", "budget_tokens": budget});
                    }
                    _ => {}
                }
                body
            }
            CloudWire::GeminiGenerateContent => {
                let system: Vec<&str> = request
                    .messages
                    .iter()
                    .filter(|m| m.role == ChatRole::System)
                    .map(|m| m.content.as_str())
                    .collect();
                let contents: Vec<serde_json::Value> = request
                    .messages
                    .iter()
                    .filter(|m| m.role != ChatRole::System)
                    .map(|message| {
                        serde_json::json!({
                            "role": match message.role {
                                ChatRole::Assistant => "model",
                                _ => "user",
                            },
                            "parts": [{"text": message.content}],
                        })
                    })
                    .collect();
                let mut generation_config = serde_json::json!({
                    "maxOutputTokens": max_tokens,
                });
                if let Some(temperature) = request.temperature {
                    generation_config["temperature"] = serde_json::json!(temperature);
                }
                match &self.spec.reasoning {
                    CloudReasoning::GeminiBudget(budget) => {
                        generation_config["thinkingConfig"] =
                            serde_json::json!({"thinkingBudget": budget});
                    }
                    CloudReasoning::GeminiLevel(level) => {
                        generation_config["thinkingConfig"] =
                            serde_json::json!({"thinkingLevel": level});
                    }
                    _ => {}
                }
                let mut body = serde_json::json!({
                    "contents": contents,
                    "generationConfig": generation_config,
                });
                if !system.is_empty() {
                    body["system_instruction"] =
                        serde_json::json!({"parts": [{"text": system.join("\n\n")}]});
                }
                body
            }
        }
    }

    fn extract_text(&self, value: &serde_json::Value) -> Option<String> {
        match self.spec.wire {
            CloudWire::OpenAiChat => value
                .pointer("/choices/0/message/content")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            CloudWire::AnthropicMessages => value
                .get("content")
                .and_then(|v| v.as_array())
                .and_then(|blocks| {
                    blocks.iter().find_map(|block| {
                        (block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
                            .then(|| block.get("text").and_then(serde_json::Value::as_str))
                            .flatten()
                            .map(str::to_string)
                    })
                }),
            CloudWire::GeminiGenerateContent => value
                .pointer("/candidates/0/content/parts/0/text")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    }

    async fn post_json(
        &self,
        body: serde_json::Value,
    ) -> Result<serde_json::Value, sqlly_ai::catalog::LlmError> {
        let mut builder = self
            .client
            .post(&self.spec.endpoint)
            .header("content-type", "application/json");
        for (header, value) in &self.spec.headers {
            builder = builder.header(header, value);
        }
        let response = builder.json(&body).send().await.map_err(|error| {
            sqlly_ai::catalog::LlmError::Other(format!("cloud provider request failed: {error}"))
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| {
            sqlly_ai::catalog::LlmError::Other(format!(
                "cloud provider response unreadable: {error}"
            ))
        })?;
        if !status.is_success() {
            let brief: String = text.chars().take(240).collect();
            return Err(sqlly_ai::catalog::LlmError::Other(format!(
                "cloud provider answered {status}: {brief}"
            )));
        }
        serde_json::from_str(&text).map_err(|_| {
            sqlly_ai::catalog::LlmError::Other("cloud provider answered with non-JSON".to_string())
        })
    }

    pub(crate) fn tool_body(
        &self,
        request: &sqlly_ai::catalog::ToolCompletionRequest,
    ) -> serde_json::Value {
        use sqlly_ai::catalog::{ToolMessage, ToolResult};
        let model = request
            .model
            .clone()
            .unwrap_or_else(|| self.spec.model.clone());
        let mut max_tokens = request.max_tokens.unwrap_or(1024);
        match self.spec.wire {
            CloudWire::OpenAiChat => {
                let tools: Vec<serde_json::Value> = request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "type": "function",
                            "function": {
                                "name": tool.name,
                                "description": tool.description,
                                "parameters": tool.input_schema,
                            }
                        })
                    })
                    .collect();
                let messages: Vec<serde_json::Value> = request
                    .messages
                    .iter()
                    .flat_map(|message| self.openai_tool_messages(message))
                    .collect();
                let mut body = serde_json::json!({
                    "model": model,
                    "messages": messages,
                    "max_tokens": max_tokens,
                    "tools": tools,
                    "tool_choice": "auto",
                });
                if let Some(temperature) = request.temperature {
                    body["temperature"] = serde_json::json!(temperature);
                }
                if let CloudReasoning::OpenAiEffort(effort) = &self.spec.reasoning {
                    body["reasoning_effort"] = serde_json::json!(effort);
                }
                body
            }
            CloudWire::AnthropicMessages => {
                if let CloudReasoning::AnthropicBudget(budget) = self.spec.reasoning
                    && max_tokens <= budget as usize
                {
                    max_tokens = budget as usize + 1024;
                }
                let system: Vec<&str> = request
                    .messages
                    .iter()
                    .filter_map(|message| match message {
                        ToolMessage::System(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let messages: Vec<serde_json::Value> = request
                    .messages
                    .iter()
                    .filter(|message| !matches!(message, ToolMessage::System(_)))
                    .flat_map(|message| self.anthropic_tool_messages(message))
                    .collect();
                let tools: Vec<serde_json::Value> = request
                    .tools
                    .iter()
                    .map(|tool| {
                        serde_json::json!({
                            "name": tool.name,
                            "description": tool.description,
                            "input_schema": tool.input_schema,
                        })
                    })
                    .collect();
                let mut body = serde_json::json!({
                    "model": model,
                    "max_tokens": max_tokens,
                    "messages": messages,
                    "tools": tools,
                    "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
                });
                if !system.is_empty() {
                    body["system"] = serde_json::json!(system.join("\n\n"));
                }
                if let Some(temperature) = request.temperature {
                    body["temperature"] = serde_json::json!(temperature);
                }
                match &self.spec.reasoning {
                    CloudReasoning::AnthropicEffort(effort) => {
                        body["thinking"] = serde_json::json!({"type": "adaptive"});
                        body["output_config"] = serde_json::json!({"effort": effort});
                    }
                    CloudReasoning::AnthropicBudget(budget) => {
                        body["thinking"] =
                            serde_json::json!({"type": "enabled", "budget_tokens": budget});
                    }
                    _ => {}
                }
                body
            }
            CloudWire::GeminiGenerateContent => self.body(&sqlly_ai::catalog::CompletionRequest {
                messages: request
                    .messages
                    .iter()
                    .map(|message| match message {
                        ToolMessage::System(text) => sqlly_ai::catalog::ChatMessage {
                            role: sqlly_ai::catalog::ChatRole::System,
                            content: text.clone(),
                        },
                        ToolMessage::User(text) => sqlly_ai::catalog::ChatMessage {
                            role: sqlly_ai::catalog::ChatRole::User,
                            content: text.clone(),
                        },
                        ToolMessage::Assistant { text, .. } => sqlly_ai::catalog::ChatMessage {
                            role: sqlly_ai::catalog::ChatRole::Assistant,
                            content: text.clone(),
                        },
                        ToolMessage::ToolResults(results) => sqlly_ai::catalog::ChatMessage {
                            role: sqlly_ai::catalog::ChatRole::User,
                            content: results
                                .iter()
                                .map(|ToolResult { content, .. }| content.as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        },
                    })
                    .collect(),
                model: request.model.clone(),
                capability: None,
                max_tokens: request.max_tokens,
                temperature: request.temperature,
                metadata: serde_json::Value::Null,
            }),
        }
    }

    fn openai_tool_messages(
        &self,
        message: &sqlly_ai::catalog::ToolMessage,
    ) -> Vec<serde_json::Value> {
        use sqlly_ai::catalog::ToolMessage;
        match message {
            ToolMessage::System(text) => vec![serde_json::json!({
                "role": "system",
                "content": text,
            })],
            ToolMessage::User(text) => vec![serde_json::json!({
                "role": "user",
                "content": text,
            })],
            ToolMessage::Assistant { text, tool_calls } => {
                let tool_calls_json: Vec<serde_json::Value> = tool_calls
                    .iter()
                    .map(|call| {
                        serde_json::json!({
                            "id": call.id,
                            "type": "function",
                            "function": {
                                "name": call.name,
                                "arguments": serde_json::to_string(&call.arguments).unwrap_or_else(|_| "{}".into()),
                            }
                        })
                    })
                    .collect();
                vec![serde_json::json!({
                    "role": "assistant",
                    "content": if text.is_empty() { serde_json::Value::Null } else { serde_json::json!(text) },
                    "tool_calls": tool_calls_json,
                })]
            }
            ToolMessage::ToolResults(results) => results
                .iter()
                .map(|result| {
                    serde_json::json!({
                        "role": "tool",
                        "tool_call_id": result.call_id,
                        "content": result.content,
                    })
                })
                .collect(),
        }
    }

    fn anthropic_tool_messages(
        &self,
        message: &sqlly_ai::catalog::ToolMessage,
    ) -> Vec<serde_json::Value> {
        use sqlly_ai::catalog::ToolMessage;
        match message {
            ToolMessage::User(text) => vec![serde_json::json!({
                "role": "user",
                "content": text,
            })],
            ToolMessage::Assistant { text, tool_calls } => {
                let mut blocks = Vec::new();
                if !text.is_empty() {
                    blocks.push(serde_json::json!({"type": "text", "text": text}));
                }
                for call in tool_calls {
                    blocks.push(serde_json::json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": call.arguments,
                    }));
                }
                vec![serde_json::json!({
                    "role": "assistant",
                    "content": blocks,
                })]
            }
            ToolMessage::ToolResults(results) => {
                let blocks: Vec<serde_json::Value> = results
                    .iter()
                    .map(|result| {
                        serde_json::json!({
                            "type": "tool_result",
                            "tool_use_id": result.call_id,
                            "content": result.content,
                            "is_error": result.is_error,
                        })
                    })
                    .collect();
                vec![serde_json::json!({
                    "role": "user",
                    "content": blocks,
                })]
            }
            ToolMessage::System(_) => vec![],
        }
    }

    pub(crate) fn parse_tool_completion(
        &self,
        value: &serde_json::Value,
    ) -> Result<sqlly_ai::catalog::ToolCompletion, sqlly_ai::catalog::LlmError> {
        match self.spec.wire {
            CloudWire::OpenAiChat => {
                let message = value.pointer("/choices/0/message").ok_or_else(|| {
                    sqlly_ai::catalog::LlmError::Other("cloud answer missing message".into())
                })?;
                let text = message
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let tool_calls = message
                    .get("tool_calls")
                    .and_then(|v| v.as_array())
                    .map(|calls| {
                        calls
                            .iter()
                            .filter_map(|call| {
                                let id = call.get("id")?.as_str()?.to_string();
                                let function = call.get("function")?;
                                let name = function.get("name")?.as_str()?.to_string();
                                let args_str = function.get("arguments")?.as_str()?;
                                let arguments = serde_json::from_str(args_str)
                                    .unwrap_or_else(|_| serde_json::json!({ "_raw": args_str }));
                                Some(sqlly_ai::catalog::ToolCall {
                                    id,
                                    name,
                                    arguments,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(sqlly_ai::catalog::ToolCompletion {
                    text,
                    tool_calls,
                    model: self.spec.model.clone(),
                    provider: self.spec.provider_id.clone(),
                })
            }
            CloudWire::AnthropicMessages => {
                let blocks = value
                    .get("content")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut text_parts = Vec::new();
                let mut tool_calls = Vec::new();
                for block in blocks {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                text_parts.push(text.to_string());
                            }
                        }
                        Some("tool_use") => {
                            tool_calls.push(sqlly_ai::catalog::ToolCall {
                                id: block
                                    .get("id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                name: block
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default()
                                    .to_string(),
                                arguments: block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::json!({})),
                            });
                        }
                        _ => {}
                    }
                }
                Ok(sqlly_ai::catalog::ToolCompletion {
                    text: text_parts.join("\n"),
                    tool_calls,
                    model: self.spec.model.clone(),
                    provider: self.spec.provider_id.clone(),
                })
            }
            CloudWire::GeminiGenerateContent => {
                let text = self.extract_text(value).unwrap_or_default();
                Ok(sqlly_ai::catalog::ToolCompletion {
                    text,
                    tool_calls: vec![],
                    model: self.spec.model.clone(),
                    provider: self.spec.provider_id.clone(),
                })
            }
        }
    }
}

#[async_trait::async_trait]
impl sqlly_ai::catalog::LlmProvider for CloudLlmProvider {
    async fn complete(
        &self,
        request: sqlly_ai::catalog::CompletionRequest,
    ) -> Result<sqlly_ai::catalog::CompletionResponse, sqlly_ai::catalog::LlmError> {
        let body = self.body(&request);
        let value = self.post_json(body).await?;
        let answer = self.extract_text(&value).ok_or_else(|| {
            sqlly_ai::catalog::LlmError::Other(
                "cloud provider answer carried no message text".to_string(),
            )
        })?;
        Ok(sqlly_ai::catalog::CompletionResponse {
            text: answer,
            model: self.spec.model.clone(),
            provider: self.spec.provider_id.clone(),
            meta: serde_json::Value::Null,
        })
    }

    fn supports_native_tools(&self) -> bool {
        matches!(
            self.spec.wire,
            CloudWire::OpenAiChat | CloudWire::AnthropicMessages
        )
    }

    async fn complete_with_tools(
        &self,
        request: sqlly_ai::catalog::ToolCompletionRequest,
    ) -> Result<sqlly_ai::catalog::ToolCompletion, sqlly_ai::catalog::LlmError> {
        if !self.supports_native_tools() {
            return sqlly_ai::catalog::LlmProvider::complete_with_tools(self, request).await;
        }
        let body = self.tool_body(&request);
        let value = self.post_json(body).await?;
        self.parse_tool_completion(&value)
    }

    fn provider_id(&self) -> &str {
        &self.spec.provider_id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_provider_requires_model_and_keeps_identity() {
        let spec = CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![("authorization".into(), "Bearer k".into())],
            model: " ".into(),
            provider_id: "openai:gpt".into(),
            reasoning: CloudReasoning::None,
        };
        assert!(CloudLlmProvider::new(spec).is_err());
        let spec = CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://openrouter.ai/api/v1/chat/completions".into(),
            headers: vec![("authorization".into(), "Bearer k".into())],
            model: "meta/llama".into(),
            provider_id: "openrouter:meta/llama".into(),
            reasoning: CloudReasoning::None,
        };
        let provider = CloudLlmProvider::new(spec).unwrap();
        use sqlly_ai::catalog::LlmProvider as _;
        assert_eq!(provider.provider_id(), "openrouter:meta/llama");
    }

    #[test]
    fn cloud_bodies_match_each_wire_shape() {
        use sqlly_ai::catalog::{ChatMessage, ChatRole, CompletionRequest};
        let request = CompletionRequest {
            messages: vec![
                ChatMessage {
                    role: ChatRole::System,
                    content: "be terse".into(),
                },
                ChatMessage {
                    role: ChatRole::User,
                    content: "hello".into(),
                },
            ],
            model: None,
            capability: None,
            max_tokens: Some(64),
            temperature: Some(0.2),
            metadata: serde_json::Value::Null,
        };
        let openai = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "gpt-4o".into(),
            provider_id: "openai:gpt-4o".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let body = openai.build_body(&request);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["max_tokens"], 64);

        let anthropic = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::AnthropicMessages,
            endpoint: "https://api.anthropic.com/v1/messages".into(),
            headers: vec![],
            model: "claude".into(),
            provider_id: "anthropic:claude".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let body = anthropic.build_body(&request);
        assert_eq!(body["system"], "be terse");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cloud_answers_parse_per_provider() {
        let openai = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "m".into(),
            provider_id: "openai:m".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert_eq!(openai.extract_response_text(&value).as_deref(), Some("hi"));

        let anthropic = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::AnthropicMessages,
            endpoint: "https://api.anthropic.com/v1/messages".into(),
            headers: vec![],
            model: "m".into(),
            provider_id: "anthropic:m".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_str(r#"{"content":[{"type":"text","text":"hello"}]}"#).unwrap();
        assert_eq!(
            anthropic.extract_response_text(&value).as_deref(),
            Some("hello")
        );
        assert_eq!(
            anthropic.extract_response_text(&serde_json::json!({})),
            None
        );
    }

    #[test]
    fn explicit_local_provider_requires_a_model() {
        let error = match build_local_llm_provider("ollama", "  ") {
            Ok(_) => panic!("blank model unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.contains("Choose a model"));
    }

    #[test]
    fn explicit_local_provider_rejects_unknown_provider() {
        let error = match build_local_llm_provider("other", "model") {
            Ok(_) => panic!("unknown provider unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(error.contains("Unsupported local AI provider"));
    }

    #[test]
    fn explicit_ollama_provider_keeps_selected_model_in_identity() {
        let provider = build_local_llm_provider("ollama", "qwen-test").unwrap();
        assert_eq!(provider.provider_id(), "ollama:qwen-test");
    }

    #[test]
    fn openai_tool_body_includes_tools_and_tool_results() {
        use sqlly_ai::catalog::{
            ToolCall, ToolCompletionRequest, ToolMessage, ToolResult, ToolSpec,
        };
        let provider = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "gpt".into(),
            provider_id: "openai:gpt".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let request = ToolCompletionRequest {
            messages: vec![
                ToolMessage::System("sys".into()),
                ToolMessage::Assistant {
                    text: "hi".into(),
                    tool_calls: vec![ToolCall {
                        id: "call-1".into(),
                        name: "list_tables".into(),
                        arguments: serde_json::json!({}),
                    }],
                },
                ToolMessage::ToolResults(vec![ToolResult {
                    call_id: "call-1".into(),
                    content: "t".into(),
                    is_error: false,
                }]),
            ],
            tools: vec![ToolSpec {
                name: "list_tables".into(),
                description: "list".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            model: None,
            max_tokens: Some(64),
            temperature: None,
        };
        let body = provider.tool_body(&request);
        assert!(body.get("tools").is_some());
        assert_eq!(body["tool_choice"], "auto");
        let messages = body["messages"].as_array().unwrap();
        assert!(messages.iter().any(|m| m.get("tool_calls").is_some()));
        assert!(messages.iter().any(|m| m["role"] == "tool"));
    }

    #[test]
    fn anthropic_tool_body_includes_tools_and_tool_result_blocks() {
        use sqlly_ai::catalog::{
            ToolCall, ToolCompletionRequest, ToolMessage, ToolResult, ToolSpec,
        };
        let provider = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::AnthropicMessages,
            endpoint: "https://api.anthropic.com/v1/messages".into(),
            headers: vec![],
            model: "claude".into(),
            provider_id: "claude:claude".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let request = ToolCompletionRequest {
            messages: vec![ToolMessage::Assistant {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "tu_1".into(),
                    name: "get_columns".into(),
                    arguments: serde_json::json!({"table": "t"}),
                }],
            }],
            tools: vec![ToolSpec {
                name: "get_columns".into(),
                description: "cols".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            model: None,
            max_tokens: Some(64),
            temperature: None,
        };
        let body = provider.tool_body(&request);
        assert!(body.get("tools").is_some());
        assert_eq!(body["tool_choice"]["disable_parallel_tool_use"], true);
        let content = &body["messages"][0]["content"];
        assert!(
            content
                .as_array()
                .unwrap()
                .iter()
                .any(|b| b["type"] == "tool_use")
        );

        let results = ToolCompletionRequest {
            messages: vec![ToolMessage::ToolResults(vec![ToolResult {
                call_id: "tu_1".into(),
                content: "id int".into(),
                is_error: false,
            }])],
            tools: vec![],
            model: None,
            max_tokens: None,
            temperature: None,
        };
        let body = provider.tool_body(&results);
        let block = &body["messages"][0]["content"][0];
        assert_eq!(block["type"], "tool_result");
    }

    #[test]
    fn openai_tool_response_parses_tool_calls() {
        let provider = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "gpt".into(),
            provider_id: "openai:gpt".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(
            r#"{"choices":[{"message":{"content":null,"tool_calls":[{"id":"c1","type":"function","function":{"name":"list_tables","arguments":"{}"}}]}}]}"#,
        )
        .unwrap();
        let parsed = provider.parse_tool_completion(&value).unwrap();
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "list_tables");
    }

    #[test]
    fn openai_tool_response_parses_bad_arguments_as_raw() {
        let provider = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "gpt".into(),
            provider_id: "openai:gpt".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(
            r#"{"choices":[{"message":{"tool_calls":[{"id":"c1","type":"function","function":{"name":"x","arguments":"not-json"}}]}}]}"#,
        )
        .unwrap();
        let parsed = provider.parse_tool_completion(&value).unwrap();
        assert_eq!(parsed.tool_calls[0].arguments["_raw"], "not-json");
    }

    #[test]
    fn anthropic_tool_response_parses_tool_use_blocks() {
        let provider = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::AnthropicMessages,
            endpoint: "https://api.anthropic.com/v1/messages".into(),
            headers: vec![],
            model: "claude".into(),
            provider_id: "claude:claude".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(
            r#"{"content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"tu","name":"list_tables","input":{}}]}"#,
        )
        .unwrap();
        let parsed = provider.parse_tool_completion(&value).unwrap();
        assert_eq!(parsed.text, "ok");
        assert_eq!(parsed.tool_calls[0].id, "tu");
    }

    #[test]
    fn cloud_provider_supports_native_tools_for_openai_and_anthropic_only() {
        use sqlly_ai::catalog::LlmProvider as _;
        let openai = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::OpenAiChat,
            endpoint: "https://api.openai.com/v1/chat/completions".into(),
            headers: vec![],
            model: "gpt".into(),
            provider_id: "openai:gpt".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        assert!(openai.supports_native_tools());
        let gemini = CloudLlmProvider::new(CloudProviderSpec {
            wire: CloudWire::GeminiGenerateContent,
            endpoint: "https://generativelanguage.googleapis.com/v1beta/models/gpt:generateContent"
                .into(),
            headers: vec![],
            model: "gemini".into(),
            provider_id: "gemini:gemini".into(),
            reasoning: CloudReasoning::None,
        })
        .unwrap();
        assert!(!gemini.supports_native_tools());
    }
}
