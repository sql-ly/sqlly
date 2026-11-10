//! Pure prompt builders for handing query-history context to the AI Ask lane.

use crate::intellisense_feedback::redact_sql;

pub struct HistoryAnalysisInput<'a> {
    pub sql: &'a str,
    pub server: &'a str,
    pub database: Option<&'a str>,
    pub timestamp: &'a str,
    pub duration_ms: Option<u64>,
    pub outcome: &'a str,
    pub rows_affected: Option<u64>,
    pub error_text: Option<&'a str>,
    pub plan_digest: Option<&'a str>,
}

pub const MAX_SQL_CHARS: usize = 4_000;

pub fn build_history_analysis_prompt(input: &HistoryAnalysisInput<'_>) -> String {
    let redacted = redact_sql(input.sql);
    let (sql_body, truncated) = truncate_sql(&redacted);
    let mut lines = vec![
        "Analyze this query run and suggest improvements. Explain likely causes of slowness or failure and propose a rewrite or index if warranted.".to_string(),
        String::new(),
        format!("Server: {}", input.server),
    ];
    if let Some(database) = input.database.filter(|db| !db.is_empty()) {
        lines.push(format!("Database: {database}"));
    }
    lines.push(format!("Ran at: {}", input.timestamp));
    if let Some(ms) = input.duration_ms {
        lines.push(format!("Elapsed: {ms} ms"));
    }
    lines.push(format!("Outcome: {}", input.outcome));
    if let Some(rows) = input.rows_affected {
        lines.push(format!("Rows affected: {rows}"));
    }
    if let Some(error) = input.error_text.filter(|text| !text.is_empty()) {
        lines.push(String::new());
        lines.push(format!("Error: {error}"));
    }
    if let Some(digest) = input.plan_digest.filter(|text| !text.is_empty()) {
        lines.push(String::new());
        lines.push("Execution plan digest:".to_string());
        lines.push(digest.to_string());
    }
    lines.push(String::new());
    lines.push("```sql".to_string());
    lines.push(sql_body);
    if truncated {
        lines.push("(truncated)".to_string());
    }
    lines.push("```".to_string());
    lines.join("\n")
}

pub fn build_history_range_prompt(entries: &[HistoryAnalysisInput<'_>]) -> String {
    let total = entries.len();
    let shown = entries.len().min(10);
    let mut lines = vec![
        "Analyze these recent query runs together: spot repeated slow patterns, shared tables, and failures.".to_string(),
        String::new(),
    ];
    for (index, entry) in entries.iter().take(10).enumerate() {
        lines.push(format!("## Run {}", index + 1));
        lines.push(build_history_analysis_prompt(entry));
        lines.push(String::new());
    }
    if total > shown {
        lines.push(format!("({total} runs total; showing first {shown})"));
    }
    lines.join("\n")
}

fn truncate_sql(sql: &str) -> (String, bool) {
    let char_count = sql.chars().count();
    if char_count <= MAX_SQL_CHARS {
        return (sql.to_string(), false);
    }
    let truncated: String = sql.chars().take(MAX_SQL_CHARS).collect();
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_input(sql: &str) -> HistoryAnalysisInput<'_> {
        HistoryAnalysisInput {
            sql,
            server: "localhost",
            database: Some("app"),
            timestamp: "2026-08-30T12:00:00Z",
            duration_ms: Some(42),
            outcome: "failed",
            rows_affected: Some(7),
            error_text: Some("timeout"),
            plan_digest: Some("Seq Scan on users"),
        }
    }

    #[test]
    fn prompt_redacts_literals_and_bounds_sql() {
        let prompt =
            build_history_analysis_prompt(&sample_input("SELECT * FROM t WHERE x = 'secret'"));
        assert!(!prompt.contains("'secret'"));
        assert!(prompt.contains("redacted"));
        let long_sql = "SELECT ".to_string() + &"x".repeat(10_000);
        let long_prompt = build_history_analysis_prompt(&HistoryAnalysisInput {
            sql: &long_sql,
            server: "srv",
            database: None,
            timestamp: "t",
            duration_ms: None,
            outcome: "unknown",
            rows_affected: None,
            error_text: None,
            plan_digest: None,
        });
        assert!(long_prompt.contains("(truncated)"));
    }

    #[test]
    fn prompt_omits_missing_metadata() {
        let prompt = build_history_analysis_prompt(&HistoryAnalysisInput {
            sql: "SELECT 1",
            server: "srv",
            database: None,
            timestamp: "t",
            duration_ms: None,
            outcome: "succeeded",
            rows_affected: None,
            error_text: None,
            plan_digest: None,
        });
        assert!(!prompt.contains("Database:"));
        assert!(!prompt.contains("Elapsed:"));
        assert!(!prompt.contains("Rows affected:"));
    }

    #[test]
    fn range_prompt_caps_at_ten_entries() {
        let sqls: Vec<String> = (0..12).map(|i| format!("SELECT {i}")).collect();
        let entries: Vec<_> = sqls
            .iter()
            .map(|sql| HistoryAnalysisInput {
                sql,
                server: "srv",
                database: None,
                timestamp: "t",
                duration_ms: None,
                outcome: "unknown",
                rows_affected: None,
                error_text: None,
                plan_digest: None,
            })
            .collect();
        let prompt = build_history_range_prompt(&entries);
        assert_eq!(prompt.matches("## Run ").count(), 10);
        assert!(prompt.contains("(12 runs total; showing first 10)"));
    }
}
