//! Structured error type for engine request failures.
//!
//! Every `request_failed` frame the engine emits carries an [`EngineError`]
//! instead of a bare string, so each failure is categorized at the site that
//! knows what went wrong. On the wire the category renders as a stable
//! lowercase prefix (`catalog: …`, `timeout: …`) that clients can match on
//! without parsing free-form text — except [`EngineError::Query`], whose text
//! comes from the SQL server or driver and reaches the user verbatim (clients
//! and the correlation log rely on seeing e.g. `Msg 208 … Invalid object
//! name 'foo'.` untouched).

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EngineError {
    /// Malformed or unsupported request/response framing: unknown tags,
    /// truncated bodies, oversized frames, invalid dialects.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Local catalog failures: opening the SQLite store, sync, FTS search,
    /// join-path lookup, AI-data invalidation.
    #[error("catalog: {0}")]
    Catalog(String),
    /// LLM / embedding provider failures: ask-schema, SQL generation,
    /// AI-cache and embedding builds.
    #[error("provider: {0}")]
    Provider(String),
    /// A deadline elapsed before the operation completed.
    #[error("timeout: {0}")]
    Timeout(String),
    /// SQL execution and connection errors, passed through verbatim.
    #[error("{0}")]
    Query(String),
    /// A capability the target engine genuinely lacks (no driver for the
    /// family, an unsupported operation on that engine).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Engine-internal failures that fit no other category (task panics,
    /// join errors).
    #[error("internal: {0}")]
    Internal(String),
}

impl EngineError {
    pub fn protocol(message: impl Into<String>) -> Self {
        Self::Protocol(message.into())
    }

    pub fn catalog(message: impl Into<String>) -> Self {
        Self::Catalog(message.into())
    }

    pub fn provider(message: impl Into<String>) -> Self {
        Self::Provider(message.into())
    }

    pub fn timeout(message: impl Into<String>) -> Self {
        Self::Timeout(message.into())
    }

    pub fn query(message: impl Into<String>) -> Self {
        Self::Query(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported(message.into())
    }

    /// Stable machine-readable category, carried on `request_failed` frames
    /// alongside the human-readable message so consumers branch on the code
    /// instead of parsing text. Frozen: existing values never change.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Protocol(_) => "protocol",
            Self::Catalog(_) => "catalog",
            Self::Provider(_) => "provider",
            Self::Timeout(_) => "timeout",
            Self::Query(_) => "query",
            Self::Unsupported(_) => "unsupported",
            Self::Internal(_) => "internal",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::EngineError;

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        let cases = [
            (EngineError::protocol("x"), "protocol"),
            (EngineError::catalog("x"), "catalog"),
            (EngineError::provider("x"), "provider"),
            (EngineError::timeout("x"), "timeout"),
            (EngineError::query("x"), "query"),
            (EngineError::unsupported("x"), "unsupported"),
            (EngineError::internal("x"), "internal"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
        }
    }

    #[test]
    fn categories_render_as_stable_prefixes() {
        assert_eq!(
            EngineError::protocol("unknown request tag 0xff").to_string(),
            "protocol: unknown request tag 0xff"
        );
        assert_eq!(
            EngineError::catalog("open catalog: locked").to_string(),
            "catalog: open catalog: locked"
        );
        assert_eq!(
            EngineError::provider("generate sql: model offline").to_string(),
            "provider: generate sql: model offline"
        );
        assert_eq!(
            EngineError::timeout("ask schema timed out after 30s").to_string(),
            "timeout: ask schema timed out after 30s"
        );
        assert_eq!(
            EngineError::internal("engine task panicked: boom").to_string(),
            "internal: engine task panicked: boom"
        );
    }

    #[test]
    fn query_errors_pass_through_verbatim() {
        assert_eq!(
            EngineError::query("Msg 208, Level 16: Invalid object name 'foo'.").to_string(),
            "Msg 208, Level 16: Invalid object name 'foo'."
        );
    }
}
