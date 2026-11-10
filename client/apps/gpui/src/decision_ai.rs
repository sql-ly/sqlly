//! Decision-model configuration persistence and provider construction.
//!
//! Mirrors `crate::cloud_ai`'s security boundary: the API key is a bearer
//! credential and lives only in the OS keychain (browser: host secret
//! storage), addressed by the fixed account name in the config's `apiKeyRef`.
//! Everything else lives in a plain JSON file next to the other preferences.
//! Nothing in this module may ever write a key to that file — the config
//! struct has no key field at all. Enforced by
//! `apps/gpui/tests/decision_ai_no_secrets.rs`.
//!
//! The decision model is opt-in and default off; `allow_row_data` separately
//! gates whether query results may ever be included in a decision state, and
//! also defaults off.

use sqlly_ai::catalog::DynDecisionProvider;

/// Keychain account (under `cloud_ai`'s `SQLLY.cloud-ai` service) holding the
/// Jev API key.
pub const DECISION_KEY_ACCOUNT: &str = "decision-ai.jev";

/// Persisted decision-model settings. If you add a field here, assume it
/// lands on disk in plaintext.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct DecisionAiConfig {
    /// Master switch; nothing is sent anywhere while this is off.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Keychain account name the key is stored under — never the key itself.
    #[serde(default = "default_api_key_ref")]
    pub api_key_ref: String,
    /// Whether query result data may be included in decision states.
    /// Schema names and SQL text are always allowed once `enabled` is on.
    #[serde(default)]
    pub allow_row_data: bool,
}

fn default_base_url() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        sqlly_ai::jev::DEFAULT_BASE_URL.to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        "https://api.typesafe.ai".to_string()
    }
}

fn default_model() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    {
        sqlly_ai::jev::DEFAULT_MODEL.to_string()
    }
    #[cfg(target_arch = "wasm32")]
    {
        "jev-1.13.0".to_string()
    }
}

fn default_api_key_ref() -> String {
    DECISION_KEY_ACCOUNT.to_string()
}

impl Default for DecisionAiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: default_base_url(),
            model: default_model(),
            api_key_ref: default_api_key_ref(),
            allow_row_data: false,
        }
    }
}

pub fn decision_ai_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("decision-ai.json");
    p
}

pub fn load_decision_ai_config() -> DecisionAiConfig {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(text) = crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "decision-ai.json",
        ) && let Ok(config) = serde_json::from_str(&text)
        {
            return config;
        }
        DecisionAiConfig::default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = decision_ai_path();
        if path.exists()
            && let Ok(data) = std::fs::read(&path)
            && let Ok(config) = serde_json::from_slice(&data)
        {
            return config;
        }
        DecisionAiConfig::default()
    }
}

pub fn save_decision_ai_config(config: &DecisionAiConfig) -> Result<(), String> {
    let text =
        crate::config_version::to_string_pretty(config).map_err(|error| error.to_string())?;
    #[cfg(target_arch = "wasm32")]
    {
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "decision-ai.json",
            &text,
        )
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::fs::write(decision_ai_path(), text).map_err(|error| error.to_string())
    }
}

/// The host the decision model talks to, for UI egress disclosure.
pub fn egress_label() -> String {
    crate::cloud_ai::host_from_base_url(&load_decision_ai_config().base_url)
}

/// The configured decision provider, or `None` when the feature is off or not
/// fully configured. Never errors: a decision model is always optional, and
/// callers fall back to the LLM adapter or skip the feature.
pub fn decision_provider() -> Option<DynDecisionProvider> {
    #[cfg(target_arch = "wasm32")]
    {
        // The Jev client is native-only; the browser build has no decision
        // provider until the relay learns to proxy one.
        None
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        decision_provider_for(&load_decision_ai_config())
    }
}

/// [`decision_provider`] for an explicit config. The keychain is consulted
/// only when `enabled` is on, so a disabled config never prompts for
/// keychain access.
#[cfg(not(target_arch = "wasm32"))]
pub fn decision_provider_for(config: &DecisionAiConfig) -> Option<DynDecisionProvider> {
    if !config.enabled {
        return None;
    }
    let api_key = match crate::cloud_ai::load_api_key_ref(&config.api_key_ref) {
        Ok(Some(key)) if !key.is_empty() => key,
        _ => return None,
    };
    let jev_config = sqlly_ai::jev::JevConfig {
        base_url: config.base_url.clone(),
        model: config.model.clone(),
        ..sqlly_ai::jev::JevConfig::default()
    };
    let client = sqlly_ai::jev::JevClient::new(&jev_config, api_key).ok()?;
    Some(std::sync::Arc::new(client) as DynDecisionProvider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_off_and_pinned() {
        let config = DecisionAiConfig::default();
        assert!(!config.enabled);
        assert!(!config.allow_row_data);
        assert_eq!(config.model, "jev-1.13.0");
        assert_eq!(config.api_key_ref, DECISION_KEY_ACCOUNT);
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = DecisionAiConfig {
            enabled: true,
            allow_row_data: true,
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: DecisionAiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, back);
    }
}
