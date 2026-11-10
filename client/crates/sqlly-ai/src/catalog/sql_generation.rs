//! Catalog-grounded SQL generation.
//!
//! [`generate_sql`] turns a natural-language request into a validated SQL
//! statement. It reuses the existing purpose-driven context assembly
//! ([`crate::catalog::ask::ask_with_purpose`] with [`SchemaPurpose::SqlGeneration`]) so
//! the model only ever sees a bounded, relevant slice of the catalog, then
//! extracts the SQL from the model response and runs it through
//! [`validate_sql`].
//!
//! ## Bounded validation-retry loop
//!
//! When the first attempt produces SQL whose blocking errors are *all*
//! retryable (an unresolved identifier, multiple statements, or empty output —
//! see [`SqlValidationResult::has_only_retryable_errors`]), `generate_sql`
//! composes a deterministic correction prompt (the original request + the prior
//! SQL + the validation findings) and asks the same provider again, up to
//! [`SqlGenerationOptions::max_attempts`] total attempts (default
//! [`DEFAULT_SQL_GENERATION_ATTEMPTS`], hard-clamped to
//! [`MAX_SQL_GENERATION_ATTEMPTS`]). The loop stops as soon as the SQL validates,
//! requires human confirmation, has only non-retryable findings, or the attempt
//! budget is exhausted.
//!
//! Destructive-intent errors (UPDATE/DELETE without WHERE) and
//! confirmation-gated operations (DROP/ALTER/TRUNCATE/MERGE) are **never**
//! retried — auto-"fixing" them would invent intent the user never expressed.
//! Warnings (tenant scope, cross-database) never block and never trigger a retry.
//!
//! The final attempt populates the top-level result fields for backward
//! compatibility; every attempt (including the final one) is also recorded in
//! [`SqlGenerationResult::attempts`].

use crate::catalog::ask::{DEFAULT_CONTEXT_TOKENS, ask_with_purpose_focused};
use crate::catalog::card::SchemaPurpose;
use crate::catalog::intent::{SqlIntent, detect_intent};
use crate::catalog::llm::{DynLlmProvider, LlmError};
use crate::catalog::prompt::GroundedObject;
use crate::catalog::store::CatalogStore;
use crate::catalog::validation::{
    SqlIssueSeverity, SqlValidationOptions, SqlValidationResult, validate_sql,
};

use sqlly_lang::sqlmodel::Dialect;

/// Default number of total generation attempts (one generation + up to one
/// correction) when the caller does not specify [`SqlGenerationOptions::max_attempts`].
pub const DEFAULT_SQL_GENERATION_ATTEMPTS: usize = 2;

/// Hard upper bound on total generation attempts. Caller-supplied values are
/// clamped to this so a runaway request cannot fan out into unbounded
/// LLM calls.
pub const MAX_SQL_GENERATION_ATTEMPTS: usize = 4;

/// Options that shape SQL generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqlGenerationOptions {
    pub dialect: Dialect,
    pub max_tokens: Option<usize>,
    /// Extra caller-supplied system text (e.g. connection/scope hints).
    pub system: Option<String>,
    /// Default database for cross-database validation of the generated SQL.
    pub default_database: Option<String>,
    /// Maximum total generation attempts (the first generation plus correction
    /// retries). `None` uses [`DEFAULT_SQL_GENERATION_ATTEMPTS`]; any value is
    /// normalized to `1..=`[`MAX_SQL_GENERATION_ATTEMPTS`].
    pub max_attempts: Option<usize>,
    /// Whether the generated SQL's validation flags tenant-scoped tables
    /// without a tenant predicate (mirrors
    /// [`SqlValidationOptions::require_tenant_filter`]).
    pub require_tenant_filter: bool,
    /// When true with `require_tenant_filter`, a missing tenant predicate is an
    /// error instead of a warning.
    pub fail_closed_tenant_filter: bool,
    /// Custom tenant column folds for that check (mirrors
    /// [`SqlValidationOptions::tenant_column_folds`]).
    pub tenant_column_folds: Option<Vec<String>>,
    /// Catalog object keys to charge first against the context token budget.
    pub focus_object_keys: Vec<String>,
}

