//! sqlly-ollama: local [Ollama](https://ollama.com) implementations of the
//! `sqlly-catalog` provider traits.
//!
//! The catalog talks to models only through `LlmProvider` and
//! `EmbeddingProvider`. This crate provides Ollama-backed implementations of
//! both so installed local models can power schema Q&A (`ask_schema`), the AI
//! card cache (`build_ai_cache`), and vector search (`build_embeddings` /
//! `search_schema`) — without baking any HTTP/model code into the core catalog.
//!
//! Selection is the engine's job: it reads `SQLLY_*` environment variables and
//! constructs these providers, falling back to the deterministic catalog stubs
//! when local AI is not configured or not reachable.
//!
//! ```no_run
//! # async fn demo() -> Result<(), Box<dyn std::error::Error>> {
//! use sqlly_ai::ollama::{OllamaConfig, OllamaLlmProvider, OllamaEmbeddingProvider};
//!
//! let config = OllamaConfig::from_env();
//! let llm = OllamaLlmProvider::new(&config, "qwen2.5-coder")?;
//! let embed = OllamaEmbeddingProvider::connect(&config, "nomic-embed-text").await?;
//! # let _ = (llm, embed);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]

mod client;
mod embed;
mod llm;

pub(crate) use client::ensure_rustls_provider;

