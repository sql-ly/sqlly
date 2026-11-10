//! Fill-in-the-middle (FIM) inline completion prompt rendering and
//! preferences. Port of the pure parts of `FIMCompletionService.swift` +
//! `QueryAISummaryPreferences.swift` (`FIMPreferences`/`FIMAIModel`/
//! `FIMPromptFormat`/`FIMModelTokens`/`FIMTokenPlaceholder`).
//!
//! Pure prompt-construction and token-budget logic lives here alongside the
//! non-streaming Ollama HTTP client (`FimCompletionService`); the app layer
//! owns the debounce/cancellation loop and the editor ghost-text rendering.

use serde::{Deserialize, Serialize};

/// Which on-device model produces the completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FimAiModel {
    Qwen25Coder15BBase,
    Qwen25Coder3BBase,
}

impl FimAiModel {
    pub fn display_name(self) -> &'static str {
        match self {
            FimAiModel::Qwen25Coder15BBase => "Qwen2.5-Coder-1.5B Base",
            FimAiModel::Qwen25Coder3BBase => "Qwen2.5-Coder-3B Base",
        }
    }

    pub fn ollama_model_name(self) -> &'static str {
        match self {
            FimAiModel::Qwen25Coder15BBase => "qwen2.5-coder:1.5b",
            FimAiModel::Qwen25Coder3BBase => "qwen2.5-coder:3b",
        }
    }

    pub fn default_format(self) -> FimPromptFormat {
        // Both Qwen models use the Qwen FIM format.
        FimPromptFormat::Qwen
    }

    #[allow(dead_code)]
    pub fn all() -> &'static [FimAiModel] {
        &[
            FimAiModel::Qwen25Coder15BBase,
            FimAiModel::Qwen25Coder3BBase,
        ]
    }
}

/// The token set / structural flavor a model expects for FIM prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FimPromptFormat {
    /// Derive the format from the selected model's `default_format`.
    Auto,
    Qwen,
    #[allow(dead_code)]
    DeepSeek,
    #[allow(dead_code)]
    CodeLlama,
    #[allow(dead_code)]
    StarCoder,
    /// Use the tokens embedded directly in the user's `pattern_template`.
    #[allow(dead_code)]
    Custom,
}

impl FimPromptFormat {
    pub fn display_name(self) -> String {
        match self {
            FimPromptFormat::Auto => crate::localization::text("settings.ai.fim.auto_match_model"),
            FimPromptFormat::Qwen => "Qwen2.5-Coder".to_string(),
            FimPromptFormat::DeepSeek => "DeepSeek-Coder".to_string(),
            FimPromptFormat::CodeLlama => "Code Llama".to_string(),
            FimPromptFormat::StarCoder => "StarCoder".to_string(),
            FimPromptFormat::Custom => {
                crate::localization::text("settings.ai.fim.custom_tokens_pattern")
            }
        }
    }

    #[allow(dead_code)]
    pub fn all() -> &'static [FimPromptFormat] {
        &[
            FimPromptFormat::Auto,
            FimPromptFormat::Qwen,
            FimPromptFormat::DeepSeek,
            FimPromptFormat::CodeLlama,
            FimPromptFormat::StarCoder,
            FimPromptFormat::Custom,
        ]
    }

    /// The concrete control tokens for this format. `Auto` resolves through
    /// the supplied model; `Custom` returns empty tokens (the pattern carries
    /// the literal tokens itself).
    pub fn tokens(self, model: FimAiModel) -> FimModelTokens {
        match self {
            FimPromptFormat::Auto => model.default_format().tokens(model),
            FimPromptFormat::Qwen => FimModelTokens {
                prefix: "<|fim_prefix|>",
                suffix: "<|fim_suffix|>",
                middle: "<|fim_middle|>",
            },
            FimPromptFormat::DeepSeek => FimModelTokens {
                prefix: "<\u{ff5c}fim\u{2581}begin\u{ff5c}>",
                suffix: "<\u{ff5c}fim\u{2581}hole\u{ff5c}>",
                middle: "<\u{ff5c}fim\u{2581}end\u{ff5c}>",
            },
            FimPromptFormat::CodeLlama => FimModelTokens {
                prefix: "<PRE>",
                suffix: "<SUF>",
                middle: "<MID>",
            },
            FimPromptFormat::StarCoder => FimModelTokens {
                prefix: "<fim_prefix>",
                suffix: "<fim_suffix>",
                middle: "<fim_middle>",
            },
            FimPromptFormat::Custom => FimModelTokens {
                prefix: "",
                suffix: "",
                middle: "",
            },
        }
    }
}

