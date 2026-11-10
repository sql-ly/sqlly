//! Cloud AI provider types — model listing and API configuration.
//!
//! Port of `CloudAIModelsClient.swift` + the `CloudAIProvider` enum from
//! `QueryAISummaryPreferences.swift`. The pure types (provider enum with
//! URLs/headers/display names, error messages, response struct) are here;
//! the HTTP transport layer (URLSession / reqwest) is wired separately.

use std::collections::BTreeMap;

/// Which cloud AI provider to use for model listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CloudAIProvider {
    OpenRouter,
    OpenAI,
    Claude,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProviderKind {
    OpenRouter,
    OpenAi,
    Anthropic,
    Gemini,
    DeepSeek,
    OpenAiCompatible,
    AnthropicCompatible,
    /// Cheaper Inference: an OpenAI-compatible gateway (Gridex #97).
    CheaperInference,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReasoningLevel {
    #[default]
    Off,
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderProfile {
    pub id: String,
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: String,
    pub model: String,
    pub api_key_ref: String,
    #[serde(default)]
    pub available_model_ids: Vec<String>,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub reasoning: ReasoningLevel,
    #[serde(default)]
    pub thinking_budget: Option<u32>,
    #[serde(default)]
    pub send_reasoning_effort: bool,
    #[serde(default)]
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AiProfilesConfig {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<ProviderProfile>,
    #[serde(default)]
    pub active_profile_id: Option<String>,
}

impl ProviderKind {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Gemini => "Gemini",
            Self::DeepSeek => "DeepSeek",
            Self::OpenAiCompatible => "OpenAI-compatible",
            Self::AnthropicCompatible => "Anthropic-compatible",
            Self::CheaperInference => "Cheaper Inference",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
            Self::Gemini => "https://generativelanguage.googleapis.com",
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::OpenAiCompatible => "http://localhost:1234/v1",
            Self::AnthropicCompatible => "http://localhost:1234",
            Self::CheaperInference => "https://api.cheaperinference.com/v1",
        }
    }

    /// Model a new profile of this kind starts with. Empty means the user
    /// fetches or types one.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::CheaperInference => "gpt-5.4-mini",
            _ => "",
        }
    }

    /// Model ids offered before the first successful model fetch.
    pub fn fallback_model_ids(self) -> &'static [&'static str] {
        match self {
            Self::CheaperInference => &["gpt-5.4-mini"],
            _ => &[],
        }
    }

    pub fn all() -> &'static [ProviderKind] {
        &[
            Self::OpenRouter,
            Self::OpenAi,
            Self::Anthropic,
            Self::Gemini,
            Self::DeepSeek,
            Self::OpenAiCompatible,
            Self::AnthropicCompatible,
            Self::CheaperInference,
        ]
    }

    pub fn supports_model_list(self) -> bool {
        true
    }

    pub fn reasoning_note(self) -> Option<String> {
        match self {
            Self::DeepSeek => Some(crate::localization::text(
                "settings.ai.cloud.deepseek_reasoning",
            )),
            Self::OpenAiCompatible | Self::CheaperInference => Some(crate::localization::text(
                "settings.ai.cloud.openai_compatible_reasoning",
            )),
            _ => None,
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn wire(self) -> sqlly_services::providers::CloudWire {
        use sqlly_services::providers::CloudWire;
        match self {
            Self::OpenRouter
            | Self::OpenAi
            | Self::DeepSeek
            | Self::OpenAiCompatible
            | Self::CheaperInference => CloudWire::OpenAiChat,
            Self::Anthropic | Self::AnthropicCompatible => CloudWire::AnthropicMessages,
            Self::Gemini => CloudWire::GeminiGenerateContent,
        }
    }

    pub fn id_slug(self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenAi => "openai",
            Self::Anthropic => "anthropic",
            Self::Gemini => "gemini",
            Self::DeepSeek => "deepseek",
            Self::OpenAiCompatible => "openai-compatible",
            Self::AnthropicCompatible => "anthropic-compatible",
            Self::CheaperInference => "cheaper-inference",
        }
    }
}

impl ProviderProfile {
    pub fn new(kind: ProviderKind) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        Self {
            id: id.clone(),
            name: kind.display_name().to_string(),
            kind,
            base_url: kind.default_base_url().to_string(),
            model: kind.default_model().to_string(),
            api_key_ref: format!("ai-profile.{id}"),
            available_model_ids: kind
                .fallback_model_ids()
                .iter()
                .map(|id| id.to_string())
                .collect(),
            context_window: None,
            reasoning: ReasoningLevel::default(),
            thinking_budget: None,
            send_reasoning_effort: false,
            proxy: None,
        }
    }
}

fn trim_base_url(base_url: &str) -> String {
    base_url.trim_end_matches('/').to_string()
}

pub fn host_from_base_url(base_url: &str) -> String {
    let trimmed = base_url.trim();
    if let Some(rest) = trimmed.strip_prefix("http://") {
        rest.split('/').next().unwrap_or(rest).to_string()
    } else if let Some(rest) = trimmed.strip_prefix("https://") {
        rest.split('/').next().unwrap_or(rest).to_string()
    } else {
        trimmed.split('/').next().unwrap_or(trimmed).to_string()
    }
}

fn is_local_host(host: &str) -> bool {
    let host_lower = host.to_ascii_lowercase();
    let bare = host_lower.split(':').next().unwrap_or(&host_lower);
    bare == "localhost" || bare == "127.0.0.1" || bare == "::1" || bare.ends_with(".local")
}

pub fn egress_label(profile: &ProviderProfile) -> String {
    let host = host_from_base_url(&profile.base_url);
    let key = if is_local_host(&host) {
        "settings.ai.cloud.egress.local_detail"
    } else {
        "settings.ai.cloud.egress.cloud_detail"
    };
    crate::localization::format(key, &[("host", &host)])
}

pub fn egress_short(profile: &ProviderProfile) -> String {
    let host = host_from_base_url(&profile.base_url);
    if is_local_host(&host) {
        crate::localization::text("settings.ai.cloud.egress.local")
    } else {
        crate::localization::text("settings.ai.cloud.egress.cloud")
    }
}

