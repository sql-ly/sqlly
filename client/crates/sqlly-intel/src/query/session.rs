use serde::{Deserialize, Serialize};
use sqlly_core::{ConnectionId, SessionId, SessionState};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuerySession {
    pub id: SessionId,
    pub connection_id: ConnectionId,
    pub database: String,
    pub state: SessionState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub query_text: Option<String>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub elapsed: Option<std::time::Duration>,
}

impl QuerySession {
    pub fn new(connection_id: ConnectionId, database: String) -> Self {
        Self {
            id: SessionId::new(),
            connection_id,
            database,
            state: SessionState::Idle,
            created_at: chrono::Utc::now(),
            query_text: None,
            started_at: None,
            elapsed: None,
        }
    }

    pub fn start_execution(&mut self) {
        self.state = SessionState::Executing;
        self.started_at = Some(chrono::Utc::now());
    }

    pub fn cancel_execution(&mut self) {
        self.state = SessionState::Canceling;
    }

    pub fn complete_execution(&mut self) {
        if let Some(started) = self.started_at {
            self.elapsed = Some(
                chrono::Utc::now()
                    .signed_duration_since(started)
                    .to_std()
                    .unwrap_or_default(),
            );
        }
        self.state = SessionState::Completed;
    }

    pub fn fail_execution(&mut self) {
        self.state = SessionState::Failed;
    }
}
