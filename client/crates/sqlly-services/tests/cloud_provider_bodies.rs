use sqlly_ai::catalog::{ChatMessage, ChatRole, CompletionRequest};
use sqlly_services::providers::{CloudLlmProvider, CloudProviderSpec, CloudReasoning, CloudWire};

fn sample_request() -> CompletionRequest {
    CompletionRequest {
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
        max_tokens: Some(1024),
        temperature: Some(0.2),
        metadata: serde_json::Value::Null,
    }
}

#[test]
fn openai_body_with_effort() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::OpenAiChat,
        endpoint: "https://api.openai.com/v1/chat/completions".into(),
        headers: vec![],
        model: "gpt-5".into(),
        provider_id: "openai:gpt-5".into(),
        reasoning: CloudReasoning::OpenAiEffort("high"),
    })
    .unwrap();
    let body = provider.build_body(&sample_request());
    assert_eq!(body["reasoning_effort"], "high");
    assert_eq!(body["max_tokens"], 1024);
    assert_eq!(body["messages"][0]["role"], "system");
}

#[test]
fn openai_body_without_effort_has_no_reasoning_field() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::OpenAiChat,
        endpoint: "https://api.openai.com/v1/chat/completions".into(),
        headers: vec![],
        model: "gpt-5".into(),
        provider_id: "openai:gpt-5".into(),
        reasoning: CloudReasoning::None,
    })
    .unwrap();
    let body = provider.build_body(&sample_request());
    assert!(body.get("reasoning_effort").is_none());
}

#[test]
fn anthropic_body_adaptive_effort() {
    let spec = CloudProviderSpec {
        wire: CloudWire::AnthropicMessages,
        endpoint: "https://api.anthropic.com/v1/messages".into(),
        headers: vec![("anthropic-version".into(), "2023-06-01".into())],
        model: "claude".into(),
        provider_id: "anthropic:claude".into(),
        reasoning: CloudReasoning::AnthropicEffort("medium"),
    };
    let provider = CloudLlmProvider::new(spec.clone()).unwrap();
    let body = provider.build_body(&sample_request());
    assert_eq!(body["thinking"], serde_json::json!({"type": "adaptive"}));
    assert_eq!(
        body["output_config"],
        serde_json::json!({"effort": "medium"})
    );
    assert!(body.get("budget_tokens").is_none());
    assert_eq!(body["system"], "be terse");
    assert!(spec.headers.iter().any(|(k, _)| k == "anthropic-version"));
}

#[test]
fn anthropic_body_legacy_budget_clamps_max_tokens() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::AnthropicMessages,
        endpoint: "https://api.anthropic.com/v1/messages".into(),
        headers: vec![],
        model: "claude".into(),
        provider_id: "anthropic:claude".into(),
        reasoning: CloudReasoning::AnthropicBudget(4096),
    })
    .unwrap();
    let body = provider.build_body(&sample_request());
    assert_eq!(body["max_tokens"], 5120);
    assert_eq!(
        body["thinking"],
        serde_json::json!({"type": "enabled", "budget_tokens": 4096})
    );
}

#[test]
fn gemini_body_shape() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::GeminiGenerateContent,
        endpoint: "https://generativelanguage.googleapis.com/v1beta/models/gemini:generateContent"
            .into(),
        headers: vec![],
        model: "gemini".into(),
        provider_id: "gemini:gemini".into(),
        reasoning: CloudReasoning::GeminiBudget(-1),
    })
    .unwrap();
    let body = provider.build_body(&sample_request());
    assert_eq!(body["contents"][0]["role"], "user");
    assert_eq!(body["system_instruction"]["parts"][0]["text"], "be terse");
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
        -1
    );
}

#[test]
fn gemini_body_thinking_level() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::GeminiGenerateContent,
        endpoint: "https://generativelanguage.googleapis.com/v1beta/models/gemini:generateContent"
            .into(),
        headers: vec![],
        model: "gemini".into(),
        provider_id: "gemini:gemini".into(),
        reasoning: CloudReasoning::GeminiLevel("low"),
    })
    .unwrap();
    let body = provider.build_body(&sample_request());
    assert_eq!(
        body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
        "low"
    );
}

#[test]
fn extract_text_skips_thinking_blocks() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::AnthropicMessages,
        endpoint: "https://api.anthropic.com/v1/messages".into(),
        headers: vec![],
        model: "claude".into(),
        provider_id: "anthropic:claude".into(),
        reasoning: CloudReasoning::None,
    })
    .unwrap();
    let value: serde_json::Value = serde_json::from_str(
        r#"{"content":[{"type":"thinking","thinking":""},{"type":"text","text":"hi"}]}"#,
    )
    .unwrap();
    assert_eq!(
        provider.extract_response_text(&value).as_deref(),
        Some("hi")
    );
}

#[test]
fn extract_text_gemini() {
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::GeminiGenerateContent,
        endpoint: "https://generativelanguage.googleapis.com/v1beta/models/gemini:generateContent"
            .into(),
        headers: vec![],
        model: "gemini".into(),
        provider_id: "gemini:gemini".into(),
        reasoning: CloudReasoning::None,
    })
    .unwrap();
    let value: serde_json::Value =
        serde_json::from_str(r#"{"candidates":[{"content":{"parts":[{"text":"hi"}]}}]}"#).unwrap();
    assert_eq!(
        provider.extract_response_text(&value).as_deref(),
        Some("hi")
    );
}
