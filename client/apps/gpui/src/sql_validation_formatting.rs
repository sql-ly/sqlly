//! SQL validation formatting — port of `SqlValidationFormatting.swift`.
//!
//! Pure, UI-free. Formats `SqlValidationResult` / `SqlValidationIssue` for
//! display. Depends on the existing `sqlly-catalog::validation` types.

use sqlly_ai::catalog::validation::{SqlIssueSeverity, SqlValidationIssue, SqlValidationResult};

#[allow(dead_code)]
/// Sorts issues by severity rank descending (error=3, requireConfirmation=2,
/// warning=1, info=0). Stable on equal severity so original ordering is
/// preserved within a rank.
pub fn rank(severity: SqlIssueSeverity) -> u8 {
    match severity {
        SqlIssueSeverity::Error => 3,
        SqlIssueSeverity::RequireConfirmation => 2,
        SqlIssueSeverity::Warning => 1,
        SqlIssueSeverity::Info => 0,
    }
}

#[allow(dead_code)]
/// Human-readable label for a severity.
pub fn label(severity: SqlIssueSeverity) -> String {
    let key = match severity {
        SqlIssueSeverity::Error => "query.sql_validation.severity_error",
        SqlIssueSeverity::RequireConfirmation => "query.sql_validation.severity_confirmation",
        SqlIssueSeverity::Warning => "query.sql_validation.severity_warning",
        SqlIssueSeverity::Info => "query.sql_validation.severity_info",
    };
    crate::localization::text(key)
}

#[allow(dead_code)]
/// Returns issues sorted by severity rank descending, preserving original
/// order within a rank (stable sort).
pub fn ordered_issues(issues: &[SqlValidationIssue]) -> Vec<&SqlValidationIssue> {
    let mut indexed: Vec<(usize, &SqlValidationIssue)> = issues.iter().enumerate().collect();
    indexed.sort_by(|a, b| {
        rank(b.1.severity)
            .cmp(&rank(a.1.severity))
            .then(a.0.cmp(&b.0))
    });
    indexed.into_iter().map(|(_, i)| i).collect()
}

#[allow(dead_code)]
/// Whether the result has any blocking or confirmation-level issues (i.e. the
/// user should be cautioned before proceeding).
pub fn is_cautionary(result: &SqlValidationResult) -> bool {
    !result.is_valid || result.requires_confirmation
}

/// "N issue(s)" / "N problem(s)" style phrase.
pub fn issue_count_phrase(count: usize) -> String {
    let key = if count == 1 {
        "query.sql_validation.issue_count_one"
    } else {
        "query.sql_validation.issue_count_many"
    };
    if count == 1 {
        crate::localization::text(key)
    } else {
        crate::localization::format(key, &[("count", &count.to_string())])
    }
}

/// A one-line summary of the result: "OK" when valid and non-cautionary;
/// otherwise the issue count phrase.
pub fn summary(result: &SqlValidationResult) -> String {
    if result.is_valid && !result.requires_confirmation {
        if result.issues.is_empty() {
            crate::localization::text("query.sql_validation.summary_ok")
        } else {
            issue_count_phrase(result.issues.len())
        }
    } else if result.requires_confirmation {
        crate::localization::format(
            "query.sql_validation.summary_confirmation",
            &[("issue_count", &issue_count_phrase(result.issues.len()))],
        )
    } else {
        issue_count_phrase(result.issues.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_ai::catalog::validation::{SqlIssueCode, SqlValidationIssue, SqlValidationResult};

    fn issue(severity: SqlIssueSeverity, msg: &str) -> SqlValidationIssue {
        SqlValidationIssue {
            code: SqlIssueCode::ParseError,
            severity,
            message: msg.to_string(),
            identifier: None,
            object_keys: Vec::new(),
            span: None,
        }
    }

    #[test]
    fn rank_ordering() {
        assert!(rank(SqlIssueSeverity::Error) > rank(SqlIssueSeverity::RequireConfirmation));
        assert!(rank(SqlIssueSeverity::RequireConfirmation) > rank(SqlIssueSeverity::Warning));
        assert!(rank(SqlIssueSeverity::Warning) > rank(SqlIssueSeverity::Info));
    }

    #[test]
    fn labels() {
        assert_eq!(label(SqlIssueSeverity::Error), "Error");
        assert_eq!(
            label(SqlIssueSeverity::RequireConfirmation),
            "Unconstrained operation"
        );
        assert_eq!(label(SqlIssueSeverity::Warning), "Warning");
        assert_eq!(label(SqlIssueSeverity::Info), "Info");
    }

    #[test]
    fn ordered_issues_by_severity() {
        let issues = vec![
            issue(SqlIssueSeverity::Info, "info"),
            issue(SqlIssueSeverity::Error, "err"),
            issue(SqlIssueSeverity::Warning, "warn"),
            issue(SqlIssueSeverity::RequireConfirmation, "conf"),
        ];
        let ordered = ordered_issues(&issues);
        assert_eq!(ordered[0].severity, SqlIssueSeverity::Error);
        assert_eq!(ordered[1].severity, SqlIssueSeverity::RequireConfirmation);
        assert_eq!(ordered[2].severity, SqlIssueSeverity::Warning);
        assert_eq!(ordered[3].severity, SqlIssueSeverity::Info);
    }

    #[test]
    fn ordered_issues_stable_within_rank() {
        let issues = vec![
            issue(SqlIssueSeverity::Warning, "first"),
            issue(SqlIssueSeverity::Warning, "second"),
            issue(SqlIssueSeverity::Warning, "third"),
        ];
        let ordered = ordered_issues(&issues);
        assert_eq!(ordered[0].message, "first");
        assert_eq!(ordered[1].message, "second");
        assert_eq!(ordered[2].message, "third");
    }

    #[test]
    fn cautionary_when_invalid() {
        let result = SqlValidationResult {
            is_valid: false,
            requires_confirmation: false,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: vec![issue(SqlIssueSeverity::Error, "err")],
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert!(is_cautionary(&result));
    }

    #[test]
    fn cautionary_when_requires_confirmation() {
        let result = SqlValidationResult {
            is_valid: false,
            requires_confirmation: true,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: Vec::new(),
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert!(is_cautionary(&result));
    }

    #[test]
    fn not_cautionary_when_valid() {
        let result = SqlValidationResult {
            is_valid: true,
            requires_confirmation: false,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: Vec::new(),
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert!(!is_cautionary(&result));
    }

    #[test]
    fn issue_count_singular_and_plural() {
        assert_eq!(issue_count_phrase(1), "1 issue");
        assert_eq!(issue_count_phrase(3), "3 issues");
        assert_eq!(issue_count_phrase(0), "0 issues");
    }

    #[test]
    fn summary_ok_no_issues() {
        let result = SqlValidationResult {
            is_valid: true,
            requires_confirmation: false,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: Vec::new(),
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert_eq!(summary(&result), "OK");
    }

    #[test]
    fn summary_with_issues() {
        let result = SqlValidationResult {
            is_valid: true,
            requires_confirmation: false,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: vec![issue(SqlIssueSeverity::Warning, "w")],
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert_eq!(summary(&result), "1 issue");
    }

    #[test]
    fn summary_requires_confirmation() {
        let result = SqlValidationResult {
            is_valid: false,
            requires_confirmation: true,
            detected_intent: sqlly_ai::catalog::SqlIntent::Select,
            issues: vec![issue(SqlIssueSeverity::RequireConfirmation, "c")],
            referenced_objects: Vec::new(),
            referenced_columns: Vec::new(),
        };
        assert_eq!(summary(&result), "1 issue (unconstrained operation)");
    }
}
