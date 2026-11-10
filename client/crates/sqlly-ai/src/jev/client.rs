//! HTTP client for the Jev decision model (TypeSafe AI `systemone` endpoint).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::catalog::{
    Answer, DecisionError, DecisionProvider, DecisionRequest, DecisionResponse, Question,
};
use crate::ollama::ensure_rustls_provider;

use super::error::JevError;

/// Default API host. Override via [`JevConfig::base_url`] for tests.
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";

/// Pinned model id. `jev-latest` changes behaviour silently; never default to it.
pub const DEFAULT_MODEL: &str = "jev-1.13.0";

/// Default per-request timeout. The vendor claims 70–500 ms; 3 s leaves room
/// for a slow network without hanging an in-loop caller.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);

/// How long to wait before the single retry on HTTP 429/529.
const RETRY_DELAY: Duration = Duration::from_millis(250);

/// Provider identifier for [`JevClient`].
pub const JEV_PROVIDER_ID: &str = "jev";

/// Connection settings for [`JevClient`]. The API key is deliberately not part
/// of the config: it lives in the OS keychain and is handed to
/// [`JevClient::new`] at construction time only.
#[derive(Debug, Clone)]
pub struct JevConfig {
    pub base_url: String,
    pub model: String,
    pub timeout: Duration,
}

impl Default for JevConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            model: DEFAULT_MODEL.to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// A reusable client bound to one Jev endpoint and model.
pub struct JevClient {
    http: reqwest::Client,
    base_url: String,
    model: String,
    api_key: String,
}

/// `Debug` must never leak the API key.
impl std::fmt::Debug for JevClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JevClient")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl JevClient {
    /// Construct a client. Does no network I/O; it only builds the underlying
    /// `reqwest::Client`.
    pub fn new(config: &JevConfig, api_key: String) -> Result<Self, JevError> {
        ensure_rustls_provider();
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .map_err(|err| JevError::Build(err.to_string()))?;
        Ok(Self {
            http,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}/v1/systemone", self.base_url)
    }

    /// Serialize a request into the documented wire format. `Noul` sends no
    /// `criteria`.
    fn wire_body(&self, request: &DecisionRequest) -> serde_json::Value {
        let mut questions = serde_json::Map::new();
        for (id, question) in &request.questions {
            let wire = match question {
                Question::Choice {
                    instructions,
                    options,
                } => {
                    let mut criteria = serde_json::Map::new();
                    for (option_id, description) in options {
                        let value = match description {
                            Some(text) => serde_json::Value::String(text.clone()),
                            None => serde_json::Value::Null,
                        };
                        criteria.insert(option_id.clone(), value);
                    }
                    serde_json::json!({
                        "type": "choice",
                        "instructions": instructions,
                        "criteria": criteria,
                    })
                }
                Question::Score {
                    instructions,
                    levels,
                } => serde_json::json!({
                    "type": "score",
                    "instructions": instructions,
                    "criteria": levels,
                }),
                Question::Noul { instructions } => serde_json::json!({
                    "type": "noul",
                    "instructions": instructions,
                }),
            };
            questions.insert(id.clone(), wire);
        }
        serde_json::json!({
            "model": self.model,
            "state": request.state,
            "questions": questions,
        })
    }

    /// One POST to the endpoint. Errors carry no request state and no key.
    async fn post_once(&self, body: &serde_json::Value) -> Result<reqwest::Response, JevError> {
        let url = self.endpoint();
        self.http
            .post(&url)
            .bearer_auth(&self.api_key)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(|err| JevError::Transport {
                url: url.clone(),
                // reqwest error strings do not include the request body or
                // headers, so nothing sensitive can leak here.
                reason: err.to_string(),
            })
    }

    async fn decode(
        &self,
        request: &DecisionRequest,
        body: &str,
    ) -> Result<DecisionResponse, JevError> {
        let wire: WireResponse = serde_json::from_str(body).map_err(|err| JevError::Decode {
            message: err.to_string(),
        })?;
        let mut answers = BTreeMap::new();
        for (id, question) in &request.questions {
            let wire_answer = wire.answers.get(id).ok_or_else(|| JevError::Decode {
                message: format!("answer for question '{id}' is missing"),
            })?;
            let answer = match (question, wire_answer) {
                (
                    Question::Choice { options, .. },
                    WireAnswer::Choice {
                        choice,
                        probabilities,
                        confidence,
                    },
                ) => {
                    if !options.iter().any(|(option_id, _)| option_id == choice) {
                        return Err(JevError::Decode {
                            message: format!(
                                "question '{id}' answered with unknown option '{choice}'"
                            ),
                        });
                    }
                    Answer::Choice {
                        choice: choice.clone(),
                        probabilities: probabilities.clone(),
                        confidence: *confidence,
                    }
                }
                (
                    Question::Score { .. },
                    WireAnswer::Score {
                        score, confidence, ..
                    },
                ) => Answer::Score {
                    score: *score,
                    confidence: *confidence,
                },
                (Question::Noul { .. }, WireAnswer::Noul { noul }) => {
                    Answer::Noul { probability: *noul }
                }
                (_, other) => {
                    return Err(JevError::Decode {
                        message: format!(
                            "question '{id}' answered with mismatched type '{}'",
                            other.type_name()
                        ),
                    });
                }
            };
            answers.insert(id.clone(), answer);
        }
        Ok(DecisionResponse {
            answers,
            input_tokens: wire.usage.map(|u| u.input_tokens).unwrap_or(0),
        })
    }
}

#[async_trait]
impl DecisionProvider for JevClient {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        request.validate()?;
        let body = self.wire_body(&request);

        let mut response = self.post_once(&body).await?;
        if matches!(response.status().as_u16(), 429 | 529) {
            // Back off once, per the vendor's guidance, then give up.
            tokio::time::sleep(RETRY_DELAY).await;
            response = self.post_once(&body).await?;
            if matches!(response.status().as_u16(), 429 | 529) {
                return Err(DecisionError::RateLimited);
            }
        }

        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|err| DecisionError::Transport(format!("reading Jev response body: {err}")))?;
        if !(200..300).contains(&status) {
            return Err(DecisionError::Status {
                code: status,
                body: text,
            });
        }
        self.decode(&request, &text).await.map_err(Into::into)
    }

    fn provider_id(&self) -> &str {
        JEV_PROVIDER_ID
    }
}

/// The documented response shape, decoded by the `type` tag per answer.
#[derive(serde::Deserialize)]
struct WireResponse {
    answers: BTreeMap<String, WireAnswer>,
    usage: Option<WireUsage>,
}

#[derive(serde::Deserialize)]
struct WireUsage {
    input_tokens: u64,
}

#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum WireAnswer {
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f32>,
        confidence: f32,
    },
    Score {
        score: f32,
        confidence: f32,
    },
    Noul {
        noul: f32,
    },
}

impl WireAnswer {
    fn type_name(&self) -> &'static str {
        match self {
            WireAnswer::Choice { .. } => "choice",
            WireAnswer::Score { .. } => "score",
            WireAnswer::Noul { .. } => "noul",
        }
    }
}