/// The concrete sentinel tokens a given model uses to delimit the FIM prompt
/// sections.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FimModelTokens {
    pub prefix: &'static str,
    pub suffix: &'static str,
    pub middle: &'static str,
}

/// App-unique placeholders the user types into the FIM pattern template.
pub mod placeholder {
    pub const PREFIX: &str = "{{PREFIX}}";
    pub const SUFFIX: &str = "{{SUFFIX}}";
    pub const MIDDLE: &str = "{{MIDDLE}}";
    pub const PREFIX_TEXT: &str = "{{prefix_text}}";
    pub const SUFFIX_TEXT: &str = "{{suffix_text}}";
    #[allow(dead_code)]
    pub const ALL: &[&str] = &[PREFIX, SUFFIX, MIDDLE, PREFIX_TEXT, SUFFIX_TEXT];
}

/// What causes an inline completion request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FimTriggerKind {
    Automatic,
    Manual,
}

impl FimTriggerKind {
    pub fn display_name(self) -> String {
        match self {
            FimTriggerKind::Automatic => {
                crate::localization::text("settings.ai.fim.automatically_as_type")
            }
            FimTriggerKind::Manual => crate::localization::text("settings.ai.fim.only_i_press_key"),
        }
    }
}

/// Rough chars-per-token ratio for the lightweight token estimator.
pub const CHARS_PER_TOKEN_ESTIMATE: usize = 4;

/// Default FIM pattern template, expressed with app-unique placeholders.
pub fn default_pattern_template() -> String {
    format!(
        "{}{}{}{}{}",
        placeholder::PREFIX,
        placeholder::PREFIX_TEXT,
        placeholder::SUFFIX,
        placeholder::SUFFIX_TEXT,
        placeholder::MIDDLE
    )
}

/// Default extra stop sequences (a blank line ends a logical block).
pub const DEFAULT_STOP_SEQUENCES: &[&str] = &["\n\n"];

/// The default system prompt sent to the FIM model.
pub const DEFAULT_SYSTEM_PROMPT: &str = "You are an inline code completion engine for SQL. Complete the SQL between the prefix and suffix. Output only the completion text \u{2014} no explanations, no markdown fences, no backticks unless they are part of the SQL syntax. Match the surrounding style and indentation.";

/// All user-configurable parameters for FIM inline completion.
///
/// `#[serde(default)]` is struct-level on purpose: every field must survive a
/// prefs file written before that field existed, and `Default` (below) is the
/// single source for those values — the same one the preset-less first run uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FimPreferences {
    pub enabled: bool,
    pub enable_for_new_queries: bool,
    pub model: FimAiModel,
    pub fim_format: FimPromptFormat,
    pub trigger_kind: FimTriggerKind,
    pub generation_delay_secs: f64,
    pub disable_on_tokens: Vec<String>,
    pub system_prompt: String,
    pub pattern_template: String,
    pub trim_leading_whitespace: bool,
    pub max_context_tokens: usize,
    pub suffix_percent: f64,
    pub temperature: f64,
    pub max_new_tokens: usize,
    pub stop_sequences: Vec<String>,
    pub stop_on_newline: bool,
}

impl Default for FimPreferences {
    fn default() -> Self {
        Self {
            enabled: false,
            enable_for_new_queries: false,
            model: FimAiModel::Qwen25Coder15BBase,
            fim_format: FimPromptFormat::Auto,
            trigger_kind: FimTriggerKind::Automatic,
            generation_delay_secs: 0.3,
            disable_on_tokens: vec![
                "//".to_string(),
                "#".to_string(),
                "/*".to_string(),
                "\"".to_string(),
                "'".to_string(),
                "`".to_string(),
            ],
            system_prompt: DEFAULT_SYSTEM_PROMPT.to_string(),
            pattern_template: default_pattern_template(),
            trim_leading_whitespace: false,
            max_context_tokens: 4096,
            suffix_percent: 0.25,
            temperature: 0.1,
            max_new_tokens: 256,
            stop_sequences: DEFAULT_STOP_SEQUENCES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            stop_on_newline: false,
        }
    }
}