pub fn models_endpoint(kind: &ProviderKind, base_url: &str) -> Option<String> {
    let base = trim_base_url(base_url);
    Some(match kind {
        ProviderKind::OpenRouter
        | ProviderKind::OpenAi
        | ProviderKind::DeepSeek
        | ProviderKind::OpenAiCompatible
        | ProviderKind::CheaperInference => format!("{base}/models"),
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => format!("{base}/v1/models"),
        ProviderKind::Gemini => format!("{base}/v1beta/models"),
    })
}

pub fn chat_endpoint(kind: &ProviderKind, base_url: &str, model: &str) -> Result<String, String> {
    let base = trim_base_url(base_url);
    Ok(match kind {
        ProviderKind::OpenRouter
        | ProviderKind::OpenAi
        | ProviderKind::DeepSeek
        | ProviderKind::OpenAiCompatible
        | ProviderKind::CheaperInference => format!("{base}/chat/completions"),
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => {
            format!("{base}/v1/messages")
        }
        ProviderKind::Gemini => format!("{base}/v1beta/models/{model}:generateContent"),
    })
}

pub fn auth_headers(kind: &ProviderKind, api_key: &str) -> Vec<(&'static str, String)> {
    match kind {
        ProviderKind::OpenRouter => vec![
            ("Authorization", format!("Bearer {api_key}")),
            ("HTTP-Referer", "https://sqlly.app".to_string()),
            ("X-Title", "SQLLY".to_string()),
        ],
        ProviderKind::OpenAi | ProviderKind::DeepSeek | ProviderKind::CheaperInference => {
            vec![("Authorization", format!("Bearer {api_key}"))]
        }
        ProviderKind::OpenAiCompatible => {
            if api_key.trim().is_empty() {
                Vec::new()
            } else {
                vec![("Authorization", format!("Bearer {api_key}"))]
            }
        }
        ProviderKind::Anthropic | ProviderKind::AnthropicCompatible => vec![
            ("x-api-key", api_key.to_string()),
            ("anthropic-version", "2023-06-01".to_string()),
        ],
        ProviderKind::Gemini => vec![("x-goog-api-key", api_key.to_string())],
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn reasoning_for(profile: &ProviderProfile) -> sqlly_services::providers::CloudReasoning {
    use sqlly_services::providers::CloudReasoning;
    if profile.reasoning == ReasoningLevel::Off {
        return CloudReasoning::None;
    }
    let effort = match profile.reasoning {
        ReasoningLevel::Off => unreachable!(),
        ReasoningLevel::Low => "low",
        ReasoningLevel::Medium => "medium",
        ReasoningLevel::High => "high",
    };
    match profile.kind {
        ProviderKind::OpenRouter | ProviderKind::OpenAi => CloudReasoning::OpenAiEffort(effort),
        ProviderKind::DeepSeek => CloudReasoning::None,
        ProviderKind::OpenAiCompatible | ProviderKind::CheaperInference => {
            if profile.send_reasoning_effort {
                CloudReasoning::OpenAiEffort(effort)
            } else {
                CloudReasoning::None
            }
        }
        ProviderKind::Anthropic => CloudReasoning::AnthropicEffort(effort),
        ProviderKind::AnthropicCompatible => {
            if let Some(budget) = profile.thinking_budget {
                CloudReasoning::AnthropicBudget(budget)
            } else {
                CloudReasoning::AnthropicEffort(effort)
            }
        }
        ProviderKind::Gemini => {
            if let Some(budget) = profile.thinking_budget {
                CloudReasoning::GeminiBudget(budget as i32)
            } else {
                let level = if profile.reasoning == ReasoningLevel::Low {
                    "low"
                } else {
                    "high"
                };
                CloudReasoning::GeminiLevel(level)
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn cloud_provider_spec(
    profile: &ProviderProfile,
    api_key: String,
) -> Result<sqlly_services::providers::CloudProviderSpec, String> {
    let endpoint = chat_endpoint(&profile.kind, &profile.base_url, &profile.model)?;
    let headers = auth_headers(&profile.kind, &api_key)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    Ok(sqlly_services::providers::CloudProviderSpec {
        wire: profile.kind.wire(),
        endpoint,
        headers,
        model: profile.model.clone(),
        provider_id: format!("{}:{}", profile.kind.id_slug(), profile.model),
        reasoning: reasoning_for(profile),
    })
}

impl CloudAIProvider {
    pub fn all() -> &'static [CloudAIProvider] {
        &[Self::OpenRouter, Self::OpenAI, Self::Claude]
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::OpenRouter => "OpenRouter",
            Self::OpenAI => "OpenAI",
            Self::Claude => "Claude",
        }
    }

    pub fn models_url(self) -> &'static str {
        match self {
            Self::OpenRouter => "https://openrouter.ai/api/v1/models",
            Self::OpenAI => "https://api.openai.com/v1/models",
            Self::Claude => "https://api.anthropic.com/v1/models",
        }
    }

    pub fn keychain_account(self) -> String {
        format!("ai-provider.{}-api-key", self.display_name().to_lowercase())
    }

    /// HTTP headers for the models-list request, given an API key.
    pub fn headers(self, api_key: &str) -> Vec<(&'static str, String)> {
        match self {
            Self::OpenRouter => vec![
                ("Authorization", format!("Bearer {}", api_key)),
                ("HTTP-Referer", "https://sqlly.app".to_string()),
                ("X-Title", "SQLLY".to_string()),
            ],
            Self::OpenAI => vec![("Authorization", format!("Bearer {}", api_key))],
            Self::Claude => vec![
                ("x-api-key", api_key.to_string()),
                ("anthropic-version", "2023-06-01".to_string()),
            ],
        }
    }
}

/// Error from the cloud-AI models fetch. The `error_description()` mirrors
/// the Swift `LocalizedError.errorDescription` strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CloudAIModelsError {
    Network,
    HttpStatus {
        provider_name: String,
        status_code: u16,
    },
    InvalidResponse {
        provider_name: String,
    },
    /// Browser (wasm) build only: cloud AI is disabled in the browser preview.
    #[cfg(target_arch = "wasm32")]
    BrowserUnsupported,
}

impl std::fmt::Display for CloudAIModelsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.error_description())
    }
}

impl std::error::Error for CloudAIModelsError {}