impl Default for SqlGenerationOptions {
    fn default() -> Self {
        Self {
            dialect: Dialect::SqlServer,
            max_tokens: None,
            system: None,
            default_database: None,
            max_attempts: None,
            require_tenant_filter: false,
            fail_closed_tenant_filter: false,
            tenant_column_folds: None,
            focus_object_keys: Vec::new(),
        }
    }
}

impl SqlGenerationOptions {
    /// Resolve [`Self::max_attempts`] into a concrete attempt count, applying
    /// the default and clamping to `1..=`[`MAX_SQL_GENERATION_ATTEMPTS`].
    fn resolved_max_attempts(&self) -> usize {
        self.max_attempts
            .unwrap_or(DEFAULT_SQL_GENERATION_ATTEMPTS)
            .clamp(1, MAX_SQL_GENERATION_ATTEMPTS)
    }
}

/// How a single generation attempt concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptCompletion {
    /// Validated with no blocking errors and no pending confirmation.
    Valid,
    /// Had blocking errors that were all retryable, and another attempt followed.
    Invalid,
    /// Carried a destructive operation awaiting human confirmation; not retried.
    RequiresConfirmation,
    /// Had at least one non-retryable blocking error (e.g. a destructive
    /// `DELETE`/`UPDATE` without `WHERE`), so no automatic correction was
    /// attempted — even if a retryable error was also present. The SQL is
    /// invalid and left for a human to resolve.
    Stopped,
    /// Had only retryable blocking errors but the attempt budget was exhausted.
    Exhausted,
}

/// One generation attempt: the SQL produced, its detected intent, the
/// validation report, and how the attempt concluded.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SqlGenerationAttempt {
    /// 1-based attempt index.
    pub attempt: usize,
    pub sql: String,
    pub intent: SqlIntent,
    pub validation: SqlValidationResult,
    pub completion: AttemptCompletion,
}

/// The outcome of a generation request.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SqlGenerationResult {
    /// The extracted SQL statement (empty if the model produced none).
    pub sql: String,
    /// Best-known intent: parsed from the SQL when possible, else from the
    /// request.
    pub intent: SqlIntent,
    /// Validation report for the generated SQL.
    pub validation: SqlValidationResult,
    /// Catalog objects the answer was grounded in.
    pub grounded_in: Vec<GroundedObject>,
    /// Free-form provider metadata from the underlying response.
    pub provider_meta: serde_json::Value,
    /// Per-attempt history, oldest first. The last entry corresponds to the
    /// top-level `sql`/`intent`/`validation`. Additive and `#[serde(default)]`
    /// so existing consumers that ignore it keep working.
    #[serde(default)]
    pub attempts: Vec<SqlGenerationAttempt>,
}

