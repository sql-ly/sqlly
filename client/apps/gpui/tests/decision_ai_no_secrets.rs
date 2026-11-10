use sqlly_gpui::decision_ai::{DECISION_KEY_ACCOUNT, DecisionAiConfig, decision_provider_for};
use std::collections::BTreeSet;

#[test]
fn decision_config_json_never_contains_secret_field_names() {
    let config = DecisionAiConfig::default();
    let json: serde_json::Value = serde_json::to_value(&config).unwrap();
    let keys: BTreeSet<String> = json.as_object().unwrap().keys().cloned().collect();
    let expected: BTreeSet<String> = ["enabled", "baseUrl", "model", "apiKeyRef", "allowRowData"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(keys, expected);
    let serialized = serde_json::to_string(&config).unwrap();
    assert!(!serialized.contains("\"apiKey\""));
    assert!(!serialized.contains("\"secret\""));
    assert!(!serialized.contains("\"token\""));
}

#[test]
fn config_round_trips_and_defaults_off() {
    let config = DecisionAiConfig::default();
    assert!(!config.enabled);
    assert!(!config.allow_row_data);
    assert_eq!(config.api_key_ref, DECISION_KEY_ACCOUNT);
    let json = serde_json::to_string(&config).unwrap();
    let back: DecisionAiConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(config, back);
}

#[test]
fn decision_provider_is_none_when_disabled() {
    // A disabled config must yield no provider — and must not touch the
    // keychain (which would prompt on macOS test runs).
    let config = DecisionAiConfig::default();
    assert!(decision_provider_for(&config).is_none());
}