pub use client::{
    BASE_URL_ENV, ChatMessage, ChatOptions, ChatRequest, ChatResponse, ChatResponseMessage,
    DEFAULT_BASE_URL, DEFAULT_PROBE_TIMEOUT_SECS, DEFAULT_TIMEOUT_SECS, OllamaClient, OllamaConfig,
    OllamaError, PROBE_TIMEOUT_SECS_ENV, TIMEOUT_SECS_ENV,
};
pub use embed::OllamaEmbeddingProvider;
pub use llm::{DEFAULT_NUM_CTX, DEFAULT_TEMPERATURE, OllamaLlmProvider};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{
        AssembledContext, EmbeddingProvider, LlmPrompt, LlmProvider, SchemaPurpose,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// A canned HTTP response the mock server returns for each request.
    #[derive(Clone)]
    struct Canned {
        status_line: &'static str,
        body: &'static str,
    }

    /// Spawn a mock HTTP server that answers every connection with the supplied
    /// sequence of canned responses (cycling on the last one). Returns the base
    /// URL and a counter of how many requests were served.
    ///
    /// We hand-roll a tiny HTTP/1.1 responder rather than pull in a mock-server
    /// crate: it keeps the dependency footprint at zero and we only need to send
    /// a fixed body back, ignoring the request contents.
    async fn mock_server(responses: Vec<Canned>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let served = count.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                // Drain whatever the client sent; we don't need to parse it.
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;

                let idx = served.fetch_add(1, Ordering::SeqCst);
                let canned = responses
                    .get(idx)
                    .or_else(|| responses.last())
                    .cloned()
                    .expect("at least one canned response");
                let response = format!(
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    canned.status_line,
                    canned.body.len(),
                    canned.body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
            }
        });
        (format!("http://{addr}"), count)
    }

    fn config_for(base_url: String) -> OllamaConfig {
        OllamaConfig {
            base_url,
            timeout: std::time::Duration::from_secs(5),
            probe_timeout: std::time::Duration::from_secs(5),
        }
    }

    fn test_prompt(question: &str) -> LlmPrompt {
        LlmPrompt {
            question: question.to_string(),
            context: AssembledContext {
                text: "dbo.users(id INT, email NVARCHAR)".to_string(),
                tokens: 12,
                object_keys: vec!["dbo.users".to_string()],
                used_prebuilt_cards: 0,
                purpose: SchemaPurpose::default(),
            },
            system: Some("You are a SQL Server assistant.".to_string()),
        }
    }

    #[tokio::test]
    async fn chat_success_maps_to_llm_response() {
        let body = r#"{"message":{"role":"assistant","content":"The users table has id and email."},"prompt_eval_count":42,"eval_count":9}"#;
        let (base_url, count) = mock_server(vec![Canned {
            status_line: "200 OK",
            body,
        }])
        .await;

        let provider = OllamaLlmProvider::new(&config_for(base_url), "qwen2.5-coder").unwrap();
        let response = provider
            .ask(test_prompt("what columns does users have"))
            .await
            .unwrap();

        assert_eq!(response.text, "The users table has id and email.");
        assert_eq!(response.meta["provider"], "ollama");
        assert_eq!(response.meta["model"], "qwen2.5-coder");
        assert_eq!(response.meta["prompt_eval_count"], 42);
        assert_eq!(response.meta["eval_count"], 9);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn llm_provider_id_includes_model() {
        let provider =
            OllamaLlmProvider::new(&config_for("http://127.0.0.1:1".to_string()), "qwen3:4b")
                .unwrap();
        assert_eq!(provider.provider_id(), "ollama:qwen3:4b");
    }

    #[tokio::test]
    async fn chat_http_error_is_actionable() {
        let (base_url, _) = mock_server(vec![Canned {
            status_line: "404 Not Found",
            body: r#"{"error":"model 'missing' not found"}"#,
        }])
        .await;

        let provider = OllamaLlmProvider::new(&config_for(base_url), "missing").unwrap();
        let err = provider
            .ask(test_prompt("anything"))
            .await
            .expect_err("404 should surface as an error");
        let message = err.to_string();
        assert!(
            message.contains("404"),
            "message should mention status: {message}"
        );
        assert!(
            message.contains("not found"),
            "message should include server body: {message}"
        );
    }

    #[tokio::test]
    async fn chat_connection_refused_is_actionable() {
        // Port 1 is reserved and never listening: forces a transport error.
        let provider =
            OllamaLlmProvider::new(&config_for("http://127.0.0.1:1".to_string()), "qwen").unwrap();
        let err = provider
            .ask(test_prompt("anything"))
            .await
            .expect_err("connection refused should error");
        assert!(
            err.to_string().contains("Ollama"),
            "transport error should mention Ollama: {err}"
        );
    }

    #[tokio::test]
    async fn embed_success_maps_to_vector_and_dims() {
        // First response is the construction-time dims probe; subsequent ones
        // serve real embed calls. `/api/embed` returns `embeddings` (an array
        // of vectors), one per input.
        let body = r#"{"embeddings":[[0.1,0.2,0.3,0.4]]}"#;
        let (base_url, count) = mock_server(vec![Canned {
            status_line: "200 OK",
            body,
        }])
        .await;

        let provider = OllamaEmbeddingProvider::connect(&config_for(base_url), "nomic-embed-text")
            .await
            .unwrap();

        assert_eq!(provider.dims(), 4);
        assert_eq!(provider.provider_id(), "ollama:nomic-embed-text");

        let vector = provider.embed("customers and orders").await.unwrap();
        assert_eq!(vector, vec![0.1, 0.2, 0.3, 0.4]);
        // One probe at construction + one real embed call.
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn embed_batch_preserves_order() {
        // A 3-input batch comes back as 3 vectors in one `/api/embed` request.
        let body = r#"{"embeddings":[[1.0,0.0],[1.0,0.0],[1.0,0.0]]}"#;
        let (base_url, count) = mock_server(vec![Canned {
            status_line: "200 OK",
            body,
        }])
        .await;

        let provider = OllamaEmbeddingProvider::connect(&config_for(base_url), "nomic-embed-text")
            .await
            .unwrap();
        let texts = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let batch = provider.embed_batch(&texts).await.unwrap();
        assert_eq!(batch.len(), 3);
        for v in &batch {
            assert_eq!(v, &vec![1.0, 0.0]);
        }
        // One probe at construction + a single batched request for all 3 inputs
        // (proves batching, not one request per item).
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn embed_empty_vector_fails_construction() {
        let (base_url, _) = mock_server(vec![Canned {
            status_line: "200 OK",
            body: r#"{"embeddings":[]}"#,
        }])
        .await;

        let err = OllamaEmbeddingProvider::connect(&config_for(base_url), "not-an-embedder")
            .await
            .expect_err("empty embedding should fail construction");
        assert!(
            err.to_string().contains("empty"),
            "error should explain empty vector: {err}"
        );
    }

    #[tokio::test]
    async fn embed_probe_http_error_fails_construction() {
        // A non-2xx on the construction-time probe (e.g. model not installed)
        // must fail construction so the engine falls back to the stub.
        let (base_url, _) = mock_server(vec![Canned {
            status_line: "404 Not Found",
            body: r#"{"error":"model 'missing-embed' not found"}"#,
        }])
        .await;

        let err = OllamaEmbeddingProvider::connect(&config_for(base_url), "missing-embed")
            .await
            .expect_err("404 probe should fail construction");
        let message = err.to_string();
        assert!(message.contains("404"), "should mention status: {message}");
        assert!(
            message.contains("not found"),
            "should include server body: {message}"
        );
    }

    #[tokio::test]
    async fn embed_connection_refused_fails_construction() {
        let err = OllamaEmbeddingProvider::connect(
            &config_for("http://127.0.0.1:1".to_string()),
            "nomic-embed-text",
        )
        .await
        .expect_err("connection refused should fail construction");
        assert!(err.to_string().contains("Ollama"), "transport error: {err}");
    }
}