impl CloudAIModelsError {
    pub fn error_description(&self) -> String {
        match self {
            Self::Network => crate::localization::text("settings.ai.cloud.error.network"),
            Self::HttpStatus {
                provider_name,
                status_code,
            } if *status_code == 401 || *status_code == 403 => crate::localization::format(
                "settings.ai.cloud.error.api_key_rejected",
                &[("provider_name", provider_name)],
            ),
            Self::HttpStatus {
                provider_name,
                status_code: 429,
            } => crate::localization::format(
                "settings.ai.cloud.error.rate_limited",
                &[("provider_name", provider_name)],
            ),
            Self::HttpStatus {
                provider_name,
                status_code,
            } => crate::localization::format(
                "settings.ai.cloud.error.http_status",
                &[
                    ("provider_name", provider_name),
                    ("status_code", &status_code.to_string()),
                ],
            ),
            #[cfg(target_arch = "wasm32")]
            Self::BrowserUnsupported => crate::browser_preview::unavailable(
                &crate::localization::text("settings.ai.cloud.error.browser_unavailable"),
            ),
            Self::InvalidResponse { provider_name } => crate::localization::format(
                "settings.ai.cloud.error.invalid_response",
                &[("provider_name", provider_name)],
            ),
        }
    }
}

/// Decoded JSON response from the models endpoint: `{ "data": [{ "id": "..." }] }`.
#[derive(Debug, serde::Deserialize)]
pub struct CloudModelsResponse {
    pub data: Vec<CloudModelEntry>,
}

#[derive(Debug, serde::Deserialize)]
pub struct CloudModelEntry {
    pub id: String,
}

#[derive(Debug, serde::Deserialize)]
struct GeminiModelsResponse {
    models: Vec<GeminiModelEntry>,
}

#[derive(Debug, serde::Deserialize)]
struct GeminiModelEntry {
    name: String,
    #[serde(default, rename = "supportedGenerationMethods")]
    supported_generation_methods: Vec<String>,
}

#[allow(dead_code)]
/// Per-provider preferences stored alongside the AI summary settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAIProviderPreferences {
    #[serde(default)]
    pub api_key_configured: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model_id: Option<String>,
    #[serde(default)]
    pub available_model_ids: Vec<String>,
}

#[allow(dead_code)]
/// One selectable model in the AI model picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AIModelSelectionOption {
    pub identifier: String,
    pub display_name: String,
}

/// Sorts model ids alphabetically (matching Swift's `.sorted()` on the
/// decoded array).
pub fn sort_model_ids(models: Vec<String>) -> Vec<String> {
    let mut v = models;
    v.sort();
    v
}

/// Parses a models-list response and returns the sorted model-id list.
pub fn parse_model_ids_for(
    kind: &ProviderKind,
    json: &[u8],
) -> Result<Vec<String>, CloudAIModelsError> {
    match kind {
        ProviderKind::Gemini => {
            let resp: GeminiModelsResponse =
                serde_json::from_slice(json).map_err(|_| CloudAIModelsError::InvalidResponse {
                    provider_name: kind.display_name().to_string(),
                })?;
            let ids: Vec<String> = resp
                .models
                .into_iter()
                .filter(|entry| {
                    entry
                        .supported_generation_methods
                        .iter()
                        .any(|method| method == "generateContent")
                })
                .map(|entry| {
                    entry
                        .name
                        .strip_prefix("models/")
                        .unwrap_or(&entry.name)
                        .to_string()
                })
                .collect();
            Ok(sort_model_ids(ids))
        }
        _ => {
            let resp: CloudModelsResponse =
                serde_json::from_slice(json).map_err(|_| CloudAIModelsError::InvalidResponse {
                    provider_name: kind.display_name().to_string(),
                })?;
            Ok(sort_model_ids(
                resp.data.into_iter().map(|m| m.id).collect(),
            ))
        }
    }
}

/// Parses a `CloudModelsResponse` from raw JSON bytes and returns the sorted
/// model-id list.
pub fn parse_model_ids(json: &[u8]) -> Result<Vec<String>, CloudAIModelsError> {
    parse_model_ids_for(&ProviderKind::OpenAi, json)
}

#[allow(dead_code)]
/// Builds the HTTP request headers as a `BTreeMap` for serialization.
pub fn header_map(provider: CloudAIProvider, api_key: &str) -> BTreeMap<String, String> {
    provider
        .headers(api_key)
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
}

/// Fetches the list of available model IDs for `profile` using `api_key`.
pub async fn fetch_models_for(
    profile: &ProviderProfile,
    api_key: &str,
) -> Result<Vec<String>, CloudAIModelsError> {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = (profile, api_key);
        Err(CloudAIModelsError::BrowserUnsupported)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        fetch_models_for_native(profile, api_key).await
    }
}

/// Fetches the list of available model IDs from `provider`'s models endpoint
/// using `api_key`. Legacy wrapper for migration tests.
pub async fn fetch_models(
    provider: CloudAIProvider,
    api_key: &str,
) -> Result<Vec<String>, CloudAIModelsError> {
    let kind = match provider {
        CloudAIProvider::OpenRouter => ProviderKind::OpenRouter,
        CloudAIProvider::OpenAI => ProviderKind::OpenAi,
        CloudAIProvider::Claude => ProviderKind::Anthropic,
    };
    let profile = ProviderProfile {
        id: "legacy".into(),
        name: provider.display_name().to_string(),
        kind,
        base_url: kind.default_base_url().to_string(),
        model: String::new(),
        api_key_ref: provider.keychain_account(),
        available_model_ids: Vec::new(),
        context_window: None,
        reasoning: ReasoningLevel::Off,
        thinking_budget: None,
        send_reasoning_effort: false,
        proxy: None,
    };
    fetch_models_for(&profile, api_key).await
}

/// Measures round-trip latency to the profile's models endpoint.
pub async fn test_latency(
    profile: &ProviderProfile,
    api_key: &str,
) -> Result<std::time::Duration, CloudAIModelsError> {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = (profile, api_key);
        Err(CloudAIModelsError::BrowserUnsupported)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        test_latency_native(profile, api_key).await
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn fetch_models_for_native(
    profile: &ProviderProfile,
    api_key: &str,
) -> Result<Vec<String>, CloudAIModelsError> {
    let provider_name = profile.name.clone();
    let url = models_endpoint(&profile.kind, &profile.base_url).ok_or(
        CloudAIModelsError::InvalidResponse {
            provider_name: provider_name.clone(),
        },
    )?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|_| CloudAIModelsError::Network)?;

    let mut req = client.get(&url);
    for (key, val) in auth_headers(&profile.kind, api_key) {
        req = req.header(key, val);
    }

    let resp = req.send().await.map_err(|_| CloudAIModelsError::Network)?;

    let status = resp.status().as_u16();
    if status != 200 {
        return Err(CloudAIModelsError::HttpStatus {
            provider_name,
            status_code: status,
        });
    }

    let body = resp
        .bytes()
        .await
        .map_err(|_| CloudAIModelsError::InvalidResponse {
            provider_name: profile.name.clone(),
        })?;

    parse_model_ids_for(&profile.kind, &body).map_err(|_| CloudAIModelsError::InvalidResponse {
        provider_name: profile.name.clone(),
    })
}

