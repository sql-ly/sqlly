//! The decision-model seam: typed one-of-N / score / yes-no answers.
//!
//! Chat models (`LlmProvider`) generate text; a decision model answers named,
//! typed questions about a piece of state in one fast parallel pass. Features
//! that need an in-loop judgment (re-ranking candidates, verifying SQL,
//! classifying columns) talk to [`DecisionProvider`] and never to a concrete
//! vendor. The Jev HTTP client lives in `crate::jev` (native only); the
//! [`LlmDecisionProvider`] fallback adapts any chat provider so features work
//! without a decision vendor configured.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use super::llm::{ChatMessage, ChatRole, CompletionRequest, DynLlmProvider};

/// One named question about the request state.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Question {
    /// Pick one of 2..=255 options. Each option is `(option id, description)`;
    /// the description may be `None` when the id speaks for itself.
    Choice {
        instructions: String,
        options: Vec<(String, Option<String>)>,
    },
    /// Rate on 2..=10 ordered levels, lowest first.
    Score {
        instructions: String,
        levels: Vec<String>,
    },
    /// A yes/no probability.
    Noul { instructions: String },
}

/// The typed answer to one [`Question`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Answer {
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
        probability: f32,
    },
}

impl Answer {
    /// True when this is a [`Answer::Noul`] whose probability meets `threshold`.
    /// Any other answer type returns false.
    pub fn noul_at_least(&self, threshold: f32) -> bool {
        match self {
            Answer::Noul { probability } => *probability >= threshold,
            _ => false,
        }
    }

    /// The chosen option id when this is a [`Answer::Choice`] whose confidence
    /// meets `min_confidence`; `None` otherwise.
    pub fn confident_choice(&self, min_confidence: f32) -> Option<&str> {
        match self {
            Answer::Choice {
                choice, confidence, ..
            } if *confidence >= min_confidence => Some(choice.as_str()),
            _ => None,
        }
    }
}

/// State plus named questions, sent to a provider in one pass.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DecisionRequest {
    /// Arbitrary JSON the questions are about. Filter it down before sending:
    /// irrelevant state degrades answers, and everything here leaves the
    /// machine when a cloud provider is configured.
    pub state: serde_json::Value,
    pub questions: BTreeMap<String, Question>,
}

impl DecisionRequest {
    /// Reject requests no provider could answer sensibly: an empty question
    /// map, empty instructions, a choice with fewer than 2 or more than 255
    /// options (or blank/duplicate option ids), a score with fewer than 2 or
    /// more than 10 levels.
    pub fn validate(&self) -> Result<(), DecisionError> {
        if self.questions.is_empty() {
            return Err(DecisionError::InvalidRequest(
                "a decision request needs at least one question".to_string(),
            ));
        }
        for (id, question) in &self.questions {
            if id.trim().is_empty() {
                return Err(DecisionError::InvalidRequest(
                    "question ids must be non-empty".to_string(),
                ));
            }
            let instructions = match question {
                Question::Choice { instructions, .. }
                | Question::Score { instructions, .. }
                | Question::Noul { instructions } => instructions,
            };
            if instructions.trim().is_empty() {
                return Err(DecisionError::InvalidRequest(format!(
                    "question '{id}' has empty instructions"
                )));
            }
            match question {
                Question::Choice { options, .. } => {
                    if options.len() < 2 || options.len() > 255 {
                        return Err(DecisionError::InvalidRequest(format!(
                            "question '{id}' must offer 2..=255 options, got {}",
                            options.len()
                        )));
                    }
                    let mut seen = std::collections::BTreeSet::new();
                    for (option_id, _) in options {
                        if option_id.trim().is_empty() {
                            return Err(DecisionError::InvalidRequest(format!(
                                "question '{id}' has an empty option id"
                            )));
                        }
                        if !seen.insert(option_id.as_str()) {
                            return Err(DecisionError::InvalidRequest(format!(
                                "question '{id}' repeats option id '{option_id}'"
                            )));
                        }
                    }
                }
                Question::Score { levels, .. } => {
                    if levels.len() < 2 || levels.len() > 10 {
                        return Err(DecisionError::InvalidRequest(format!(
                            "question '{id}' must define 2..=10 score levels, got {}",
                            levels.len()
                        )));
                    }
                }
                Question::Noul { .. } => {}
            }
        }
        Ok(())
    }
}

/// The provider's answers, keyed by question id.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct DecisionResponse {
    pub answers: BTreeMap<String, Answer>,
    /// Input tokens the provider billed for this request (0 when unknown).
    pub input_tokens: u64,
}

