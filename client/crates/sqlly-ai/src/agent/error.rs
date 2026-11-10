use thiserror::Error;

pub type AgentResult<T> = Result<T, AgentError>;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("SQL Agent not available: {0}")]
    NotAvailable(String),

    #[error("Job not found: {0}")]
    JobNotFound(String),

    #[error("Job action failed: {0}")]
    ActionFailed(String),

    #[error("{0}")]
    Other(String),
}

impl AgentError {
    pub fn message(&self) -> sqlly_lang::DisplayMessage {
        use sqlly_lang::DisplayMessage;
        match self {
            Self::NotAvailable(detail) => {
                DisplayMessage::new("services.agent.errors.not_available", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::JobNotFound(job) => {
                DisplayMessage::new("services.agent.errors.job_not_found", self.to_string())
                    .with_arg("job_name", job)
            }
            Self::ActionFailed(detail) => {
                DisplayMessage::new("services.agent.errors.action_failed", self.to_string())
                    .with_arg("detail", detail)
            }
            Self::Other(detail) => DisplayMessage::raw(detail),
        }
    }
}