#[cfg(not(target_arch = "wasm32"))]
async fn test_latency_native(
    profile: &ProviderProfile,
    api_key: &str,
) -> Result<std::time::Duration, CloudAIModelsError> {
    let provider_name = profile.name.clone();
    let url = models_endpoint(&profile.kind, &profile.base_url).ok_or(
        CloudAIModelsError::InvalidResponse {
            provider_name: provider_name.clone(),
        },
    )?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|_| CloudAIModelsError::Network)?;

    let started = std::time::Instant::now();
    let mut req = client.get(&url);
    for (key, val) in auth_headers(&profile.kind, api_key) {
        req = req.header(key, val);
    }

    match req.send().await {
        Ok(_) => Ok(started.elapsed()),
        Err(_) => Err(CloudAIModelsError::Network),
    }
}

// ============================================================================
// Cloud AI configuration persistence
//
// The split is a security boundary, not an organizational one: API keys are
// bearer credentials for a billable account, so they live in the OS keychain
// (via `keyring`, the same crate and error conventions as
// `crate::sql_credentials`), and everything else lives in a plain JSON file
// next to the other preferences. Nothing in this module may ever write a key
// to that file — see the `#[serde(skip)]` on `api_key`.
// ============================================================================

/// Keychain service for cloud-AI provider keys. Distinct from
/// `sql_credentials`' `SQLLY.sql-auth` so the two credential classes can be
/// revoked independently and never collide on an account name.
const KEYCHAIN_SERVICE: &str = crate::distribution::keychain_service!("SQLLY.cloud-ai");

fn cloud_ai_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("cloud-ai.json");
    p
}

pub fn ai_profiles_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("ai-profiles.json");
    p
}

#[cfg(not(target_arch = "wasm32"))]
fn key_entry_ref(account: &str) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, account).map_err(|error| error.to_string())
}

pub fn load_api_key_ref(account: &str) -> Result<Option<String>, String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match key_entry_ref(account)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::get(KEYCHAIN_SERVICE, account)
    }
}

pub fn store_api_key_ref(account: &str, api_key: &str) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        key_entry_ref(account)?
            .set_password(api_key)
            .map_err(|error| error.to_string())
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::set(KEYCHAIN_SERVICE, account, api_key)
    }
}

pub fn delete_api_key_ref(account: &str) -> Result<(), String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        match key_entry_ref(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::secrets::delete(KEYCHAIN_SERVICE, account)
    }
}

pub fn keychain_key_present_ref(account: &str) -> bool {
    matches!(load_api_key_ref(account), Ok(Some(key)) if !key.is_empty())
}

pub fn migrate_legacy(config: &CloudAIConfig) -> AiProfilesConfig {
    let legacy = [
        (
            "legacy-openrouter",
            ProviderKind::OpenRouter,
            CloudAIProvider::OpenRouter,
            &config.openrouter,
        ),
        (
            "legacy-openai",
            ProviderKind::OpenAi,
            CloudAIProvider::OpenAI,
            &config.openai,
        ),
        (
            "legacy-claude",
            ProviderKind::Anthropic,
            CloudAIProvider::Claude,
            &config.claude,
        ),
    ];
    let profiles: Vec<ProviderProfile> = legacy
        .into_iter()
        .map(|(id, kind, provider, cfg)| ProviderProfile {
            id: id.to_string(),
            name: kind.display_name().to_string(),
            kind,
            base_url: kind.default_base_url().to_string(),
            model: cfg.selected_model_id.clone().unwrap_or_default(),
            api_key_ref: provider.keychain_account(),
            available_model_ids: cfg.available_model_ids.clone(),
            context_window: None,
            reasoning: ReasoningLevel::Off,
            thinking_budget: None,
            send_reasoning_effort: false,
            proxy: None,
        })
        .collect();
    let active_profile_id = config.app_ai_provider.map(|provider| match provider {
        CloudAIProvider::OpenRouter => "legacy-openrouter".to_string(),
        CloudAIProvider::OpenAI => "legacy-openai".to_string(),
        CloudAIProvider::Claude => "legacy-claude".to_string(),
    });
    AiProfilesConfig {
        version: 2,
        profiles,
        active_profile_id,
    }
}

pub fn load_ai_profiles() -> AiProfilesConfig {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(text) = crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "ai-profiles.json",
        ) && let Ok(config) = serde_json::from_str(&text)
        {
            return config;
        }
        if crate::host_storage::load_text(crate::host_storage::StorageArea::Data, "cloud-ai.json")
            .is_some()
        {
            let migrated = migrate_legacy(&load_cloud_ai_config());
            let _ = save_ai_profiles(&migrated);
            return migrated;
        }
        return AiProfilesConfig {
            version: 2,
            ..Default::default()
        };
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = ai_profiles_path();
        if path.exists()
            && let Ok(data) = std::fs::read(&path)
            && let Ok(config) = serde_json::from_slice(&data)
        {
            return config;
        }
        if cloud_ai_path().exists() {
            let migrated = migrate_legacy(&load_cloud_ai_config());
            let _ = save_ai_profiles(&migrated);
            return migrated;
        }
        AiProfilesConfig {
            version: 2,
            ..Default::default()
        }
    }
}

pub fn save_ai_profiles(config: &AiProfilesConfig) -> Result<(), String> {
    let text = serde_json::to_string_pretty(config).map_err(|error| error.to_string())?;
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "ai-profiles.json",
            &text,
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::write(ai_profiles_path(), text).map_err(|error| error.to_string())
    }
}

