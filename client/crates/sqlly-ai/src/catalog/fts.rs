//! FTS5 query helpers.

use crate::catalog::tokens::tokenize_terms;

/// Build a conservative FTS5 MATCH expression from user text.
///
/// The returned string contains only quoted tokens and `OR`, so callers can pass
/// it as a bound SQL parameter without exposing raw FTS syntax to user input.
pub fn build_match_query(input: &str) -> Option<String> {
    let terms = tokenize_terms(input)
        .into_iter()
        .map(|term| term.replace('"', "\"\""))
        .map(|term| {
            if term.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                format!("\"{}\"*", term)
            } else {
                format!("\"{}\"", term)
            }
        })
        .collect::<Vec<_>>();

    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_fts_special_characters() {
        assert_eq!(
            build_match_query("email* - OR \"x\""),
            Some("\"email\"*".into())
        );
    }

    #[test]
    fn empty_query_returns_none() {
        assert_eq!(build_match_query("* - AND OR"), None);
    }
}
