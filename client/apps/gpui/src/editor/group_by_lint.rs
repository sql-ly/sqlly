//! Live GROUP BY lint: underline a select-list column that a grouped query
//! neither aggregates nor groups by — SQL Server's error 8120 — and offer a
//! "Fix this" link that writes the missing columns into the GROUP BY.
//!
//! The analysis lives in [`sqlly_intel::query::group_by`], shared with
//! completion (which puts the same columns first inside a GROUP BY list).
//! Pure functions, so the debounced lint task can run them off the main
//! thread.

use super::types::{DiagnosticFix, DiagnosticSeverity, DiagnosticSource, ValidationDiagnostic};
use sqlly_intel::query::group_by::ungrouped_columns;
use sqlly_lang::sqlmodel::Dialect;

/// How the dialect treats an ungrouped select-list column: an error where
/// the engine always rejects it, a warning where a server setting decides
/// (MySQL / MariaDB's `ONLY_FULL_GROUP_BY`), nothing where it is allowed
/// (SQLite picks a value from the group).
fn severity_for(dialect: Dialect) -> Option<DiagnosticSeverity> {
    match dialect {
        Dialect::SqlServer | Dialect::Postgres | Dialect::Oracle => Some(DiagnosticSeverity::Error),
        Dialect::MySql => Some(DiagnosticSeverity::Warning),
        Dialect::Sqlite => None,
    }
}

/// One diagnostic per ungrouped select-list column.
pub(crate) fn group_by_lint_diagnostics(
    content: &str,
    dialect: Dialect,
) -> Vec<ValidationDiagnostic> {
    let Some(severity) = severity_for(dialect) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for finding in ungrouped_columns(content) {
        let names: Vec<&str> = finding.missing.iter().map(|m| m.text.as_str()).collect();
        let label = crate::localization::format(
            "editor.group_by_lint.fix_label",
            &[("columns", &names.join(", "))],
        );
        for column in &finding.missing {
            out.push(ValidationDiagnostic {
                severity,
                source: DiagnosticSource::Lint,
                message: crate::localization::format(
                    "editor.group_by_lint.diagnostic",
                    &[("column", &column.text)],
                ),
                line: content[..column.start.min(content.len())]
                    .matches('\n')
                    .count(),
                identifier: Some(column.text.clone()),
                start_byte: column.start,
                end_byte: column.end,
                explainer: Some(format!(
                    "This query groups its rows (it aggregates, or has a GROUP BY), so each \
                     result row stands for a whole group. '{0}' can differ from row to row \
                     within a group, so the database can't tell which value to show. Group by \
                     '{0}' to get one row per value, or wrap it in an aggregate such as \
                     MAX({0}).",
                    column.text
                )),
                fix: Some(DiagnosticFix::AddToGroupBy {
                    label: label.clone(),
                }),
            });
        }
    }
    out
}

/// The buffer with the GROUP BY fixed for the query whose ungrouped column
/// covers `offset`, and where the caret lands. Recomputed from the current
/// buffer, so it is never stale.
pub(super) fn group_by_fix_at(content: &str, offset: usize) -> Option<(String, usize)> {
    ungrouped_columns(content)
        .into_iter()
        .find(|finding| {
            finding
                .missing
                .iter()
                .any(|m| m.start <= offset && offset <= m.end)
        })
        .map(|finding| (finding.fixed_sql, finding.fixed_cursor))
}

/// Wrap `text` to lines of at most `width` characters, breaking at spaces.
pub(super) fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPORTED: &str = "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\n";

    #[test]
    fn the_reported_query_underlines_category_with_an_explainer_and_fix() {
        let diags = group_by_lint_diagnostics(REPORTED, Dialect::SqlServer);
        assert_eq!(diags.len(), 1);
        let diag = &diags[0];
        assert_eq!(&REPORTED[diag.start_byte..diag.end_byte], "Category");
        assert_eq!(diag.severity, DiagnosticSeverity::Error);
        assert_eq!(diag.source, DiagnosticSource::Lint);
        assert_eq!(diag.line, 1);
        assert!(
            diag.message
                .contains("Column 'Category' is invalid in the select list")
        );
        assert!(diag.explainer.as_deref().unwrap().contains("MAX(Category)"));
        assert_eq!(
            diag.fix.as_ref().map(DiagnosticFix::label),
            Some("Fix this: add Category to GROUP BY")
        );
    }

    #[test]
    fn the_fix_writes_the_group_by_and_clears_the_lint() {
        let offset = REPORTED.find("Category").unwrap() + 2;
        let (fixed, cursor) = group_by_fix_at(REPORTED, offset).expect("a fix");
        assert_eq!(
            fixed,
            "SELECT\n    Category,\n    COUNT(1)\nfrom\n    Shared.Settings\ngroup by\n    Category\n"
        );
        assert_eq!(&fixed[..cursor], fixed.trim_end());
        assert!(group_by_lint_diagnostics(&fixed, Dialect::SqlServer).is_empty());
        assert!(
            group_by_fix_at(REPORTED, 0).is_none(),
            "no fix away from a finding"
        );
    }

    #[test]
    fn severity_follows_the_dialect() {
        assert_eq!(
            group_by_lint_diagnostics(REPORTED, Dialect::MySql)[0].severity,
            DiagnosticSeverity::Warning
        );
        assert_eq!(
            group_by_lint_diagnostics(REPORTED, Dialect::Postgres)[0].severity,
            DiagnosticSeverity::Error
        );
        assert!(group_by_lint_diagnostics(REPORTED, Dialect::Sqlite).is_empty());
    }

    #[test]
    fn every_missing_column_is_underlined_and_the_fix_names_them_all() {
        let sql = "SELECT a, b, COUNT(1) FROM t";
        let diags = group_by_lint_diagnostics(sql, Dialect::SqlServer);
        assert_eq!(diags.len(), 2);
        assert_eq!(
            diags[1].fix.as_ref().map(DiagnosticFix::label),
            Some("Fix this: add a, b to GROUP BY")
        );
    }

    #[test]
    fn wrap_words_breaks_at_spaces() {
        assert_eq!(
            wrap_words("one two three four", 9),
            vec!["one two", "three", "four"]
        );
    }
}