/// The instruction appended to the system prompt so the model returns one clean
/// SQL statement we can extract and validate.
const SQL_OUTPUT_INSTRUCTION: &str = "\
Return exactly one SQL statement that answers the request.
Output the statement inside a single ```sql fenced code block and nothing else.
Do not include explanations, comments, or multiple statements.";

/// Generate a validated SQL statement for a natural-language `question`.
///
/// Runs a bounded validation-retry loop: see the module docs. The returned
/// top-level fields reflect the final attempt; [`SqlGenerationResult::attempts`]
/// carries the full per-attempt history.
pub async fn generate_sql(
    store: &CatalogStore,
    provider: &DynLlmProvider,
    question: &str,
    options: &SqlGenerationOptions,
) -> Result<SqlGenerationResult, LlmError> {
    // Intent detected from the original request is the fallback used for every
    // attempt whose SQL cannot be classified — never re-derived from a
    // correction prompt (which contains prior SQL and would bias detection).
    let requested_intent = detect_intent(question);
    let max_attempts = options.resolved_max_attempts();

    // The prompt question evolves across attempts: the first is the user's
    // request; each retry is a deterministic correction prompt.
    let mut current_question = question.to_string();

    let mut attempts: Vec<SqlGenerationAttempt> = Vec::new();
    let mut last_grounded_in: Vec<GroundedObject> = Vec::new();
    let mut last_provider_meta = serde_json::Value::Null;

    for attempt_no in 1..=max_attempts {
        let pass = generate_sql_once(store, provider, &current_question, options).await?;

        last_grounded_in = pass.grounded_in;
        last_provider_meta = pass.provider_meta;

        let intent = if pass.validation.detected_intent == SqlIntent::Unknown {
            requested_intent
        } else {
            pass.validation.detected_intent
        };

        let has_blocking_error = pass
            .validation
            .issues
            .iter()
            .any(|issue| issue.severity == SqlIssueSeverity::Error);
        let completion = if pass.validation.requires_confirmation {
            AttemptCompletion::RequiresConfirmation
        } else if !has_blocking_error {
            AttemptCompletion::Valid
        } else if !pass.validation.has_only_retryable_errors() {
            // At least one blocking error is non-retryable: do not invent a fix.
            AttemptCompletion::Stopped
        } else if attempt_no >= max_attempts {
            AttemptCompletion::Exhausted
        } else {
            AttemptCompletion::Invalid
        };

        // Compose the next correction prompt before moving the validation into
        // the attempt record (only needed when we will actually retry).
        let next_question = if completion == AttemptCompletion::Invalid {
            Some(compose_correction_question(
                question,
                &pass.sql,
                &pass.validation,
                attempt_no,
            ))
        } else {
            None
        };

        attempts.push(SqlGenerationAttempt {
            attempt: attempt_no,
            sql: pass.sql,
            intent,
            validation: pass.validation,
            completion,
        });

        match next_question {
            Some(q) => current_question = q,
            None => break,
        }
    }

    // `max_attempts` is clamped to at least 1, so the loop always pushes once.
    let final_attempt = attempts
        .last()
        .expect("retry loop runs at least once")
        .clone();

    Ok(SqlGenerationResult {
        sql: final_attempt.sql,
        intent: final_attempt.intent,
        validation: final_attempt.validation,
        grounded_in: last_grounded_in,
        provider_meta: last_provider_meta,
        attempts,
    })
}

/// The product of a single generation pass: extracted SQL, its validation
/// report, the grounding set, and the provider's free-form metadata.
struct GenerationPass {
    sql: String,
    validation: SqlValidationResult,
    grounded_in: Vec<GroundedObject>,
    provider_meta: serde_json::Value,
}

/// Run exactly one generation pass: ask the provider with the SQL-generation
/// purpose, extract the SQL, and validate it against the catalog.
async fn generate_sql_once(
    store: &CatalogStore,
    provider: &DynLlmProvider,
    question: &str,
    options: &SqlGenerationOptions,
) -> Result<GenerationPass, LlmError> {
    let system = compose_system(options.system.as_deref());
    let max_tokens = options.max_tokens.unwrap_or(DEFAULT_CONTEXT_TOKENS);

    let response = ask_with_purpose_focused(
        store,
        provider,
        question,
        &options.focus_object_keys,
        max_tokens,
        Some(system),
        SchemaPurpose::SqlGeneration,
    )
    .await?;

    let sql = extract_sql(&response.text);

    let validation = validate_sql(
        store,
        &sql,
        &SqlValidationOptions {
            dialect: options.dialect,
            engine: None,
            default_database: options.default_database.clone(),
            default_schema: None,
            allow_multiple_statements: false,
            require_tenant_filter: options.require_tenant_filter,
            fail_closed_tenant_filter: options.fail_closed_tenant_filter,
            tenant_column_folds: options.tenant_column_folds.clone(),
            // Model-written mutations always stay behind the human
            // confirmation gate, whatever the editor's warning default is.
            flag_unconstrained_mutations: true,
        },
    )
    .map_err(|e| LlmError::Other(e.to_string()))?;

    let grounded_in = extract_grounding(&response.meta);

    Ok(GenerationPass {
        sql,
        validation,
        grounded_in,
        provider_meta: response.meta,
    })
}

/// Render the validation findings of a failed attempt into a deterministic,
/// model-readable feedback block. Issues are ordered Error → Warning →
/// RequireConfirmation → Info, then by code, then by message, so the same
/// validation result always renders identical text (snapshot-friendly).
fn render_validation_feedback(validation: &SqlValidationResult) -> String {
    let mut issues: Vec<&crate::catalog::validation::SqlValidationIssue> =
        validation.issues.iter().collect();
    issues.sort_by(|a, b| {
        severity_rank(a.severity)
            .cmp(&severity_rank(b.severity))
            .then_with(|| format!("{:?}", a.code).cmp(&format!("{:?}", b.code)))
            .then_with(|| a.message.cmp(&b.message))
    });

    let mut lines = Vec::with_capacity(issues.len());
    for issue in issues {
        let severity = match issue.severity {
            SqlIssueSeverity::Info => "info",
            SqlIssueSeverity::Warning => "warning",
            SqlIssueSeverity::Error => "error",
            SqlIssueSeverity::RequireConfirmation => "confirm",
        };
        match &issue.identifier {
            Some(id) => lines.push(format!("- [{severity}] {} ({})", issue.message, id)),
            None => lines.push(format!("- [{severity}] {}", issue.message)),
        }
    }
    lines.join("\n")
}

/// Stable ordering rank for issue severities (lower sorts first).
fn severity_rank(severity: SqlIssueSeverity) -> u8 {
    match severity {
        SqlIssueSeverity::Error => 0,
        SqlIssueSeverity::Warning => 1,
        SqlIssueSeverity::RequireConfirmation => 2,
        SqlIssueSeverity::Info => 3,
    }
}

/// Build the correction prompt for the next attempt. Deterministic: the same
/// inputs always produce the same text.
fn compose_correction_question(
    original_question: &str,
    prior_sql: &str,
    validation: &SqlValidationResult,
    attempt_no: usize,
) -> String {
    let feedback = render_validation_feedback(validation);
    format!(
        "Original request:\n{original_question}\n\n\
Previous SQL attempt {attempt_no} failed catalog validation:\n\
```sql\n{prior_sql}\n```\n\n\
Validation findings to fix:\n{feedback}\n\n\
Return one corrected SQL statement that resolves these findings while \
preserving the original request's intent. Do not introduce unrelated changes."
    )
}

/// Layer the SQL-output instruction on top of any caller-supplied system text.
fn compose_system(caller: Option<&str>) -> String {
    match caller {
        Some(extra) if !extra.trim().is_empty() => {
            format!("{}\n\n{SQL_OUTPUT_INSTRUCTION}", extra.trim())
        }
        _ => SQL_OUTPUT_INSTRUCTION.to_string(),
    }
}

/// Extract a SQL statement from a model response.
///
/// Preference order: a ```sql-tagged fenced block, then any fenced block, then
/// the whole trimmed response. Always returns the best available text; an empty
/// string means the model produced nothing usable.
pub fn extract_sql(text: &str) -> String {
    if let Some(block) = fenced_block(text, Some("sql")) {
        return block;
    }
    if let Some(block) = fenced_block(text, None) {
        return block;
    }
    text.trim().to_string()
}