impl FimPreferences {
    /// Load/save live here rather than in `preferences.rs` (where the path fn
    /// and the other prefs' load/save do) so they stay next to the struct and
    /// its `Default`, which is the serde default source; splitting them across
    /// modules would hide persistence from every reader of `FimPreferences`.
    pub fn load() -> Self {
        #[cfg(target_arch = "wasm32")]
        return crate::host_storage::load_text(crate::host_storage::StorageArea::Data, "fim.json")
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = crate::preferences::fim_path();
            match std::fs::read(&path) {
                Ok(data) => serde_json::from_slice(&data).unwrap_or_default(),
                Err(_) => Self::default(),
            }
        }
    }

    pub fn save(&self) {
        #[cfg(target_arch = "wasm32")]
        {
            if let Ok(text) = crate::config_version::to_string_pretty(self) {
                let _ = crate::host_storage::store_text(
                    crate::host_storage::StorageArea::Data,
                    "fim.json",
                    &text,
                );
            }
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = crate::preferences::fim_path();
            if let Ok(data) = crate::config_version::to_vec_pretty(self) {
                let _ = std::fs::write(path, data);
            }
        }
    }

    #[allow(dead_code)]
    /// Lightweight token estimate for a string (chars / 4, min 1 for non-empty).
    pub fn estimated_token_count(text: &str) -> usize {
        if text.is_empty() {
            0
        } else {
            std::cmp::max(1, text.chars().count() / CHARS_PER_TOKEN_ESTIMATE)
        }
    }

    /// The character budgets for prefix and suffix derived from the token
    /// window. `max_new_tokens` is reserved out of `max_context_tokens` first,
    /// then the remainder is split by `suffix_percent` (suffix) / remainder
    /// (prefix). Returns character counts.
    pub fn context_character_budgets(&self) -> (usize, usize) {
        let usable = self.max_context_tokens.saturating_sub(self.max_new_tokens);
        let clamped_suffix = self.suffix_percent.clamp(0.0, 1.0);
        let suffix_tokens = (usable as f64 * clamped_suffix) as usize;
        let prefix_tokens = usable.saturating_sub(suffix_tokens);
        (
            prefix_tokens * CHARS_PER_TOKEN_ESTIMATE,
            suffix_tokens * CHARS_PER_TOKEN_ESTIMATE,
        )
    }

    /// The active control tokens, resolving `.auto`/model.
    pub fn resolved_tokens(&self) -> FimModelTokens {
        self.fim_format.tokens(self.model)
    }

    /// Truncates prefix to the trailing `limit` chars (keeps text nearest the
    /// cursor). A non-positive limit means "no limit".
    pub fn clamp_prefix(text: &str, limit: usize) -> String {
        if limit > 0 && text.chars().count() > limit {
            text.chars()
                .rev()
                .take(limit)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        } else {
            text.to_string()
        }
    }

    /// Truncates suffix to the leading `limit` chars.
    pub fn clamp_suffix(text: &str, limit: usize) -> String {
        if limit > 0 && text.chars().count() > limit {
            text.chars().take(limit).collect()
        } else {
            text.to_string()
        }
    }

    /// Strips the leading whitespace from the first line of the suffix.
    fn strip_leading_indentation(text: &str) -> String {
        match text.find(|c: char| c != ' ' && c != '\t') {
            Some(idx) => text[idx..].to_string(),
            None => text.to_string(),
        }
    }

    /// Renders the final model prompt: applies the token-budget context split,
    /// substitutes the app-unique placeholders in the pattern with the active
    /// format's real tokens, and injects the (clamped) text.
    pub fn render_prompt(&self, prefix: &str, suffix: &str) -> String {
        let (prefix_budget, suffix_budget) = self.context_character_budgets();
        let clamped_prefix = if prefix_budget == 0 {
            String::new()
        } else {
            Self::clamp_prefix(prefix, prefix_budget)
        };
        let mut clamped_suffix = if suffix_budget == 0 {
            String::new()
        } else {
            Self::clamp_suffix(suffix, suffix_budget)
        };
        if self.trim_leading_whitespace {
            clamped_suffix = Self::strip_leading_indentation(&clamped_suffix);
        }
        let tokens = self.resolved_tokens();
        let mut rendered = self.pattern_template.clone();
        rendered = rendered.replace(placeholder::PREFIX, tokens.prefix);
        rendered = rendered.replace(placeholder::SUFFIX, tokens.suffix);
        rendered = rendered.replace(placeholder::MIDDLE, tokens.middle);
        rendered = rendered.replace(placeholder::PREFIX_TEXT, &clamped_prefix);
        rendered = rendered.replace(placeholder::SUFFIX_TEXT, &clamped_suffix);
        rendered
    }

    /// The effective stop set: the format's structural control tokens, plus
    /// the user's extra `stop_sequences`, plus an optional trailing newline.
    pub fn effective_stop_sequences(&self) -> Vec<String> {
        let tokens = self.resolved_tokens();
        let mut stops: Vec<String> = [tokens.prefix, tokens.suffix, tokens.middle]
            .into_iter()
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        stops.extend(self.stop_sequences.iter().cloned());
        if self.stop_on_newline {
            stops.push("\n".to_string());
        }
        // De-duplicate while preserving order.
        let mut seen = std::collections::HashSet::new();
        stops.retain(|s| seen.insert(s.clone()));
        stops
    }

    /// Whether a freshly created query tab starts with inline autocomplete on.
    /// Mirrors the Swift new-document flow: a tab inherits the global switch,
    /// and `enable_for_new_queries` opts new tabs in even while the global
    /// switch is off, so flipping the global on later lights them up without
    /// a per-tab visit.
    pub fn seed_new_tab_enabled(&self) -> bool {
        self.enabled || self.enable_for_new_queries
    }

    /// Whether a completion should be suppressed given the text immediately
    /// before the cursor. Matches any `disable_on_tokens` entry that the
    /// prefix ends with (ignoring trailing spaces/tabs on the current line).
    pub fn should_suppress(&self, prefix: &str) -> bool {
        if self.disable_on_tokens.is_empty() {
            return false;
        }
        let current_line = prefix.rsplit('\n').next().unwrap_or(prefix);
        let trimmed = current_line.trim_end_matches([' ', '\t']);
        self.disable_on_tokens
            .iter()
            .any(|tok| trimmed.ends_with(tok.as_str()))
    }
}

