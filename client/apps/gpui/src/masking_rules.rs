//! Pattern-based sensitive-column matching for result auto-masking.
//!
//! Globbing is implemented by hand (`*` any run, `?` one character, ASCII
//! case-insensitive). Patterns wrapped in slashes (`/…/`) are compiled as
//! case-insensitive regular expressions via the `regex` crate.

use serde::{Deserialize, Serialize};

pub use sqlly_core::{MaskRule, MaskingOverrides, RuleScope};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MaskingRules {
    pub enabled: bool,
    pub patterns: Vec<MaskRule>,
}

impl Default for MaskingRules {
    /// Conservative default set. `email` is intentionally OFF by default: email columns are
    /// used as join/display keys far more often than as secrets, so masking them by default
    /// breaks everyday work; users add `*email*` in Preferences when their policy needs it.
    fn default() -> Self {
        Self {
            enabled: true,
            patterns: [
                "*password*",
                "*passwd*",
                "*secret*",
                "*token*",
                "*api_key*",
                "*apikey*",
                "ssn",
                "*_ssn",
                "ssn_*",
                "social_security*",
                "*credit_card*",
                "*card_number*",
                "*iban*",
            ]
            .iter()
            .map(|p| MaskRule {
                pattern: (*p).to_string(),
                scope: RuleScope::ColumnName,
            })
            .collect(),
        }
    }
}

fn bare_column_name(column: &str) -> &str {
    column.rsplit('.').next().unwrap_or(column)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<char> = pattern.chars().map(|c| c.to_ascii_lowercase()).collect();
    let txt: Vec<char> = text.chars().map(|c| c.to_ascii_lowercase()).collect();
    glob_match_chars(&pat, &txt)
}

fn glob_match_chars(pat: &[char], txt: &[char]) -> bool {
    match pat.first() {
        None => txt.is_empty(),
        Some('*') => {
            let rest = skip_stars(pat);
            glob_match_chars(rest, txt) || (!txt.is_empty() && glob_match_chars(pat, &txt[1..]))
        }
        Some('?') => !txt.is_empty() && glob_match_chars(&pat[1..], &txt[1..]),
        Some(ch) => !txt.is_empty() && txt[0] == *ch && glob_match_chars(&pat[1..], &txt[1..]),
    }
}

fn skip_stars(pat: &[char]) -> &[char] {
    let n = pat.iter().take_while(|c| **c == '*').count();
    &pat[n..]
}

fn regex_inner(pattern: &str) -> Option<&str> {
    if pattern.len() >= 2 && pattern.starts_with('/') && pattern.ends_with('/') {
        Some(&pattern[1..pattern.len() - 1])
    } else {
        None
    }
}

fn pattern_matches(pattern: &str, candidate: &str) -> bool {
    if let Some(inner) = regex_inner(pattern) {
        return regex::RegexBuilder::new(inner)
            .case_insensitive(true)
            .build()
            .map(|re| re.is_match(candidate))
            .unwrap_or(false);
    }
    glob_match(pattern, candidate)
}

pub fn rule_matches(rule: &MaskRule, column: &str, table: Option<&str>) -> bool {
    match rule.scope {
        RuleScope::ColumnName => pattern_matches(&rule.pattern, bare_column_name(column)),
        RuleScope::TableDotColumn => {
            let Some(table) = table else {
                return false;
            };
            let qualified = format!("{table}.{}", bare_column_name(column));
            pattern_matches(&rule.pattern, &qualified)
        }
    }
}

