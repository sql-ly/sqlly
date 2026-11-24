//! Cloud AI providers are called from native code, not from a browser, so a
//! provider that refuses cross-origin (CORS) requests still answers SQLly.

use std::sync::{Arc, Mutex};

use sqlly_ai::catalog::{ChatMessage, ChatRole, CompletionRequest, LlmProvider};
use sqlly_services::providers::{CloudLlmProvider, CloudProviderSpec, CloudReasoning, CloudWire};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// A provider endpoint with no CORS support: it sends no
/// `Access-Control-Allow-Origin` header and turns away any request that carries
/// an `Origin` header, the way browser-origin calls are refused. Records the
/// raw request head of every call it serves.
async fn cors_hostile_provider() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(Mutex::new(Vec::new()));
    let recorded = heads.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buf = [0u8; 4096];
            let head_end = loop {
                let read = socket.read(&mut buf).await.unwrap_or(0);
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(pos) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(pos + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
            let content_length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while request.len() < head_end + content_length {
                let read = socket.read(&mut buf).await.unwrap_or(0);
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buf[..read]);
            }
            let has_origin = head.lines().any(|line| line.starts_with("origin:"));
            recorded.lock().unwrap().push(head);
            let (status, body) = if has_origin {
                (
                    "403 Forbidden",
                    r#"{"error":{"message":"CORS requests are not allowed for this endpoint"}}"#,
                )
            } else {
                (
                    "200 OK",
                    r#"{"choices":[{"message":{"role":"assistant","content":"The sky scatters blue light."}}]}"#,
                )
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });
    (format!("http://{addr}/v1/chat/completions"), heads)
}

// https://github.com/beekeeper-studio/beekeeper-studio/issues/5013
#[tokio::test]
async fn cloud_provider_call_sends_no_origin_and_needs_no_cors() {
    let (endpoint, heads) = cors_hostile_provider().await;
    let provider = CloudLlmProvider::new(CloudProviderSpec {
        wire: CloudWire::OpenAiChat,
        endpoint,
        headers: vec![("authorization".into(), "Bearer test-key".into())],
        model: "openai/gpt-6-astra".into(),
        provider_id: "gateway:openai/gpt-6-astra".into(),
        reasoning: CloudReasoning::None,
    })
    .unwrap();

    let response = provider
        .complete(CompletionRequest {
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "Why is the sky blue?".into(),
            }],
            model: None,
            capability: None,
            max_tokens: Some(64),
            temperature: None,
            metadata: serde_json::Value::Null,
        })
        .await
        .expect("a provider without CORS support must still answer a native client");

    assert_eq!(response.text, "The sky scatters blue light.");
    let heads = heads.lock().unwrap();
    assert_eq!(heads.len(), 1, "exactly one provider request");
    assert!(
        !heads[0].lines().any(|line| line.starts_with("origin:")),
        "native provider calls must not carry a browser Origin header: {}",
        heads[0]
    );
    assert!(
        heads[0].contains("authorization: bearer test-key"),
        "auth header from the spec is sent: {}",
        heads[0]
    );
}