/// How many tokens of prefix tail / completion head the repetition scan
/// compares. Repetitions the base models emit restate the last clause, which
/// is well under this.
const REPETITION_SCAN_TOKENS: usize = 32;

/// Lexes `text` into SQL-ish tokens: runs of `[A-Za-z0-9_]` (lowercased) and
/// single non-whitespace symbol chars, each with its byte range in `text`.
fn lex_repetition_tokens(text: &str) -> Vec<(String, std::ops::Range<usize>)> {
    let mut tokens = Vec::new();
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        if c.is_alphanumeric() || c == '_' {
            let mut end = start + c.len_utf8();
            while let Some(&(idx, nc)) = chars.peek() {
                if nc.is_alphanumeric() || nc == '_' {
                    end = idx + nc.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push((text[start..end].to_lowercase(), start..end));
        } else {
            let end = start + c.len_utf8();
            tokens.push((text[start..end].to_string(), start..end));
        }
    }
    tokens
}

/// Removes the leading portion of `completion` that merely restates the text
/// immediately before the cursor. The base FIM models sometimes re-emit the
/// clause they just read in the prefix (e.g. suggesting `WHERE Id = 47 AND …`
/// when the prefix already ends `where\n    Id = 47`); this trims the restated
/// tokens so only genuinely new text is shown as ghost text. Matching is
/// token-based, so case and whitespace differences still count as repetition.
/// Requires at least two repeated tokens to avoid clipping legitimate
/// single-token coincidences. Returns an empty string when the completion is
/// entirely a repetition (caller hides the suggestion).
pub fn trim_prefix_repetition(prefix: &str, completion: &str) -> String {
    let scan_start = prefix
        .char_indices()
        .rev()
        .nth(REPETITION_SCAN_TOKENS * 24)
        .map(|(idx, _)| idx)
        .unwrap_or(0);
    let mut prefix_tokens = lex_repetition_tokens(&prefix[scan_start..]);
    if prefix_tokens.len() > REPETITION_SCAN_TOKENS {
        prefix_tokens.drain(..prefix_tokens.len() - REPETITION_SCAN_TOKENS);
    }
    let completion_tokens: Vec<_> = lex_repetition_tokens(completion)
        .into_iter()
        .take(REPETITION_SCAN_TOKENS)
        .collect();
    let max_k = prefix_tokens.len().min(completion_tokens.len());
    for k in (2..=max_k).rev() {
        let tail = &prefix_tokens[prefix_tokens.len() - k..];
        let head = &completion_tokens[..k];
        if tail.iter().map(|(t, _)| t).eq(head.iter().map(|(t, _)| t)) {
            let cut = completion_tokens[k - 1].1.end;
            let rest = &completion[cut..];
            return if rest.trim().is_empty() {
                String::new()
            } else {
                rest.to_string()
            };
        }
    }
    completion.to_string()
}

// ---------------------------------------------------------------------------
// HTTP client — Ollama `/api/generate` FIM completion.
// Mirrors `FIMCompletionService.swift`.
// ---------------------------------------------------------------------------

/// Error from a FIM completion request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FimCompletionError {
    /// The preferences/prompt were invalid (e.g. empty model name).
    InvalidRequest,
    /// The HTTP request failed (network error, non-2xx status, or the
    /// Ollama daemon was not running).
    HttpFailed(String),
    /// The response body could not be parsed (missing `response` field,
    /// malformed JSON).
    InvalidResponse,
}