/// Precedence: connection `exclude` > connection `include` > global patterns. `enabled == false` → never.
pub fn should_mask(
    global: &MaskingRules,
    overrides: &MaskingOverrides,
    column: &str,
    table: Option<&str>,
) -> bool {
    if !global.enabled {
        return false;
    }
    if overrides
        .exclude
        .iter()
        .any(|rule| rule_matches(rule, column, table))
    {
        return false;
    }
    if overrides
        .include
        .iter()
        .any(|rule| rule_matches(rule, column, table))
    {
        return true;
    }
    global
        .patterns
        .iter()
        .any(|rule| rule_matches(rule, column, table))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(pattern: &str) -> MaskRule {
        MaskRule {
            pattern: pattern.to_string(),
            scope: RuleScope::ColumnName,
        }
    }

    fn table_rule(pattern: &str) -> MaskRule {
        MaskRule {
            pattern: pattern.to_string(),
            scope: RuleScope::TableDotColumn,
        }
    }

    #[test]
    fn glob_star_is_case_insensitive() {
        assert!(rule_matches(&col("*Password*"), "user_PASSWORD_hash", None));
        assert!(rule_matches(&col("*password*"), "users.password", None));
    }

    #[test]
    fn glob_question_mark() {
        assert!(rule_matches(&col("s?n"), "ssn", None));
        assert!(rule_matches(&col("s?n"), "san", None));
        assert!(!rule_matches(&col("s?n"), "sssn", None));
        assert!(!rule_matches(&col("s?n"), "sn", None));
    }

    #[test]
    fn exact_pattern_does_not_match_superstring() {
        assert!(rule_matches(&col("ssn"), "ssn", None));
        assert!(!rule_matches(&col("ssn"), "lessons", None));
        assert!(!rule_matches(&col("ssn"), "session", None));
    }

    #[test]
    fn star_ssn_does_not_match_lessons_or_session() {
        // `*ssn*` is a consecutive substring glob, not a subsequence. "lessons"
        // and "session" do not contain "ssn", so they are not masked by it.
        assert!(!rule_matches(&col("*ssn*"), "lessons", None));
        assert!(!rule_matches(&col("*ssn*"), "session", None));
        assert!(rule_matches(&col("*ssn*"), "user_ssn", None));
        assert!(rule_matches(&col("*ssn*"), "ssn_value", None));
        assert!(rule_matches(&col("*ssn*"), "fooSSNbar", None));
    }

    #[test]
    fn table_scope_requires_table() {
        assert!(!rule_matches(
            &table_rule("users.password"),
            "password",
            None
        ));
    }

    #[test]
    fn table_scope_matches_qualified() {
        assert!(rule_matches(
            &table_rule("users.password"),
            "password",
            Some("users")
        ));
        assert!(!rule_matches(
            &table_rule("users.password"),
            "password",
            Some("accounts")
        ));
    }

    #[test]
    fn exclude_beats_include_beats_global() {
        let global = MaskingRules::default();
        let mut overrides = MaskingOverrides::default();
        assert!(should_mask(&global, &overrides, "password_hash", None));

        overrides.include.push(col("display_name"));
        assert!(should_mask(&global, &overrides, "display_name", None));

        overrides.exclude.push(col("password_hash"));
        assert!(!should_mask(&global, &overrides, "password_hash", None));
        assert!(should_mask(&global, &overrides, "display_name", None));
    }

    #[test]
    fn disabled_masks_nothing() {
        let global = MaskingRules {
            enabled: false,
            ..MaskingRules::default()
        };
        let mut overrides = MaskingOverrides::default();
        overrides.include.push(col("anything"));
        assert!(!should_mask(&global, &overrides, "password_hash", None));
        assert!(!should_mask(&global, &overrides, "anything", None));
    }

    #[test]
    fn default_set_round_trips_json() {
        let original = MaskingRules::default();
        let json = serde_json::to_string(&original).expect("serialize");
        let back: MaskingRules = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, original);
        let empty = MaskingOverrides::default();
        assert!(should_mask(&back, &empty, "password_hash", None));
        assert!(should_mask(&back, &empty, "ssn", None));
        assert!(should_mask(&back, &empty, "user_ssn", None));
        assert!(!should_mask(&back, &empty, "lessons", None));
        assert!(!should_mask(&back, &empty, "session", None));
        assert!(!should_mask(&back, &empty, "email", None));
    }
}