/// Failures from a decision provider.
#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("invalid decision request: {0}")]
    InvalidRequest(String),
    #[error("decision provider transport error: {0}")]
    Transport(String),
    #[error("decision provider returned HTTP {code}: {body}")]
    Status { code: u16, body: String },
    #[error("could not decode decision provider response: {0}")]
    Decode(String),
    #[error("decision provider rate limited the request")]
    RateLimited,
    #[error("decision provider is disabled")]
    Disabled,
}

impl DecisionError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::InvalidRequest(detail) => {
                DisplayMessage::new("services.ai.decision.invalid_request", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Transport(detail) => {
                DisplayMessage::new("services.ai.decision.transport", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Status { code, body } => {
                DisplayMessage::new("services.ai.decision.status", self.to_string())
                    .with_arg("status", code.to_string())
                    .with_arg("detail", body)
            }
            Self::Decode(detail) => {
                DisplayMessage::new("services.ai.decision.decode", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::RateLimited => {
                DisplayMessage::new("services.ai.decision.rate_limited", self.to_string())
            }
            Self::Disabled => {
                DisplayMessage::new("services.ai.decision.disabled", self.to_string())
            }
        }
    }
}

/// The contract for anything that answers decision requests.
#[async_trait]
pub trait DecisionProvider: Send + Sync {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError>;

    /// A stable identifier for diagnostics and selection assertions.
    fn provider_id(&self) -> &str;
}

/// Shared handle to any decision provider.
pub type DynDecisionProvider = Arc<dyn DecisionProvider>;

/// Provider identifier for [`StubDecisionProvider`].
pub const STUB_DECISION_PROVIDER_ID: &str = "stub-decision";

/// A deterministic, dependency-free provider for tests: answers are scripted
/// per question id, and asking about an unscripted id is an error.
#[derive(Debug, Default)]
pub struct StubDecisionProvider {
    answers: BTreeMap<String, Answer>,
}

impl StubDecisionProvider {
    pub fn with_answers(answers: BTreeMap<String, Answer>) -> Self {
        Self { answers }
    }
}

#[async_trait]
impl DecisionProvider for StubDecisionProvider {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        request.validate()?;
        let mut answers = BTreeMap::new();
        for id in request.questions.keys() {
            let answer = self.answers.get(id).ok_or_else(|| {
                DecisionError::InvalidRequest(format!("no scripted answer for question '{id}'"))
            })?;
            answers.insert(id.clone(), answer.clone());
        }
        Ok(DecisionResponse {
            answers,
            input_tokens: 0,
        })
    }

    fn provider_id(&self) -> &str {
        STUB_DECISION_PROVIDER_ID
    }
}

/// Provider identifier for [`LlmDecisionProvider`].
pub const LLM_FALLBACK_PROVIDER_ID: &str = "llm-fallback";

/// Adapts any chat model to the decision seam so features work without a
/// decision vendor configured.
///
/// The questions are rendered into one prompt that demands a single JSON
/// object, and every value is validated against its question. Chat models
/// return no calibrated probabilities, so fallback answers are uncalibrated:
/// `confidence` is fixed at 0.5, and a choice's probability mass sits entirely
/// on the pick.
pub struct LlmDecisionProvider {
    llm: DynLlmProvider,
}

impl LlmDecisionProvider {
    pub fn new(llm: DynLlmProvider) -> Self {
        Self { llm }
    }

    fn render_prompt(request: &DecisionRequest) -> String {
        let mut out = String::new();
        out.push_str(
            "Answer the questions below about the STATE. Reply with exactly one JSON object and \
             nothing else. The object must have one key per question id. The value shapes are: \
             choice questions {\"choice\": \"<option id>\"}, score questions {\"score\": \
             <level number>}, yes/no questions {\"noul\": <probability between 0 and 1>}.\n\n",
        );
        out.push_str("STATE:\n");
        out.push_str(&request.state.to_string());
        out.push_str("\n\nQUESTIONS:\n");
        for (id, question) in &request.questions {
            match question {
                Question::Choice {
                    instructions,
                    options,
                } => {
                    out.push_str(&format!(
                        "- \"{id}\" (choice): {instructions}\n  Options:\n"
                    ));
                    for (option_id, description) in options {
                        match description {
                            Some(text) => {
                                out.push_str(&format!("    - \"{option_id}\": {text}\n"));
                            }
                            None => out.push_str(&format!("    - \"{option_id}\"\n")),
                        }
                    }
                }
                Question::Score {
                    instructions,
                    levels,
                } => {
                    out.push_str(&format!(
                        "- \"{id}\" (score, levels 0..={}): {instructions}\n  Levels:\n",
                        levels.len() - 1
                    ));
                    for (index, level) in levels.iter().enumerate() {
                        out.push_str(&format!("    - {index}: {level}\n"));
                    }
                }
                Question::Noul { instructions } => {
                    out.push_str(&format!("- \"{id}\" (yes/no): {instructions}\n"));
                }
            }
        }
        out
    }

    fn parse_answers(
        request: &DecisionRequest,
        text: &str,
    ) -> Result<BTreeMap<String, Answer>, DecisionError> {
        let json = extract_json_object(text).ok_or_else(|| {
            DecisionError::Decode("LLM fallback reply contains no JSON object".to_string())
        })?;
        let parsed: serde_json::Value = serde_json::from_str(json)
            .map_err(|err| DecisionError::Decode(format!("LLM fallback reply: {err}")))?;
        let map = parsed.as_object().ok_or_else(|| {
            DecisionError::Decode("LLM fallback reply is not a JSON object".to_string())
        })?;
        let mut answers = BTreeMap::new();
        for (id, question) in &request.questions {
            let value = map.get(id).ok_or_else(|| {
                DecisionError::Decode(format!("LLM fallback reply is missing question '{id}'"))
            })?;
            let answer = match question {
                Question::Choice { options, .. } => {
                    let choice = value
                        .get("choice")
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| {
                            DecisionError::Decode(format!(
                                "question '{id}' answer has no string 'choice'"
                            ))
                        })?;
                    if !options.iter().any(|(option_id, _)| option_id == choice) {
                        return Err(DecisionError::Decode(format!(
                            "question '{id}' answered with unknown option '{choice}'"
                        )));
                    }
                    let mut probabilities = BTreeMap::new();
                    probabilities.insert(choice.to_string(), 1.0);
                    Answer::Choice {
                        choice: choice.to_string(),
                        probabilities,
                        confidence: 0.5,
                    }
                }
                Question::Score { levels, .. } => {
                    let score = value.get("score").and_then(|v| v.as_f64()).ok_or_else(|| {
                        DecisionError::Decode(format!(
                            "question '{id}' answer has no numeric 'score'"
                        ))
                    })?;
                    let max = (levels.len() - 1) as f64;
                    if !(0.0..=max).contains(&score) {
                        return Err(DecisionError::Decode(format!(
                            "question '{id}' score {score} is outside 0..={max}"
                        )));
                    }
                    Answer::Score {
                        score: score as f32,
                        confidence: 0.5,
                    }
                }
                Question::Noul { .. } => {
                    let probability =
                        value.get("noul").and_then(|v| v.as_f64()).ok_or_else(|| {
                            DecisionError::Decode(format!(
                                "question '{id}' answer has no numeric 'noul'"
                            ))
                        })?;
                    if !(0.0..=1.0).contains(&probability) {
                        return Err(DecisionError::Decode(format!(
                            "question '{id}' noul {probability} is outside 0..=1"
                        )));
                    }
                    Answer::Noul {
                        probability: probability as f32,
                    }
                }
            };
            answers.insert(id.clone(), answer);
        }
        Ok(answers)
    }
}

/// The first balanced `{…}` object in `text`, tolerating prose or code fences
/// around it. Brace matching ignores braces inside JSON strings.
fn extract_json_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, &byte) in bytes[start..].iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&text[start..=start + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

#[async_trait]
impl DecisionProvider for LlmDecisionProvider {
    async fn decide(&self, request: DecisionRequest) -> Result<DecisionResponse, DecisionError> {
        request.validate()?;
        let prompt = Self::render_prompt(&request);
        let completion = self
            .llm
            .complete(CompletionRequest {
                messages: vec![ChatMessage {
                    role: ChatRole::User,
                    content: prompt,
                }],
                model: None,
                capability: None,
                max_tokens: None,
                temperature: Some(0.0),
                metadata: serde_json::Value::Null,
            })
            .await
            .map_err(|err| DecisionError::Transport(err.to_string()))?;
        let answers = Self::parse_answers(&request, &completion.text)?;
        Ok(DecisionResponse {
            answers,
            input_tokens: 0,
        })
    }

    fn provider_id(&self) -> &str {
        LLM_FALLBACK_PROVIDER_ID
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::llm::{CompletionResponse, LlmError, LlmProvider};

    fn choice_question(option_count: usize) -> Question {
        Question::Choice {
            instructions: "Pick one".to_string(),
            options: (0..option_count)
                .map(|i| (format!("opt{i}"), None))
                .collect(),
        }
    }

    fn request_with(questions: Vec<(&str, Question)>) -> DecisionRequest {
        DecisionRequest {
            state: serde_json::json!({"table": "dbo.users"}),
            questions: questions
                .into_iter()
                .map(|(id, q)| (id.to_string(), q))
                .collect(),
        }
    }

    #[test]
    fn validate_rejects_256_options() {
        let request = request_with(vec![("pick", choice_question(256))]);
        assert!(matches!(
            request.validate(),
            Err(DecisionError::InvalidRequest(_))
        ));
        let ok = request_with(vec![("pick", choice_question(255))]);
        assert!(ok.validate().is_ok());
    }

    #[test]
    fn validate_rejects_one_level_score() {
        let request = request_with(vec![(
            "rate",
            Question::Score {
                instructions: "Rate it".to_string(),
                levels: vec!["only".to_string()],
            },
        )]);
        assert!(matches!(
            request.validate(),
            Err(DecisionError::InvalidRequest(_))
        ));
    }

    #[test]
    fn validate_rejects_empty_questions_and_instructions() {
        let empty = DecisionRequest {
            state: serde_json::Value::Null,
            questions: BTreeMap::new(),
        };
        assert!(matches!(
            empty.validate(),
            Err(DecisionError::InvalidRequest(_))
        ));
        let blank = request_with(vec![(
            "q",
            Question::Noul {
                instructions: "  ".to_string(),
            },
        )]);
        assert!(matches!(
            blank.validate(),
            Err(DecisionError::InvalidRequest(_))
        ));
    }

    #[tokio::test]
    async fn stub_returns_scripted_answers() {
        let mut scripted = BTreeMap::new();
        scripted.insert("urgent".to_string(), Answer::Noul { probability: 0.9 });
        let stub = StubDecisionProvider::with_answers(scripted);
        assert_eq!(stub.provider_id(), STUB_DECISION_PROVIDER_ID);

        let request = request_with(vec![(
            "urgent",
            Question::Noul {
                instructions: "Is this urgent?".to_string(),
            },
        )]);
        let response = stub.decide(request).await.unwrap();
        assert!(response.answers["urgent"].noul_at_least(0.9));
        assert!(!response.answers["urgent"].noul_at_least(0.95));

        let unknown = request_with(vec![(
            "other",
            Question::Noul {
                instructions: "Unscripted".to_string(),
            },
        )]);
        assert!(matches!(
            stub.decide(unknown).await,
            Err(DecisionError::InvalidRequest(_))
        ));
    }

    #[test]
    fn confident_choice_honors_threshold() {
        let answer = Answer::Choice {
            choice: "opt1".to_string(),
            probabilities: BTreeMap::from([("opt1".to_string(), 0.8)]),
            confidence: 0.7,
        };
        assert_eq!(answer.confident_choice(0.7), Some("opt1"));
        assert_eq!(answer.confident_choice(0.71), None);
        assert_eq!(
            Answer::Noul { probability: 1.0 }.confident_choice(0.0),
            None
        );
    }

    /// A chat provider that always replies with the same text.
    struct CannedLlm {
        reply: String,
    }

    #[async_trait]
    impl LlmProvider for CannedLlm {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse::new(&self.reply))
        }
    }

    fn fallback_request() -> DecisionRequest {
        request_with(vec![
            ("pick", choice_question(2)),
            (
                "rate",
                Question::Score {
                    instructions: "Rate it".to_string(),
                    levels: vec!["low".to_string(), "mid".to_string(), "high".to_string()],
                },
            ),
            (
                "urgent",
                Question::Noul {
                    instructions: "Is it urgent?".to_string(),
                },
            ),
        ])
    }

    #[tokio::test]
    async fn llm_fallback_parses_valid_json() {
        let llm = Arc::new(CannedLlm {
            reply: r#"Here you go: {"pick":{"choice":"opt1"},"rate":{"score":1.5},"urgent":{"noul":0.92}}"#.to_string(),
        });
        let provider = LlmDecisionProvider::new(llm);
        assert_eq!(provider.provider_id(), LLM_FALLBACK_PROVIDER_ID);
        let response = provider.decide(fallback_request()).await.unwrap();
        assert_eq!(response.answers["pick"].confident_choice(0.5), Some("opt1"));
        match &response.answers["rate"] {
            Answer::Score { score, confidence } => {
                assert!((score - 1.5).abs() < f32::EPSILON);
                assert!((confidence - 0.5).abs() < f32::EPSILON);
            }
            other => panic!("expected score answer, got {other:?}"),
        }
        assert!(response.answers["urgent"].noul_at_least(0.9));
    }

    #[tokio::test]
    async fn llm_fallback_rejects_unknown_option() {
        let llm = Arc::new(CannedLlm {
            reply: r#"{"pick":{"choice":"nope"},"rate":{"score":1},"urgent":{"noul":0.5}}"#
                .to_string(),
        });
        let provider = LlmDecisionProvider::new(llm);
        assert!(matches!(
            provider.decide(fallback_request()).await,
            Err(DecisionError::Decode(_))
        ));
    }
}
