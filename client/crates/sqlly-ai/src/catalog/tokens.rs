//! Shared tokenization helpers for schema search and deterministic reranking.

/// Lowercase alphanumeric tokens, dropping tiny terms and common SQL/question noise.
pub(crate) fn tokenize_terms(input: &str) -> Vec<String> {
    input
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|term| term.len() > 1)
        .filter(|term| !is_noise_word(term))
        .map(str::to_string)
        .collect()
}

pub(crate) fn is_noise_word(term: &str) -> bool {
    matches!(
        term.to_ascii_lowercase().as_str(),
        "select"
            | "from"
            | "where"
            | "join"
            | "inner"
            | "left"
            | "right"
            | "outer"
            | "on"
            | "and"
            | "or"
            | "not"
            | "the"
            | "a"
            | "an"
            | "of"
            | "to"
            | "in"
            | "is"
            | "by"
            | "group"
            | "order"
            | "how"
            | "do"
            | "i"
            | "what"
            | "which"
            | "all"
            | "show"
            | "me"
            | "for"
            | "with"
            | "table"
            | "tables"
            | "column"
            | "columns"
            | "list"
            | "get"
            | "find"
    )
}