/// Find the first fenced code block. When `lang` is `Some`, only a block whose
/// info string starts with that language is matched.
fn fenced_block(text: &str, lang: Option<&str>) -> Option<String> {
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim_start();
        let Some(rest) = trimmed.strip_prefix("```") else {
            continue;
        };
        let info = rest.trim().to_ascii_lowercase();
        let matches = match lang {
            Some(want) => info.starts_with(want),
            None => true,
        };
        if !matches {
            continue;
        }
        let mut body = Vec::new();
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                let joined = body.join("\n").trim().to_string();
                if joined.is_empty() {
                    return None;
                }
                return Some(joined);
            }
            body.push(inner);
        }
        // Unterminated fence: take what we collected.
        let joined = body.join("\n").trim().to_string();
        return if joined.is_empty() {
            None
        } else {
            Some(joined)
        };
    }
    None
}

/// Pull the `grounded_in` array (written by [`crate::catalog::ask`]) out of response
/// metadata.
fn extract_grounding(meta: &serde_json::Value) -> Vec<GroundedObject> {
    meta.get("grounded_in")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::llm::{
        CompletionRequest, CompletionResponse, LlmPrompt, LlmProvider, LlmResponse,
    };
    use crate::catalog::wire::{
        CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, SourceKind,
    };
    use std::sync::Arc;

    fn table(name: &str, cols: &[&str]) -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind: CatalogObjectKind::Table,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, c)| CatalogColumn {
                    name: (*c).into(),
                    type_name: Some("INT".into()),
                    ordinal: i as i64 + 1,
                    nullable: false,
                })
                .collect(),
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: Some(1),
                modify_date: Some(2),
            },
        }
    }

    /// A provider that returns a fixed answer and records the system prompt.
    struct CannedProvider {
        answer: String,
        systems: std::sync::Mutex<Vec<String>>,
    }

    impl CannedProvider {
        fn new(answer: &str) -> Self {
            Self {
                answer: answer.to_string(),
                systems: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for CannedProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            Ok(CompletionResponse::new(&self.answer))
        }

        async fn ask(&self, prompt: LlmPrompt) -> Result<LlmResponse, LlmError> {
            self.systems
                .lock()
                .unwrap()
                .push(prompt.system.clone().unwrap_or_default());
            Ok(LlmResponse::new(&self.answer))
        }
    }

    /// A provider that returns a scripted sequence of answers, one per call, and
    /// records every prompt question it saw. Once the script is exhausted it
    /// keeps returning the last answer.
    struct ScriptedProvider {
        answers: Vec<String>,
        calls: std::sync::Mutex<usize>,
        questions: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn new(answers: &[&str]) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                calls: std::sync::Mutex::new(0),
                questions: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn call_count(&self) -> usize {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait::async_trait]
    impl LlmProvider for ScriptedProvider {
        async fn complete(
            &self,
            _request: CompletionRequest,
        ) -> Result<CompletionResponse, LlmError> {
            let mut calls = self.calls.lock().unwrap();
            let idx = (*calls).min(self.answers.len().saturating_sub(1));
            *calls += 1;
            Ok(CompletionResponse::new(&self.answers[idx]))
        }

        async fn ask(&self, prompt: LlmPrompt) -> Result<LlmResponse, LlmError> {
            self.questions.lock().unwrap().push(prompt.question.clone());
            let mut calls = self.calls.lock().unwrap();
            let idx = (*calls).min(self.answers.len().saturating_sub(1));
            *calls += 1;
            Ok(LlmResponse::new(&self.answers[idx]))
        }
    }

    fn store_with(objects: &[CatalogObject]) -> CatalogStore {
        let store = CatalogStore::open_in_memory().unwrap();
        store.sync(objects).unwrap();
        store
    }

    #[test]
    fn extract_sql_prefers_sql_fence() {
        let text = "Here you go:\n```sql\nSELECT 1\n```\nhope that helps";
        assert_eq!(extract_sql(text), "SELECT 1");
    }

    #[test]
    fn extract_sql_falls_back_to_generic_fence() {
        let text = "```\nSELECT 2\n```";
        assert_eq!(extract_sql(text), "SELECT 2");
    }

    #[test]
    fn extract_sql_falls_back_to_whole_text() {
        assert_eq!(extract_sql("  SELECT 3  "), "SELECT 3");
    }

    #[tokio::test]
    async fn generate_extracts_and_validates_sql() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let provider: DynLlmProvider = Arc::new(CannedProvider::new(
            "```sql\nSELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1\n```",
        ));
        let result = generate_sql(
            &store,
            &provider,
            "show me a customer",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            result.sql,
            "SELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1"
        );
        assert_eq!(result.intent, SqlIntent::Select);
        assert!(result.validation.is_valid, "{:?}", result.validation.issues);
    }

    #[tokio::test]
    async fn generate_reports_validation_problems() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let provider: DynLlmProvider = Arc::new(CannedProvider::new(
            "```sql\nDELETE FROM dbo.Customers\n```",
        ));
        let result = generate_sql(
            &store,
            &provider,
            "delete all customers",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(result.intent, SqlIntent::Delete);
        assert!(!result.validation.is_valid);
        assert!(
            result
                .validation
                .issues
                .iter()
                .any(|i| i.code == crate::catalog::validation::SqlIssueCode::DeleteWithoutWhere)
        );
    }

    #[tokio::test]
    async fn generate_passes_sql_output_instruction_to_provider() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let canned = Arc::new(CannedProvider::new(
            "```sql\nSELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1\n```",
        ));
        let provider: DynLlmProvider = canned.clone();
        generate_sql(
            &store,
            &provider,
            "show a customer",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();
        let systems = canned.systems.lock().unwrap();
        assert!(systems.iter().any(|s| s.contains("one SQL statement")));
    }

    #[tokio::test]
    async fn generate_preserves_grounding() {
        let store = store_with(&[table("Customers", &["CustomerId", "Email"])]);
        let provider: DynLlmProvider = Arc::new(CannedProvider::new(
            "```sql\nSELECT Email FROM dbo.Customers WHERE CustomerId = 1\n```",
        ));
        let result = generate_sql(
            &store,
            &provider,
            "where is the customer email",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();
        assert!(
            result
                .grounded_in
                .iter()
                .any(|g| g.object_key == "dbo.customers")
        );
    }

    #[tokio::test]
    async fn single_attempt_when_first_pass_is_valid() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nSELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "show a customer",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(scripted.call_count(), 1, "no retry for a valid first pass");
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(result.attempts[0].completion, AttemptCompletion::Valid);
        assert!(result.validation.is_valid, "{:?}", result.validation.issues);
        // Top-level fields mirror the final (only) attempt.
        assert_eq!(result.sql, result.attempts[0].sql);
    }

    #[tokio::test]
    async fn retries_after_unresolved_table_then_succeeds() {
        let store = store_with(&[table("Customers", &["CustomerId", "Name"])]);
        // First answer references a non-existent table (retryable error);
        // second answer is valid.
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nSELECT * FROM dbo.Ghosts\n```",
            "```sql\nSELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "show a customer",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(scripted.call_count(), 2, "should retry exactly once");
        assert_eq!(result.attempts.len(), 2);
        assert_eq!(result.attempts[0].completion, AttemptCompletion::Invalid);
        assert_eq!(result.attempts[1].completion, AttemptCompletion::Valid);
        assert!(result.validation.is_valid, "{:?}", result.validation.issues);
        assert_eq!(
            result.sql,
            "SELECT CustomerId FROM dbo.Customers WHERE CustomerId = 1"
        );
    }

    #[tokio::test]
    async fn stops_at_max_attempts_when_never_valid() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        // Always unresolved: every pass is a retryable error.
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nSELECT * FROM dbo.Ghosts\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "show a ghost",
            &SqlGenerationOptions {
                max_attempts: Some(3),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(scripted.call_count(), 3);
        assert_eq!(result.attempts.len(), 3);
        assert_eq!(
            result.attempts.last().unwrap().completion,
            AttemptCompletion::Exhausted
        );
        assert!(!result.validation.is_valid);
    }

    #[tokio::test]
    async fn max_attempts_is_clamped_to_hard_cap() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nSELECT * FROM dbo.Ghosts\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "show a ghost",
            &SqlGenerationOptions {
                max_attempts: Some(99),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert_eq!(scripted.call_count(), MAX_SQL_GENERATION_ATTEMPTS);
        assert_eq!(result.attempts.len(), MAX_SQL_GENERATION_ATTEMPTS);
    }

    #[tokio::test]
    async fn does_not_retry_destructive_confirmation() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nDROP TABLE dbo.Customers\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "drop the customers table",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(scripted.call_count(), 1, "confirmation must not retry");
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(
            result.attempts[0].completion,
            AttemptCompletion::RequiresConfirmation
        );
        assert!(result.validation.requires_confirmation);
    }

    #[tokio::test]
    async fn does_not_retry_delete_without_where() {
        // An unconstrained DELETE is non-retryable: one pass only, and the
        // attempt terminates as `RequiresConfirmation` (surfaced for the user to
        // confirm), never auto-corrected and never the misleading `Valid`.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nDELETE FROM dbo.Customers\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "delete all customers",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            scripted.call_count(),
            1,
            "delete-without-where must not retry"
        );
        assert_eq!(result.attempts.len(), 1);
        assert_eq!(
            result.attempts[0].completion,
            AttemptCompletion::RequiresConfirmation
        );
        assert!(!result.validation.is_valid);
    }

    #[tokio::test]
    async fn does_not_retry_mixed_retryable_and_nonretryable_errors() {
        // `DELETE FROM dbo.Ghosts` carries a retryable unresolved-table error
        // AND a non-retryable delete-without-where error. The non-retryable
        // error must veto the retry so the loop never asks the model to "fix"
        // the destructive statement by inventing a predicate.
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let scripted = Arc::new(ScriptedProvider::new(&[
            "```sql\nDELETE FROM dbo.Ghosts\n```",
            // A second (corrected) answer that should NEVER be requested.
            "```sql\nDELETE FROM dbo.Customers WHERE CustomerId = 1\n```",
        ]));
        let provider: DynLlmProvider = scripted.clone();
        let result = generate_sql(
            &store,
            &provider,
            "delete the ghosts",
            &SqlGenerationOptions::default(),
        )
        .await
        .unwrap();

        assert_eq!(
            scripted.call_count(),
            1,
            "a non-retryable error present alongside a retryable one must stop the loop"
        );
        assert_eq!(result.attempts.len(), 1);
        // The unconstrained DELETE makes this require confirmation rather than
        // retry; either way the loop never asks the model to repair it.
        assert_eq!(
            result.attempts[0].completion,
            AttemptCompletion::RequiresConfirmation
        );
        assert!(!result.validation.is_valid);
        // The final SQL is the model's first (unrepaired) answer, not a
        // predicate the loop invented.
        assert_eq!(result.sql, "DELETE FROM dbo.Ghosts");
    }

    #[test]
    fn correction_prompt_is_deterministic() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let validation = validate_sql(
            &store,
            "SELECT * FROM dbo.Ghosts",
            &SqlValidationOptions::default(),
        )
        .unwrap();
        let a =
            compose_correction_question("find ghosts", "SELECT * FROM dbo.Ghosts", &validation, 1);
        let b =
            compose_correction_question("find ghosts", "SELECT * FROM dbo.Ghosts", &validation, 1);
        assert_eq!(a, b);
    }

    #[test]
    fn correction_prompt_contains_prior_sql_and_findings() {
        let store = store_with(&[table("Customers", &["CustomerId"])]);
        let validation = validate_sql(
            &store,
            "SELECT * FROM dbo.Ghosts",
            &SqlValidationOptions::default(),
        )
        .unwrap();
        let prompt =
            compose_correction_question("find ghosts", "SELECT * FROM dbo.Ghosts", &validation, 1);
        assert!(prompt.contains("find ghosts"), "missing original request");
        assert!(
            prompt.contains("SELECT * FROM dbo.Ghosts"),
            "missing prior SQL"
        );
        assert!(prompt.contains("```sql"), "prior SQL should be fenced");
        assert!(
            prompt.contains("[error]"),
            "missing severity-tagged finding"
        );
        assert!(
            prompt.contains("dbo.Ghosts"),
            "missing offending identifier"
        );
    }
}
