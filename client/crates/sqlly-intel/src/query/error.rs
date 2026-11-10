use thiserror::Error;

pub type QueryOutcome<T> = Result<T, QueryError>;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error("Empty query")]
    EmptyQuery,

    #[error("No active connection for session")]
    NoConnection,

    #[error("Query execution failed: {0}")]
    ExecutionFailed(String),

    #[error("Query timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("Query cancelled")]
    Cancelled,

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("{0}")]
    Other(String),
}

impl QueryError {
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage;
        match self {
            Self::EmptyQuery => {
                DisplayMessage::new("services.query.errors.empty", self.to_string())
            }
            Self::NoConnection => {
                DisplayMessage::new("services.query.errors.no_connection", self.to_string())
            }
            Self::ExecutionFailed(detail) => {
                DisplayMessage::new("services.query.errors.execution_failed", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Timeout(duration) => {
                DisplayMessage::new("services.query.errors.timeout", self.to_string())
                    .with_arg("duration", format!("{duration:?}"))
            }
            Self::Cancelled => {
                DisplayMessage::new("services.query.errors.cancelled", self.to_string())
            }
            Self::SessionNotFound(session) => {
                DisplayMessage::new("services.query.errors.session_not_found", self.to_string())
                    .with_arg("session_id", session)
            }
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
