#![forbid(unsafe_code)]
pub mod error;
pub mod executor;
pub mod pg_procedure;
pub mod procedure;
pub mod result;
pub mod safety;
pub mod session;
pub mod stream;

pub mod column_provenance;
pub mod completion;
pub mod group_by;
pub mod history;
pub mod parameters;
pub mod tokenizer;

use sqlly_core::{ConnectionId, EventBus, EventPayload, SessionManager, StudioResult};
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

#[derive(Clone)]
pub struct QueryService {
    events: EventBus,
    sessions: Arc<SessionManager>,
}

impl QueryService {
    pub fn new(events: EventBus, sessions: SessionManager) -> Self {
        Self {
            events,
            sessions: Arc::new(sessions),
        }
    }

    pub async fn execute_text(
        &self,
        connection_id: ConnectionId,
        database: Option<String>,
        sql: String,
    ) -> StudioResult<sqlly_core::SessionId> {
        let session_id = self.sessions.create(
            connection_id,
            database.unwrap_or_else(|| "master".to_string()),
        );
        let started = Instant::now();
        self.events.publish(EventPayload::QueryStarted {
            session_id: session_id.clone(),
        })?;
        self.events.publish(EventPayload::Message {
            session_id: session_id.clone(),
            text: format!("Queued query batch ({} bytes)", sql.len()),
        })?;
        self.events.publish(EventPayload::QueryCompleted {
            session_id: session_id.clone(),
            elapsed_ms: started.elapsed().as_millis(),
        })?;
        Ok(session_id)
    }

    pub fn cancel(&self, session_id: sqlly_core::SessionId) -> StudioResult<()> {
        self.events
            .publish(EventPayload::QueryCancelled { session_id })?;
        Ok(())
    }
}