/// Ollama `/api/generate` request body.
#[derive(Serialize)]
struct OllamaGenerateRequest {
    model: String,
    prompt: String,
    stream: bool,
    options: OllamaOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
}

#[derive(Serialize)]
struct OllamaOptions {
    temperature: f32,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "stop")]
    stop: Vec<String>,
    num_predict: u32,
}

/// Ollama `/api/generate` response body.
#[derive(Deserialize)]
struct OllamaGenerateResponse {
    response: String,
}

/// HTTP client for Ollama FIM completion. Stateless aside from the base URL
/// and the preferences snapshot — each `complete()` call builds a fresh
/// POST request.
#[derive(Clone)]
pub struct FimCompletionService {
    base_url: String,
    model: FimAiModel,
    prefs: FimPreferences,
}

impl FimCompletionService {
    /// Create a service targeting the given Ollama base URL (e.g.
    /// `http://127.0.0.1:11434`).
    pub fn new(base_url: impl Into<String>, model: FimAiModel, prefs: FimPreferences) -> Self {
        Self {
            base_url: base_url.into(),
            model,
            prefs,
        }
    }

    /// Build the HTTP request body for a `(prefix, suffix)` completion. Exposed
    /// for unit tests so the prompt-rendering path can be verified without
    /// network I/O.
    fn build_request_body(&self, prefix: &str, suffix: &str) -> Result<String, FimCompletionError> {
        let model = self.model.ollama_model_name();
        if model.is_empty() {
            return Err(FimCompletionError::InvalidRequest);
        }
        let prompt = self.prefs.render_prompt(prefix, suffix);
        let body = OllamaGenerateRequest {
            model: model.to_string(),
            prompt,
            stream: false,
            options: OllamaOptions {
                temperature: self.prefs.temperature as f32,
                stop: self.prefs.effective_stop_sequences(),
                num_predict: self.prefs.max_new_tokens as u32,
            },
            system: if self.prefs.system_prompt.is_empty() {
                None
            } else {
                Some(self.prefs.system_prompt.clone())
            },
        };
        serde_json::to_string(&body).map_err(|_| FimCompletionError::InvalidRequest)
    }

    /// Request a FIM completion from Ollama. Returns the completion text (the
    /// `response` field of the Ollama JSON response).
    pub async fn complete(&self, prefix: &str, suffix: &str) -> Result<String, FimCompletionError> {
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = (prefix, suffix);
            Err(FimCompletionError::HttpFailed(
                crate::browser_preview::unavailable(&crate::localization::text(
                    "settings.ai.fim.browser_unavailable",
                )),
            ))
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.complete_native(prefix, suffix).await
        }
    }

    /// Request a FIM completion from Ollama (native only).
    #[cfg(not(target_arch = "wasm32"))]
    async fn complete_native(
        &self,
        prefix: &str,
        suffix: &str,
    ) -> Result<String, FimCompletionError> {
        let body = self.build_request_body(prefix, suffix)?;
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| FimCompletionError::HttpFailed(e.to_string()))?;
        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| FimCompletionError::HttpFailed(e.to_string()))?;
        if !resp.status().is_success() {
            return Err(FimCompletionError::HttpFailed(format!(
                "HTTP {}",
                resp.status()
            )));
        }
        let parsed: OllamaGenerateResponse = resp
            .json()
            .await
            .map_err(|_| FimCompletionError::InvalidResponse)?;
        Ok(parsed.response)
    }

    /// Construct a service from environment + prefs. Reads the `OLLAMA_HOST`
    /// env var (default `http://127.0.0.1:11434`), mirroring the Swift
    /// `activeOllamaBaseURL()`.
    pub fn from_env(model: FimAiModel, prefs: FimPreferences) -> Self {
        let base_url =
            std::env::var("OLLAMA_HOST").unwrap_or_else(|_| "http://127.0.0.1:11434".to_string());
        Self::new(base_url, model, prefs)
    }
}

