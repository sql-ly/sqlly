use sqlly_gpui::cloud_ai::{
    AiProfilesConfig, CloudAIConfig, CloudAIProviderConfig, ProviderKind, ProviderProfile,
};
use std::collections::BTreeSet;

#[test]
fn profiles_json_never_contains_secret_field_names() {
    let profiles: Vec<ProviderProfile> = ProviderKind::all()
        .iter()
        .map(|kind| ProviderProfile {
            api_key_ref: "ai-profile.test".into(),
            ..ProviderProfile::new(*kind)
        })
        .collect();
    let config = AiProfilesConfig {
        version: 2,
        profiles,
        active_profile_id: None,
    };
    let json: serde_json::Value = serde_json::to_value(&config).unwrap();
    let keys: BTreeSet<String> = json["profiles"][0]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    let expected: BTreeSet<String> = [
        "id",
        "name",
        "kind",
        "baseUrl",
        "model",
        "apiKeyRef",
        "availableModelIds",
        "contextWindow",
        "reasoning",
        "thinkingBudget",
        "sendReasoningEffort",
        "proxy",
    ]
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
fn legacy_config_still_skips_api_key() {
    let config = CloudAIConfig {
        openai: CloudAIProviderConfig {
            api_key: "SENTINEL".into(),
            selected_model_id: Some("gpt".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains("SENTINEL"));
}