/// Per-provider settings. `api_key` is deliberately **not** serialized: it is
/// a runtime-only field hydrated from the keychain by
/// [`load_cloud_ai_config`]. Keys are changed only through [`store_api_key`]
/// and [`delete_api_key`]; saving this config never touches the keychain. If
/// you add a field here, assume it lands on disk in plaintext.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudAIProviderConfig {
    #[serde(skip)]
    pub api_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_model_id: Option<String>,
    #[serde(default)]
    pub available_model_ids: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CloudAIConfig {
    #[serde(default)]
    pub openrouter: CloudAIProviderConfig,
    #[serde(default)]
    pub openai: CloudAIProviderConfig,
    #[serde(default)]
    pub claude: CloudAIProviderConfig,
    /// When set, Ask, Generate, and schema-card cache route through this
    /// cloud provider's selected model instead of the local runtime. Query
    /// summaries, AI review, and FIM stay on Ollama. `None` (the default)
    /// keeps catalog LLM calls on-device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_ai_provider: Option<CloudAIProvider>,
}

impl CloudAIConfig {
    pub fn provider(&self, provider: CloudAIProvider) -> &CloudAIProviderConfig {
        match provider {
            CloudAIProvider::OpenRouter => &self.openrouter,
            CloudAIProvider::OpenAI => &self.openai,
            CloudAIProvider::Claude => &self.claude,
        }
    }