#[allow(dead_code)]
/// Quick reachability probe for the Ollama daemon. Returns `true` if the
/// `/api/tags` endpoint responds with 2xx. Used to gate FIM requests so we
/// don't waste a full generation round-trip when the daemon is down.
pub async fn ollama_is_reachable(base_url: &str) -> bool {
    #[cfg(target_arch = "wasm32")]
    return {
        // No local Ollama daemon can be reached from the browser preview.
        let _ = base_url;
        false
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        let url = format!("{}/api/tags", base_url.trim_end_matches('/'));
        let _ = rustls::crypto::ring::default_provider().install_default();
        let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
        else {
            return false;
        };
        matches!(client.get(&url).send().await, Ok(r) if r.status().is_success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_display_names() {
        assert_eq!(
            FimAiModel::Qwen25Coder15BBase.display_name(),
            "Qwen2.5-Coder-1.5B Base"
        );
        assert_eq!(
            FimAiModel::Qwen25Coder3BBase.display_name(),
            "Qwen2.5-Coder-3B Base"
        );
    }

    #[test]
    fn ollama_model_names() {
        assert_eq!(
            FimAiModel::Qwen25Coder15BBase.ollama_model_name(),
            "qwen2.5-coder:1.5b"
        );
        assert_eq!(
            FimAiModel::Qwen25Coder3BBase.ollama_model_name(),
            "qwen2.5-coder:3b"
        );
    }

    #[test]
    fn format_tokens_qwen() {
        let t = FimPromptFormat::Qwen.tokens(FimAiModel::Qwen25Coder15BBase);
        assert_eq!(t.prefix, "<|fim_prefix|>");
        assert_eq!(t.suffix, "<|fim_suffix|>");
        assert_eq!(t.middle, "<|fim_middle|>");
    }

    #[test]
    fn format_tokens_auto_resolves_model() {
        let t = FimPromptFormat::Auto.tokens(FimAiModel::Qwen25Coder3BBase);
        assert_eq!(t.prefix, "<|fim_prefix|>"); // Qwen default
    }

    #[test]
    fn format_tokens_custom_empty() {
        let t = FimPromptFormat::Custom.tokens(FimAiModel::Qwen25Coder15BBase);
        assert!(t.prefix.is_empty());
        assert!(t.suffix.is_empty());
        assert!(t.middle.is_empty());
    }

    #[test]
    fn format_tokens_code_llama() {
        let t = FimPromptFormat::CodeLlama.tokens(FimAiModel::Qwen25Coder15BBase);
        assert_eq!(t.prefix, "<PRE>");
        assert_eq!(t.suffix, "<SUF>");
        assert_eq!(t.middle, "<MID>");
    }

    #[test]
    fn trigger_kind_display() {
        assert_eq!(
            FimTriggerKind::Automatic.display_name(),
            "Automatically as you type"
        );
        assert_eq!(
            FimTriggerKind::Manual.display_name(),
            "Only when I press a key"
        );
    }

    #[test]
    fn estimated_token_count_empty() {
        assert_eq!(FimPreferences::estimated_token_count(""), 0);
    }

    #[test]
    fn estimated_token_count_non_empty_min_one() {
        assert_eq!(FimPreferences::estimated_token_count("ab"), 1);
        assert_eq!(FimPreferences::estimated_token_count("abcdefgh"), 2);
    }

    #[test]
    fn context_character_budgets_default() {
        let p = FimPreferences::default();
        let (prefix, suffix) = p.context_character_budgets();
        // usable = 4096 - 256 = 3840; suffix = 3840*0.25 = 960; prefix = 2880
        assert_eq!(prefix, 2880 * CHARS_PER_TOKEN_ESTIMATE);
        assert_eq!(suffix, 960 * CHARS_PER_TOKEN_ESTIMATE);
    }

    #[test]
    fn context_character_budgets_zero_when_output_consumes_all() {
        let p = FimPreferences {
            max_context_tokens: 100,
            max_new_tokens: 200,
            ..FimPreferences::default()
        };
        let (prefix, suffix) = p.context_character_budgets();
        assert_eq!(prefix, 0);
        assert_eq!(suffix, 0);
    }

    #[test]
    fn clamp_prefix_truncates_trailing() {
        assert_eq!(FimPreferences::clamp_prefix("abcdefgh", 3), "fgh");
    }

    #[test]
    fn clamp_prefix_no_limit_when_zero() {
        assert_eq!(FimPreferences::clamp_prefix("abc", 0), "abc");
    }

    #[test]
    fn clamp_suffix_truncates_leading() {
        assert_eq!(FimPreferences::clamp_suffix("abcdefgh", 3), "abc");
    }

    #[test]
    fn render_prompt_substitutes_tokens_and_text() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Qwen,
            pattern_template: default_pattern_template(),
            ..FimPreferences::default()
        };
        let rendered = p.render_prompt("SELECT ", "* FROM t");
        assert!(rendered.starts_with("<|fim_prefix|>SELECT "));
        assert!(rendered.contains("<|fim_suffix|>* FROM t"));
        assert!(rendered.ends_with("<|fim_middle|>"));
    }

    #[test]
    fn render_prompt_custom_format_uses_empty_tokens() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Custom,
            pattern_template: "PRE[{}]SUF[{}]MID[{}]".to_string(),
            ..FimPreferences::default()
        };
        let rendered = p.render_prompt("a", "b");
        // Custom format has empty tokens, and the pattern has no placeholders,
        // so no substitution occurs.
        assert_eq!(rendered, "PRE[{}]SUF[{}]MID[{}]");
    }

    #[test]
    fn effective_stop_sequences_includes_structural_tokens() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Qwen,
            stop_on_newline: true,
            ..FimPreferences::default()
        };
        let stops = p.effective_stop_sequences();
        assert!(stops.contains(&"<|fim_prefix|>".to_string()));
        assert!(stops.contains(&"<|fim_suffix|>".to_string()));
        assert!(stops.contains(&"<|fim_middle|>".to_string()));
        assert!(stops.contains(&"\n".to_string()));
        assert!(stops.contains(&"\n\n".to_string()));
    }

    #[test]
    fn effective_stop_sequences_custom_format_no_structural() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Custom,
            ..FimPreferences::default()
        };
        let stops = p.effective_stop_sequences();
        // Custom format has empty tokens → only stop_sequences + newline
        assert!(!stops.iter().any(|s| s.starts_with("<|")));
    }

    #[test]
    fn effective_stop_sequences_dedupes() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Qwen,
            stop_sequences: vec!["<|fim_prefix|>".to_string()],
            ..FimPreferences::default()
        };
        let stops = p.effective_stop_sequences();
        assert_eq!(stops.iter().filter(|s| s == &"<|fim_prefix|>").count(), 1);
    }

    #[test]
    fn seed_new_tab_disabled_by_default() {
        assert!(!FimPreferences::default().seed_new_tab_enabled());
    }

    #[test]
    fn seed_new_tab_follows_global_enable() {
        let p = FimPreferences {
            enabled: true,
            ..FimPreferences::default()
        };
        assert!(p.seed_new_tab_enabled());
    }

    #[test]
    fn seed_new_tab_follows_new_query_opt_in() {
        let p = FimPreferences {
            enabled: false,
            enable_for_new_queries: true,
            ..FimPreferences::default()
        };
        assert!(p.seed_new_tab_enabled());
    }

    #[test]
    fn should_suppress_matches_token() {
        let p = FimPreferences::default();
        assert!(p.should_suppress("SELECT * //"));
        assert!(p.should_suppress("x #"));
    }

    #[test]
    fn should_suppress_ignores_trailing_whitespace() {
        let p = FimPreferences::default();
        assert!(p.should_suppress("SELECT //   "));
    }

    #[test]
    fn should_suppress_false_for_clean_prefix() {
        let p = FimPreferences::default();
        assert!(!p.should_suppress("SELECT * FROM "));
    }

    #[test]
    fn should_suppress_only_checks_current_line() {
        let p = FimPreferences::default();
        assert!(p.should_suppress("SELECT\n// "));
        assert!(!p.should_suppress("// \nSELECT"));
    }

    #[test]
    fn should_suppress_empty_tokens_never_suppresses() {
        let p = FimPreferences {
            disable_on_tokens: vec![],
            ..FimPreferences::default()
        };
        assert!(!p.should_suppress("anything // here"));
    }

    #[test]
    fn default_pattern_template_contains_placeholders() {
        let tmpl = default_pattern_template();
        assert!(tmpl.contains(placeholder::PREFIX));
        assert!(tmpl.contains(placeholder::SUFFIX));
        assert!(tmpl.contains(placeholder::MIDDLE));
        assert!(tmpl.contains(placeholder::PREFIX_TEXT));
        assert!(tmpl.contains(placeholder::SUFFIX_TEXT));
    }

    #[test]
    fn trim_leading_whitespace_strips_indent() {
        let p = FimPreferences {
            fim_format: FimPromptFormat::Qwen,
            trim_leading_whitespace: true,
            pattern_template: default_pattern_template(),
            ..FimPreferences::default()
        };
        let rendered = p.render_prompt("x", "    FROM t");
        // After stripping leading indent, the suffix text should be "FROM t"
        assert!(rendered.contains("FROM t"));
        assert!(!rendered.contains("    FROM t"));
    }

    #[test]
    fn fim_preferences_round_trip() {
        let p = FimPreferences {
            enabled: true,
            enable_for_new_queries: true,
            model: FimAiModel::Qwen25Coder3BBase,
            fim_format: FimPromptFormat::DeepSeek,
            trigger_kind: FimTriggerKind::Manual,
            generation_delay_secs: 1.25,
            disable_on_tokens: vec!["--".to_string()],
            system_prompt: "be terse".to_string(),
            pattern_template: "{{PREFIX}}{{prefix_text}}{{MIDDLE}}".to_string(),
            trim_leading_whitespace: true,
            max_context_tokens: 8192,
            suffix_percent: 0.4,
            temperature: 0.7,
            max_new_tokens: 512,
            stop_sequences: vec![";".to_string()],
            stop_on_newline: true,
        };
        let json = serde_json::to_vec(&p).unwrap();
        let decoded: FimPreferences = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn fim_preferences_default_round_trip() {
        let p = FimPreferences::default();
        let json = serde_json::to_vec(&p).unwrap();
        let decoded: FimPreferences = serde_json::from_slice(&json).unwrap();
        assert_eq!(decoded, p);
    }

    #[test]
    fn fim_preferences_empty_json_is_defaults() {
        // Struct-level #[serde(default)]: a prefs file written before any field
        // existed must still load, not error into a total reset.
        let decoded: FimPreferences = serde_json::from_str("{}").unwrap();
        assert_eq!(decoded, FimPreferences::default());
    }

    #[test]
    fn fim_preferences_partial_json_keeps_other_defaults() {
        let decoded: FimPreferences =
            serde_json::from_str(r#"{"enabled": true, "maxNewTokens": 64}"#).unwrap();
        assert!(decoded.enabled);
        assert_eq!(decoded.max_new_tokens, 64);
        let d = FimPreferences::default();
        assert_eq!(decoded.model, d.model);
        assert_eq!(decoded.temperature, d.temperature);
        assert_eq!(decoded.system_prompt, d.system_prompt);
    }

    #[test]
    fn fim_preferences_json_keys_are_camel_case() {
        let json = serde_json::to_string(&FimPreferences::default()).unwrap();
        for key in [
            "\"enabled\"",
            "\"enableForNewQueries\"",
            "\"model\"",
            "\"fimFormat\"",
            "\"triggerKind\"",
            "\"generationDelaySecs\"",
            "\"disableOnTokens\"",
            "\"systemPrompt\"",
            "\"patternTemplate\"",
            "\"trimLeadingWhitespace\"",
            "\"maxContextTokens\"",
            "\"suffixPercent\"",
            "\"temperature\"",
            "\"maxNewTokens\"",
            "\"stopSequences\"",
            "\"stopOnNewline\"",
        ] {
            assert!(json.contains(key), "missing key {key} in {json}");
        }
    }

    #[test]
    fn trim_prefix_repetition_drops_restated_where_clause() {
        // The reported bug: prefix already ends `where … ReportingPeriodId =47`
        // and the model re-emits that clause with different case/spacing.
        let prefix = "update\n    Accounting.ReportingPeriod\nset\n    IsOpen = 1\nwhere\n    ReportingPeriodId =47\n    ";
        let completion = "WHERE ReportingPeriodId = 47 AND IsOpen = 0";
        assert_eq!(
            trim_prefix_repetition(prefix, completion),
            " AND IsOpen = 0"
        );
    }

    #[test]
    fn trim_prefix_repetition_hides_pure_repetition() {
        let prefix = "select * from t where Id = 47";
        assert_eq!(trim_prefix_repetition(prefix, "WHERE Id = 47"), "");
    }

    #[test]
    fn trim_prefix_repetition_keeps_novel_completion() {
        let prefix = "SELECT * FROM users WHERE ";
        let completion = "id = 1";
        assert_eq!(trim_prefix_repetition(prefix, completion), completion);
    }

    #[test]
    fn trim_prefix_repetition_ignores_single_token_overlap() {
        // One shared token is coincidence, not clause repetition.
        let prefix = "SELECT * FROM orders LIMIT 1";
        let completion = "1 + 2";
        assert_eq!(trim_prefix_repetition(prefix, completion), completion);
    }

    #[test]
    fn trim_prefix_repetition_matches_across_case_and_whitespace() {
        let prefix = "where\n    a=1";
        assert_eq!(
            trim_prefix_repetition(prefix, "WHERE A = 1 OR b = 2"),
            " OR b = 2"
        );
    }

    #[test]
    fn trim_prefix_repetition_empty_inputs() {
        assert_eq!(trim_prefix_repetition("", "SELECT 1"), "SELECT 1");
        assert_eq!(trim_prefix_repetition("SELECT 1", ""), "");
    }

    #[test]
    fn trim_prefix_repetition_long_prefix_still_matches_tail() {
        let mut prefix = "x, ".repeat(2000);
        prefix.push_str("where Id = 47");
        assert_eq!(
            trim_prefix_repetition(&prefix, "where Id = 47 and b = 2"),
            " and b = 2"
        );
    }

    #[test]
    fn fim_malformed_json_falls_back_to_defaults() {
        let decoded: FimPreferences = serde_json::from_str("{ not json").unwrap_or_default();
        assert_eq!(decoded, FimPreferences::default());
    }
}
