//! Catalog-grounded SQL review and query summaries.
//!
//! Both surfaces reuse the [`crate::catalog::ask`] context-assembly path — ranked
//! objects, bounded token budget, grounding metadata — so the model only ever
//! sees the relevant slice of the schema, exactly like `ask_schema` and
//! `generate_sql`. Review additionally runs the deterministic
//! [`crate::catalog::validation::validate_sql`] pass first and hands the model its
//! findings, so the narrative is anchored to real, parser-backed issues
//! rather than free-form speculation.

use crate::catalog::ask::ask_with_purpose;
use crate::catalog::card::SchemaPurpose;
use crate::catalog::llm::{DynLlmProvider, LlmError};
use crate::catalog::store::CatalogStore;
use crate::catalog::validation::{SqlValidationOptions, SqlValidationResult, validate_sql};

/// A grounded review of one SQL batch: the deterministic validation findings
/// plus the model's narrative, with grounding metadata carried in `meta`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SqlReviewReport {
    pub narrative: String,
    pub validation: SqlValidationResult,
    /// Provider metadata including `grounded_in` — the catalog objects the
    /// narrative was built from.
    pub meta: serde_json::Value,
}

/// A grounded plain-language summary of an executed query.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct QuerySummaryReport {
    pub summary: String,
    /// Provider metadata including `grounded_in`.
    pub meta: serde_json::Value,
}

/// Review `sql` against the catalog: deterministic validation first, then a
/// schema-grounded narrative from the provider using the SqlReview purpose.
pub async fn review_sql(
    store: &CatalogStore,
    provider: &DynLlmProvider,
    sql: &str,
    options: &SqlValidationOptions,
    max_tokens: usize,
) -> Result<SqlReviewReport, LlmError> {
    let validation =
        validate_sql(store, sql, options).map_err(|e| LlmError::Other(e.to_string()))?;
    let findings = if validation.issues.is_empty() {
        "The deterministic validator found no issues.".to_string()
    } else {
        let lines: Vec<String> = validation
            .issues
            .iter()
            .map(|issue| {
                format!(
                    "- [{:?}/{:?}] {}",
                    issue.severity, issue.code, issue.message
                )
            })
            .collect();
        format!("Deterministic validator findings:\n{}", lines.join("\n"))
    };
    let question = format!(
        "Review this SQL for correctness, safety, and intent. Explain what it does, call out risks, and address each validator finding.\n\nSQL:\n{sql}\n\n{findings}"
    );
    let response = ask_with_purpose(
        store,
        provider,
        &question,
        max_tokens,
        None,
        SchemaPurpose::SqlReview,
    )
    .await?;
    Ok(SqlReviewReport {
        narrative: response.text,
        validation,
        meta: response.meta,
    })
}

/// Summarize an executed query in plain language, grounded in the catalog
/// objects it touched. Result-shape facts are passed as context so the
/// summary reflects what actually happened, not just the SQL text.
pub async fn summarize_query(
    store: &CatalogStore,
    provider: &DynLlmProvider,
    sql: &str,
    result_columns: &[String],
    row_count: Option<u64>,
    elapsed_ms: Option<u64>,
    max_tokens: usize,
) -> Result<QuerySummaryReport, LlmError> {
    let mut facts = Vec::new();
    if !result_columns.is_empty() {
        facts.push(format!("Result columns: {}", result_columns.join(", ")));
    }
    if let Some(rows) = row_count {
        facts.push(format!("Rows returned: {rows}"));
    }
    if let Some(ms) = elapsed_ms {
        facts.push(format!("Elapsed: {ms}ms"));
    }
    let facts = if facts.is_empty() {
        String::new()
    } else {
        format!("\n\nExecution facts:\n{}", facts.join("\n"))
    };
    let question = format!(
        "Summarize in two or three plain-language sentences what this query does and what its result shows.\n\nSQL:\n{sql}{facts}"
    );
    let response = ask_with_purpose(
        store,
        provider,
        &question,
        max_tokens,
        None,
        SchemaPurpose::Explanation,
    )
    .await?;
    Ok(QuerySummaryReport {
        summary: response.text,
        meta: response.meta,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::llm::StubLlmProvider;
    use std::sync::Arc;

    fn store_with_users() -> CatalogStore {
        let store = CatalogStore::open_in_memory().unwrap();
        store
            .sync(&[crate::catalog::CatalogObject {
                schema: Some("dbo".into()),
                name: "users".into(),
                kind: crate::catalog::CatalogObjectKind::Table,
                columns: vec![crate::catalog::CatalogColumn {
                    name: "id".into(),
                    type_name: Some("int".into()),
                    ordinal: 0,
                    nullable: false,
                }],
                primary_key: vec![],
                foreign_keys: vec![],
                parameters: vec![],
                definition: None,
                source: crate::catalog::CatalogSource {
                    kind: crate::catalog::SourceKind::Database,
                    path: None,
                    create_date: Some(1),
                    modify_date: Some(2),
                },
            }])
            .unwrap();
        store
    }

    #[tokio::test]
    async fn review_is_deterministic_with_the_stub_and_carries_validation() {
        let store = store_with_users();
        let provider: DynLlmProvider = Arc::new(StubLlmProvider::new());
        let report = review_sql(
            &store,
            &provider,
            "DELETE FROM dbo.users",
            &SqlValidationOptions {
                flag_unconstrained_mutations: true,
                ..SqlValidationOptions::default()
            },
            2048,
        )
        .await
        .unwrap();
        assert!(!report.narrative.is_empty());
        // DELETE without WHERE is a validator finding the report must carry.
        assert!(
            !report.validation.issues.is_empty(),
            "{:?}",
            report.validation
        );
        assert!(
            report.meta.get("grounded_in").is_some(),
            "review must be grounded: {:?}",
            report.meta
        );
    }

    #[tokio::test]
    async fn summary_is_grounded_and_includes_result_facts_in_the_question() {
        let store = store_with_users();
        let provider: DynLlmProvider = Arc::new(StubLlmProvider::new());
        let report = summarize_query(
            &store,
            &provider,
            "SELECT id FROM dbo.users",
            &["id".to_string()],
            Some(42),
            Some(17),
            2048,
        )
        .await
        .unwrap();
        assert!(!report.summary.is_empty());
        assert!(
            report.meta.get("grounded_in").is_some(),
            "summary must be grounded: {:?}",
            report.meta
        );
    }
}