    pub fn provider_mut(&mut self, provider: CloudAIProvider) -> &mut CloudAIProviderConfig {
        match provider {
            CloudAIProvider::OpenRouter => &mut self.openrouter,
            CloudAIProvider::OpenAI => &mut self.openai,
            CloudAIProvider::Claude => &mut self.claude,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(dead_code)]
fn key_entry(provider: CloudAIProvider) -> Result<keyring::Entry, String> {
    key_entry_ref(&provider.keychain_account())
}

/// `Ok(None)` means "no key stored"; `Err` means the keychain itself failed
/// (locked, access denied) and is NOT the same as absence — callers must not
/// collapse the two or they will silently offer to overwrite a key that is
/// actually there.
#[cfg(not(target_arch = "wasm32"))]
pub fn load_api_key(provider: CloudAIProvider) -> Result<Option<String>, String> {
    load_api_key_ref(&provider.keychain_account())
}

#[cfg(not(target_arch = "wasm32"))]
pub fn store_api_key(provider: CloudAIProvider, api_key: &str) -> Result<(), String> {
    store_api_key_ref(&provider.keychain_account(), api_key)
}

/// Deleting an absent entry succeeds — "no key" is the desired end state
/// either way, and the UI clearing an already-empty field must not error.
#[cfg(not(target_arch = "wasm32"))]
pub fn delete_api_key(provider: CloudAIProvider) -> Result<(), String> {
    delete_api_key_ref(&provider.keychain_account())
}

// Browser (wasm) build: API keys live in sessionStorage via
// `host_storage::secrets` (cleared when the tab closes). Cloud AI requests
// themselves are disabled in the browser, but a pasted key round-trips so
// the Preferences UI behaves consistently.
#[cfg(target_arch = "wasm32")]
pub fn load_api_key(provider: CloudAIProvider) -> Result<Option<String>, String> {
    load_api_key_ref(&provider.keychain_account())
}

#[cfg(target_arch = "wasm32")]
pub fn store_api_key(provider: CloudAIProvider, api_key: &str) -> Result<(), String> {
    store_api_key_ref(&provider.keychain_account(), api_key)
}

#[cfg(target_arch = "wasm32")]
pub fn delete_api_key(provider: CloudAIProvider) -> Result<(), String> {
    delete_api_key_ref(&provider.keychain_account())
}

/// Whether a key is actually in the keychain for `provider`. This is what the
/// UI should show "Saved to Keychain" from — the in-memory
/// [`CloudAIProviderConfig::api_key`] only reflects what was typed, not what
/// was durably stored.
///
/// A keychain failure reports `false` (nothing is known to be saved) rather
/// than guessing. This performs a real keychain read, so call it on load and
/// after a save — not from a render path.
// Unused until the Preferences UI replaces its hardcoded "persisted to the
// Keychain" claim with real status.
#[allow(dead_code)]
pub fn keychain_key_present(provider: CloudAIProvider) -> bool {
    matches!(load_api_key(provider), Ok(Some(key)) if !key.is_empty())
}

/// Non-secret settings come from `cloud-ai.json`, keys from the keychain.
/// Follows the preferences convention of degrading to defaults on any error:
/// a corrupt file or a denied keychain must not stop the app from opening.
pub fn load_cloud_ai_config() -> CloudAIConfig {
    #[cfg(target_arch = "wasm32")]
    let mut config: CloudAIConfig =
        crate::host_storage::load_text(crate::host_storage::StorageArea::Data, "cloud-ai.json")
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
    #[cfg(not(target_arch = "wasm32"))]
    let mut config: CloudAIConfig = match std::fs::read(cloud_ai_path()) {
        Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
        Err(_) => CloudAIConfig::default(),
    };
    for provider in CloudAIProvider::all() {
        if let Ok(Some(key)) = load_api_key(*provider) {
            config.provider_mut(*provider).api_key = key;
        }
    }
    config
}

fn serialize_cloud_ai_config(config: &CloudAIConfig) -> Result<String, String> {
    crate::config_version::to_string_pretty(config).map_err(|error| error.to_string())
}

/// Writes only non-secret provider settings. API keys have an intentionally
/// separate lifecycle: callers must use [`store_api_key`] or
/// [`delete_api_key`] for an explicit credential change. This separation means
/// refreshing a model list with an otherwise-default config can never revoke
/// an existing keychain entry.
pub fn save_cloud_ai_config(config: &CloudAIConfig) -> Result<(), String> {
    let text = serialize_cloud_ai_config(config)?;
    #[cfg(target_arch = "wasm32")]
    crate::host_storage::store_text(
        crate::host_storage::StorageArea::Data,
        "cloud-ai.json",
        &text,
    )?;
    #[cfg(not(target_arch = "wasm32"))]
    std::fs::write(cloud_ai_path(), text).map_err(|error| error.to_string())?;
    Ok(())
}

#[allow(dead_code)]
/// Builds `AIModelSelectionOption` entries from a sorted list of model IDs,
/// using the provider's display name as a prefix for disambiguation.
pub fn model_selection_options(
    provider: CloudAIProvider,
    model_ids: &[String],
) -> Vec<AIModelSelectionOption> {
    let name = provider.display_name();
    model_ids
        .iter()
        .map(|id| AIModelSelectionOption {
            identifier: format!("{name}:{id}"),
            display_name: id.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_all_covers_three() {
        assert_eq!(CloudAIProvider::all().len(), 3);
    }

    #[test]
    fn provider_display_names() {
        assert_eq!(CloudAIProvider::OpenRouter.display_name(), "OpenRouter");
        assert_eq!(CloudAIProvider::OpenAI.display_name(), "OpenAI");
        assert_eq!(CloudAIProvider::Claude.display_name(), "Claude");
    }

    #[test]
    fn provider_models_urls() {
        assert!(
            CloudAIProvider::OpenRouter
                .models_url()
                .contains("openrouter.ai")
        );
        assert!(CloudAIProvider::OpenAI.models_url().contains("openai.com"));
        assert!(
            CloudAIProvider::Claude
                .models_url()
                .contains("anthropic.com")
        );
    }

    #[test]
    fn provider_keychain_account() {
        assert_eq!(
            CloudAIProvider::OpenAI.keychain_account(),
            "ai-provider.openai-api-key"
        );
        assert_eq!(
            CloudAIProvider::Claude.keychain_account(),
            "ai-provider.claude-api-key"
        );
    }

    #[test]
    fn openrouter_headers_include_bearer_and_referer() {
        let h = CloudAIProvider::OpenRouter.headers("key123");
        assert_eq!(h.len(), 3);
        assert!(
            h.iter()
                .any(|(k, v)| *k == "Authorization" && v == "Bearer key123")
        );
        assert!(h.iter().any(|(k, _)| *k == "HTTP-Referer"));
        assert!(h.iter().any(|(k, _)| *k == "X-Title"));
    }

    #[test]
    fn openai_headers_just_bearer() {
        let h = CloudAIProvider::OpenAI.headers("k");
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].0, "Authorization");
    }

    #[test]
    fn claude_headers_use_api_key() {
        let h = CloudAIProvider::Claude.headers("k");
        assert_eq!(h.len(), 2);
        assert!(h.iter().any(|(k, v)| *k == "x-api-key" && v == "k"));
        assert!(
            h.iter()
                .any(|(k, v)| *k == "anthropic-version" && v == "2023-06-01")
        );
    }

    #[test]
    fn error_network_description() {
        assert!(
            CloudAIModelsError::Network
                .error_description()
                .contains("Could not reach")
        );
    }

    #[test]
    fn error_401_says_rejected_key() {
        let e = CloudAIModelsError::HttpStatus {
            provider_name: "OpenAI".into(),
            status_code: 401,
        };
        assert!(e.error_description().contains("rejected the API key"));
    }

    #[test]
    fn error_403_says_rejected_key() {
        let e = CloudAIModelsError::HttpStatus {
            provider_name: "Claude".into(),
            status_code: 403,
        };
        assert!(e.error_description().contains("rejected the API key"));
    }

    #[test]
    fn error_429_says_rate_limiting() {
        let e = CloudAIModelsError::HttpStatus {
            provider_name: "OpenRouter".into(),
            status_code: 429,
        };
        assert!(e.error_description().contains("rate-limiting"));
    }

    #[test]
    fn error_500_says_http_status() {
        let e = CloudAIModelsError::HttpStatus {
            provider_name: "OpenAI".into(),
            status_code: 500,
        };
        assert!(e.error_description().contains("HTTP 500"));
    }

    #[test]
    fn error_invalid_response_mentions_provider() {
        let e = CloudAIModelsError::InvalidResponse {
            provider_name: "Claude".into(),
        };
        assert!(e.error_description().contains("Claude"));
        assert!(e.error_description().contains("unexpected model list"));
    }

    #[test]
    fn sort_model_ids_alphabetical() {
        assert_eq!(
            sort_model_ids(vec!["c".into(), "a".into(), "b".into()]),
            vec!["a", "b", "c"]
        );
    }

    #[test]
    fn parse_model_ids_from_json() {
        let json = br#"{"data":[{"id":"z"},{"id":"a"},{"id":"m"}]}"#;
        let ids = parse_model_ids(json).unwrap();
        assert_eq!(ids, vec!["a", "m", "z"]);
    }

    #[test]
    fn parse_model_ids_invalid_json() {
        let json = b"not json";
        assert!(parse_model_ids(json).is_err());
    }

    #[test]
    fn header_map_builds_btreemap() {
        let m = header_map(CloudAIProvider::OpenAI, "k");
        assert_eq!(m.get("Authorization"), Some(&"Bearer k".to_string()));
    }

    #[test]
    fn provider_preferences_default() {
        let p = CloudAIProviderPreferences::default();
        assert!(!p.api_key_configured);
        assert!(p.selected_model_id.is_none());
        assert!(p.available_model_ids.is_empty());
    }

    // ------------------------------------------------------------------
    // Persistence
    //
    // Keychain-touching tests swap in `keyring`'s mock backend first (the
    // convention from `crate::sql_credentials`) so the real macOS Keychain
    // is never read or written by the suite.
    // ------------------------------------------------------------------

    fn use_mock_keyring() {
        keyring::mock::install();
    }

    fn config_with_key(key: &str) -> CloudAIConfig {
        CloudAIConfig {
            openai: CloudAIProviderConfig {
                api_key: key.to_string(),
                selected_model_id: Some("gpt-4o".to_string()),
                available_model_ids: vec!["gpt-4o".to_string(), "o3".to_string()],
            },
            ..Default::default()
        }
    }

    /// The security-critical assertion: a key held in memory must never reach
    /// the JSON that `save_cloud_ai_config` writes to disk.
    #[test]
    fn api_key_is_never_serialized_to_json() {
        let json = serde_json::to_string(&config_with_key("sk-live-SUPERSECRET")).unwrap();
        assert!(
            !json.contains("SUPERSECRET"),
            "api key leaked into on-disk JSON: {json}"
        );
        assert!(
            !json.contains("apiKey"),
            "api key field leaked into on-disk JSON: {json}"
        );
        // The non-secret half must still be there — otherwise this test would
        // pass on an empty document.
        assert!(json.contains("gpt-4o"));
    }

    #[test]
    fn json_round_trip_keeps_non_secrets_and_drops_key() {
        let json = serde_json::to_vec_pretty(&config_with_key("sk-live-SUPERSECRET")).unwrap();
        let back: CloudAIConfig = serde_json::from_slice(&json).unwrap();
        assert_eq!(back.openai.selected_model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(back.openai.available_model_ids, vec!["gpt-4o", "o3"]);
        // Deserializing cannot resurrect the key; only the keychain can.
        assert!(back.openai.api_key.is_empty());
    }

    #[test]
    fn deserialize_missing_file_contents_yields_defaults() {
        // Mirrors what `load_cloud_ai_config` does with an unreadable file.
        let config: CloudAIConfig = serde_json::from_slice(b"{}").unwrap_or_default();
        assert_eq!(config, CloudAIConfig::default());
        assert!(config.openai.api_key.is_empty());
        assert!(config.openai.selected_model_id.is_none());
    }

    #[test]
    fn deserialize_corrupt_json_falls_back_to_defaults() {
        let config: CloudAIConfig = serde_json::from_slice(b"not json").unwrap_or_default();
        assert_eq!(config, CloudAIConfig::default());
    }

    #[test]
    fn provider_accessors_map_to_matching_fields() {
        let mut config = CloudAIConfig::default();
        config.provider_mut(CloudAIProvider::Claude).api_key = "c".into();
        config.provider_mut(CloudAIProvider::OpenAI).api_key = "o".into();
        config.provider_mut(CloudAIProvider::OpenRouter).api_key = "r".into();
        assert_eq!(config.claude.api_key, "c");
        assert_eq!(config.openai.api_key, "o");
        assert_eq!(config.openrouter.api_key, "r");
        assert_eq!(config.provider(CloudAIProvider::Claude).api_key, "c");
    }

    #[test]
    fn fetched_model_save_payload_is_non_secret_only() {
        let mut config = CloudAIConfig::default();
        config.openai.available_model_ids = vec!["gpt-5".to_string()];
        let json = serialize_cloud_ai_config(&config).unwrap();
        assert!(json.contains("gpt-5"));
        assert!(!json.contains("apiKey"));
        assert!(!json.contains("delete"));
    }

    #[test]
    fn absent_key_reads_as_none_not_an_error() {
        use_mock_keyring();
        // A provider the user has never configured must be reported as
        // "no key", not as a keychain failure.
        assert_eq!(load_api_key(CloudAIProvider::OpenRouter).unwrap(), None);
        assert!(!keychain_key_present(CloudAIProvider::OpenRouter));
    }

    #[test]
    fn delete_api_key_is_idempotent_when_absent() {
        use_mock_keyring();
        let provider = CloudAIProvider::Claude;
        // Clearing an already-empty field must succeed, not error.
        delete_api_key(provider).unwrap();
        delete_api_key(provider).unwrap();
        assert!(!keychain_key_present(provider));
    }

    #[test]
    fn each_provider_gets_a_distinct_keychain_account() {
        // A shared account name would make one provider's key silently
        // overwrite another's.
        let accounts: std::collections::BTreeSet<String> = CloudAIProvider::all()
            .iter()
            .map(|p| p.keychain_account())
            .collect();
        assert_eq!(accounts.len(), CloudAIProvider::all().len());
    }

    #[test]
    fn models_endpoint_per_kind() {
        assert_eq!(
            models_endpoint(&ProviderKind::OpenAiCompatible, "http://localhost:1234/v1/"),
            Some("http://localhost:1234/v1/models".into())
        );
        assert_eq!(
            models_endpoint(&ProviderKind::Anthropic, "https://api.anthropic.com"),
            Some("https://api.anthropic.com/v1/models".into())
        );
        assert_eq!(
            models_endpoint(
                &ProviderKind::Gemini,
                "https://generativelanguage.googleapis.com"
            ),
            Some("https://generativelanguage.googleapis.com/v1beta/models".into())
        );
    }

    #[test]
    fn parse_openai_models() {
        let json = br#"{"data":[{"id":"z"},{"id":"a"}]}"#;
        let ids = parse_model_ids_for(&ProviderKind::OpenAi, json).unwrap();
        assert_eq!(ids, vec!["a", "z"]);
    }

    #[test]
    fn parse_anthropic_models() {
        let json = br#"{"data":[{"id":"claude","display_name":"Claude"}]}"#;
        let ids = parse_model_ids_for(&ProviderKind::Anthropic, json).unwrap();
        assert_eq!(ids, vec!["claude"]);
    }

    #[test]
    fn parse_gemini_models() {
        let json = br#"{"models":[
            {"name":"models/gemini-2.5","supportedGenerationMethods":["generateContent"]},
            {"name":"models/embed","supportedGenerationMethods":["embedContent"]}
        ]}"#;
        let ids = parse_model_ids_for(&ProviderKind::Gemini, json).unwrap();
        assert_eq!(ids, vec!["gemini-2.5"]);
    }

    #[test]
    fn parse_invalid_is_error() {
        assert!(parse_model_ids_for(&ProviderKind::OpenAi, b"nope").is_err());
    }

    #[test]
    fn egress_label_localhost_is_local() {
        let profile = ProviderProfile {
            base_url: "http://localhost:1234/v1".into(),
            ..ProviderProfile::new(ProviderKind::OpenAiCompatible)
        };
        assert!(egress_label(&profile).starts_with("Local — localhost"));
    }

    #[test]
    fn egress_label_cloud_names_host() {
        let profile = ProviderProfile {
            base_url: "https://api.anthropic.com".into(),
            ..ProviderProfile::new(ProviderKind::Anthropic)
        };
        assert!(egress_label(&profile).contains("api.anthropic.com"));
    }

    #[test]
    fn egress_label_dot_local_is_local() {
        let profile = ProviderProfile {
            base_url: "http://ollama.local:11434/v1".into(),
            ..ProviderProfile::new(ProviderKind::OpenAiCompatible)
        };
        assert!(egress_label(&profile).starts_with("Local — ollama.local"));
    }

    #[test]
    fn reasoning_for_maps_per_kind() {
        use sqlly_services::providers::CloudReasoning;
        let mut profile = ProviderProfile::new(ProviderKind::DeepSeek);
        profile.reasoning = ReasoningLevel::High;
        assert_eq!(reasoning_for(&profile), CloudReasoning::None);

        profile.kind = ProviderKind::OpenAi;
        profile.reasoning = ReasoningLevel::High;
        assert_eq!(
            reasoning_for(&profile),
            CloudReasoning::OpenAiEffort("high")
        );

        profile.kind = ProviderKind::OpenAiCompatible;
        profile.send_reasoning_effort = false;
        assert_eq!(reasoning_for(&profile), CloudReasoning::None);

        profile.send_reasoning_effort = true;
        assert_eq!(
            reasoning_for(&profile),
            CloudReasoning::OpenAiEffort("high")
        );
    }

    #[test]
    fn migrate_legacy_preserves_keyring_accounts() {
        let mut config = CloudAIConfig::default();
        config.claude.selected_model_id = Some("x".into());
        config.app_ai_provider = Some(CloudAIProvider::Claude);
        let profiles = migrate_legacy(&config);
        let claude = profiles
            .profiles
            .iter()
            .find(|profile| profile.id == "legacy-claude")
            .unwrap();
        assert_eq!(claude.api_key_ref, "ai-provider.claude-api-key");
        assert_eq!(claude.model, "x");
        assert_eq!(profiles.active_profile_id.as_deref(), Some("legacy-claude"));
    }

    #[test]
    fn migrate_legacy_with_no_selection_has_no_active_profile() {
        let profiles = migrate_legacy(&CloudAIConfig::default());
        assert!(profiles.active_profile_id.is_none());
    }

    #[test]
    fn migrate_legacy_keeps_available_models() {
        let mut config = CloudAIConfig::default();
        config.openai.available_model_ids = vec!["gpt-4o".into()];
        let profiles = migrate_legacy(&config);
        let openai = profiles
            .profiles
            .iter()
            .find(|profile| profile.id == "legacy-openai")
            .unwrap();
        assert_eq!(openai.available_model_ids, vec!["gpt-4o"]);
    }

    /// Gridex #97: Cheaper Inference is a named OpenAI-compatible preset with
    /// its gateway address and a starter model filled in.
    #[test]
    fn cheaper_inference_preset_defaults() {
        let kind = ProviderKind::CheaperInference;
        assert!(ProviderKind::all().contains(&kind));
        assert_eq!(kind.display_name(), "Cheaper Inference");
        assert_eq!(
            kind.default_base_url(),
            "https://api.cheaperinference.com/v1"
        );
        assert_eq!(kind.default_model(), "gpt-5.4-mini");
        assert_eq!(kind.id_slug(), "cheaper-inference");
        assert_eq!(
            kind.wire(),
            sqlly_services::providers::CloudWire::OpenAiChat
        );

        let profile = ProviderProfile::new(kind);
        assert_eq!(profile.name, "Cheaper Inference");
        assert_eq!(profile.base_url, "https://api.cheaperinference.com/v1");
        assert_eq!(profile.model, "gpt-5.4-mini");
        assert!(
            profile
                .available_model_ids
                .contains(&"gpt-5.4-mini".to_string())
        );

        assert_eq!(
            models_endpoint(&kind, &profile.base_url).as_deref(),
            Some("https://api.cheaperinference.com/v1/models")
        );
        assert_eq!(
            chat_endpoint(&kind, &profile.base_url, &profile.model).unwrap(),
            "https://api.cheaperinference.com/v1/chat/completions"
        );
        assert_eq!(
            auth_headers(&kind, "sk-test"),
            vec![("Authorization", "Bearer sk-test".to_string())]
        );

        let json = serde_json::to_value(&profile).unwrap();
        assert_eq!(json["kind"]["kind"], "cheaperInference");
        let back: ProviderProfile = serde_json::from_value(json).unwrap();
        assert_eq!(back.kind, kind);
    }

    #[test]
    fn other_presets_keep_an_empty_default_model() {
        for kind in ProviderKind::all() {
            if *kind != ProviderKind::CheaperInference {
                assert_eq!(kind.default_model(), "", "{kind:?}");
                assert!(ProviderProfile::new(*kind).model.is_empty());
            }
        }
    }

    /// Gridex #92 (Windows: model fetching says "fetch failed" or crashes).
    /// Every failure mode of the models fetch comes back as a typed error with
    /// readable text for Settings, never a panic: refused connection, a non-200
    /// reply, a 200 with junk, and an API key that is not a valid header value.
    mod model_fetch_failures {
        use super::*;
        use std::io::{Read, Write};

        fn profile_at(base_url: String) -> ProviderProfile {
            ProviderProfile {
                base_url,
                ..ProviderProfile::new(ProviderKind::OpenAiCompatible)
            }
        }

        /// One-shot HTTP server on 127.0.0.1 that answers with `response`.
        fn serve_once(response: &'static str) -> String {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    let _ = stream.read(&mut buf);
                    let _ = stream.write_all(response.as_bytes());
                }
            });
            format!("http://{addr}/v1")
        }

        #[tokio::test]
        async fn refused_connection_is_a_network_error_with_text() {
            // Bind then drop: the port is free and nothing listens on it.
            let port = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap()
                .port();
            let err = fetch_models_for(&profile_at(format!("http://127.0.0.1:{port}/v1")), "k")
                .await
                .unwrap_err();
            assert_eq!(err, CloudAIModelsError::Network);
            assert!(!err.to_string().trim().is_empty());
        }

        #[tokio::test]
        async fn server_error_reply_is_an_http_status_error() {
            let base = serve_once(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
            );
            let err = fetch_models_for(&profile_at(base), "k").await.unwrap_err();
            assert!(
                matches!(
                    err,
                    CloudAIModelsError::HttpStatus {
                        status_code: 500,
                        ..
                    }
                ),
                "{err:?}"
            );
            assert!(!err.to_string().trim().is_empty());
        }

        #[tokio::test]
        async fn junk_body_is_an_invalid_response_error() {
            let base = serve_once(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot json!",
            );
            let err = fetch_models_for(&profile_at(base), "k").await.unwrap_err();
            assert!(
                matches!(err, CloudAIModelsError::InvalidResponse { .. }),
                "{err:?}"
            );
        }

        #[tokio::test]
        async fn api_key_that_is_not_a_header_value_errors_instead_of_panicking() {
            let base = serve_once("HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n");
            let err = fetch_models_for(&profile_at(base), "bad\nkey")
                .await
                .unwrap_err();
            assert_eq!(err, CloudAIModelsError::Network);
        }
    }
}
