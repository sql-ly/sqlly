//! sqlly-jev: the Jev decision-model implementation of
//! [`crate::catalog::DecisionProvider`].
//!
//! Jev (TypeSafe AI) is not a chat model: it takes a JSON state plus named
//! questions and returns typed answers with probabilities in one parallel
//! pass. It cannot generate text, so it lives behind the decision seam and is
//! never registered as a chat provider. Native-only: it speaks HTTPS via
//! `reqwest`, which does not exist in the browser build.

#![forbid(unsafe_code)]

mod client;
mod error;

pub use client::{
    DEFAULT_BASE_URL, DEFAULT_MODEL, DEFAULT_TIMEOUT, JEV_PROVIDER_ID, JevClient, JevConfig,
};
pub use error::JevError;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{Answer, DecisionError, DecisionProvider, DecisionRequest, Question};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
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
    /// URL, a counter of served requests, and the captured raw request texts.
    ///
    /// Hand-rolled like `crate::ollama`'s mock: the workspace deliberately has
    /// no mock-server crate.
    async fn mock_server(
        responses: Vec<Canned>,
    ) -> (String, Arc<AtomicUsize>, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let served = count.clone();
        let captured = requests.clone();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => return,
                };
                let mut buf = [0u8; 16384];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                captured
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&buf[..n]).to_string());

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
        (format!("http://{addr}"), count, requests)
    }

    fn client_for(base_url: String) -> JevClient {
        let config = JevConfig {
            base_url,
            ..JevConfig::default()
        };
        JevClient::new(&config, "sk-test-secret".to_string()).unwrap()
    }

    fn three_question_request() -> DecisionRequest {
        let mut questions = BTreeMap::new();
        questions.insert(
            "pick".to_string(),
            Question::Choice {
                instructions: "Which option?".to_string(),
                options: vec![
                    ("option1".to_string(), Some("First".to_string())),
                    ("option2".to_string(), None),
                ],
            },
        );
        questions.insert(
            "rate".to_string(),
            Question::Score {
                instructions: "Rate this".to_string(),
                levels: vec![
                    "Level 0".to_string(),
                    "Level 1".to_string(),
                    "Level 2".to_string(),
                ],
            },
        );
        questions.insert(
            "urgent".to_string(),
            Question::Noul {
                instructions: "Does this convey urgency?".to_string(),
            },
        );
        DecisionRequest {
            state: serde_json::json!({"subject": "quarterly report"}),
            questions,
        }
    }

    const FULL_BODY: &str = r#"{"model":"jev-1.13.0","answers":{
        "pick":{"type":"choice","choice":"option1","probabilities":{"option1":0.85,"option2":0.15},"confidence":0.82},
        "rate":{"type":"score","score":1.6,"legend":{"0":"Level 0","1":"Level 1","2":"Level 2"},"probabilities":{"0":0.05,"1":0.3,"2":0.65},"confidence":0.78},
        "urgent":{"type":"noul","noul":0.92}
    },"usage":{"input_tokens":312,"output_tokens":48}}"#;

    #[tokio::test]
    async fn decodes_all_three_answer_types() {
        let (base_url, _count, _requests) = mock_server(vec![Canned {
            status_line: "200 OK",
            body: FULL_BODY,
        }])
        .await;

        let client = client_for(base_url);
        let response = client.decide(three_question_request()).await.unwrap();

        match &response.answers["pick"] {
            Answer::Choice {
                choice,
                probabilities,
                confidence,
            } => {
                assert_eq!(choice, "option1");
                assert!((probabilities["option2"] - 0.15).abs() < f32::EPSILON);
                assert!((confidence - 0.82).abs() < f32::EPSILON);
            }
            other => panic!("expected choice answer, got {other:?}"),
        }
        match &response.answers["rate"] {
            Answer::Score { score, confidence } => {
                assert!((score - 1.6).abs() < f32::EPSILON);
                assert!((confidence - 0.78).abs() < f32::EPSILON);
            }
            other => panic!("expected score answer, got {other:?}"),
        }
        assert!(response.answers["urgent"].noul_at_least(0.9));
        assert_eq!(response.input_tokens, 312);
    }

    #[tokio::test]
    async fn sends_bearer_and_pinned_model() {
        let (base_url, count, requests) = mock_server(vec![Canned {
            status_line: "200 OK",
            body: FULL_BODY,
        }])
        .await;

        let client = client_for(base_url);
        client.decide(three_question_request()).await.unwrap();

        assert_eq!(count.load(Ordering::SeqCst), 1);
        let captured = requests.lock().unwrap();
        let request_text = &captured[0];
        assert!(request_text.starts_with("POST /v1/systemone HTTP/1.1"));
        assert!(request_text.contains("authorization: Bearer sk-test-secret"));
        // Assert on the parsed body, not substrings: JSON key order is not part
        // of the wire contract (and flipped when gpui-pre turned on
        // serde_json's `preserve_order` feature for the whole graph).
        let body_text = request_text.split("\r\n\r\n").nth(1).unwrap();
        let body: serde_json::Value = serde_json::from_str(body_text).unwrap();
        assert_eq!(body["model"], "jev-1.13.0");
        // The wire body carries the state, the choice criteria map, the score
        // criteria array, and no criteria on the noul question.
        assert_eq!(body["state"]["subject"], "quarterly report");
        let questions = &body["questions"];
        assert_eq!(questions["pick"]["type"], "choice");
        assert_eq!(questions["pick"]["criteria"]["option1"], "First");
        assert_eq!(
            questions["pick"]["criteria"]
                .get("option2")
                .map(serde_json::Value::is_null),
            Some(true)
        );
        assert_eq!(
            questions["rate"]["criteria"],
            serde_json::json!(["Level 0", "Level 1", "Level 2"])
        );
        assert_eq!(questions["urgent"]["type"], "noul");
        assert_eq!(
            questions["urgent"]["instructions"],
            "Does this convey urgency?"
        );
        assert!(questions["urgent"].get("criteria").is_none());
    }

    #[tokio::test]
    async fn retries_once_on_429_then_rate_limited() {
        let (base_url, count, _requests) = mock_server(vec![
            Canned {
                status_line: "429 Too Many Requests",
                body: r#"{"error":"rate limit"}"#,
            },
            Canned {
                status_line: "429 Too Many Requests",
                body: r#"{"error":"rate limit"}"#,
            },
        ])
        .await;

        let client = client_for(base_url);
        let err = client.decide(three_question_request()).await.unwrap_err();
        assert!(matches!(err, DecisionError::RateLimited));
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retry_succeeds_after_single_429() {
        let (base_url, count, _requests) = mock_server(vec![
            Canned {
                status_line: "429 Too Many Requests",
                body: r#"{"error":"rate limit"}"#,
            },
            Canned {
                status_line: "200 OK",
                body: FULL_BODY,
            },
        ])
        .await;

        let client = client_for(base_url);
        let response = client.decide(three_question_request()).await.unwrap();
        assert_eq!(response.answers.len(), 3);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn status_errors_carry_code_and_body() {
        let (base_url, _count, _requests) = mock_server(vec![Canned {
            status_line: "401 Unauthorized",
            body: r#"{"error":"bad key"}"#,
        }])
        .await;

        let client = client_for(base_url);
        let err = client.decide(three_question_request()).await.unwrap_err();
        match err {
            DecisionError::Status { code, body } => {
                assert_eq!(code, 401);
                assert!(body.contains("bad key"));
            }
            other => panic!("expected status error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unknown_choice_is_decode_error() {
        let body = r#"{"model":"jev-1.13.0","answers":{
            "pick":{"type":"choice","choice":"option9","probabilities":{"option9":1.0},"confidence":0.9},
            "rate":{"type":"score","score":1.0,"probabilities":{"1":1.0},"confidence":0.9},
            "urgent":{"type":"noul","noul":0.5}
        },"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (base_url, _count, _requests) = mock_server(vec![Canned {
            status_line: "200 OK",
            body,
        }])
        .await;

        let client = client_for(base_url);
        let err = client.decide(three_question_request()).await.unwrap_err();
        assert!(matches!(err, DecisionError::Decode(_)));
    }

    #[tokio::test]
    async fn missing_answer_is_decode_error() {
        let body = r#"{"model":"jev-1.13.0","answers":{
            "urgent":{"type":"noul","noul":0.5}
        },"usage":{"input_tokens":10,"output_tokens":1}}"#;
        let (base_url, _count, _requests) = mock_server(vec![Canned {
            status_line: "200 OK",
            body,
        }])
        .await;

        let client = client_for(base_url);
        let err = client.decide(three_question_request()).await.unwrap_err();
        assert!(matches!(err, DecisionError::Decode(_)));
    }

    #[test]
    fn debug_redacts_key() {
        let client = JevClient::new(&JevConfig::default(), "sk-very-secret".to_string()).unwrap();
        let debug = format!("{client:?}");
        assert!(!debug.contains("sk-very-secret"));
        assert!(debug.contains("<redacted>"));
        assert_eq!(client.provider_id(), JEV_PROVIDER_ID);
    }
}
