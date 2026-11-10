use crate::query::session::QuerySession;
use sqlly_core::{EventBus, EventPayload, StudioError, StudioResult};

pub struct QueryExecutor {
    events: EventBus,
}

impl QueryExecutor {
    pub fn new(events: EventBus) -> Self {
        Self { events }
    }

    pub async fn execute(
        &self,
        session: &mut QuerySession,
        sql: &str,
        database: Option<&str>,
    ) -> StudioResult<()> {
        if sql.trim().is_empty() {
            return Err(StudioError::InvalidInput("Empty query".to_string()));
        }

        tracing::info!(session_id = %session.id, "Executing query");
        session.start_execution();
        session.query_text = Some(sql.to_string());
        if let Some(database) = database {
            session.database = database.to_string();
        }

        self.events.publish(EventPayload::QueryStarted {
            session_id: session.id.clone(),
        })?;
        self.events.publish(EventPayload::Message {
            session_id: session.id.clone(),
            text: format!("Queued query batch ({} bytes)", sql.len()),
        })?;
        session.complete_execution();
        self.events.publish(EventPayload::QueryCompleted {
            session_id: session.id.clone(),
            elapsed_ms: session
                .elapsed
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or_default(),
        })?;
        Ok(())
    }

    pub async fn cancel(&self, session: &mut QuerySession) -> StudioResult<()> {
        tracing::info!(session_id = %session.id, "Cancelling query");
        session.cancel_execution();
        self.events.publish(EventPayload::QueryCancelled {
            session_id: session.id.clone(),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{ConnectionId, EventPayload};

    #[tokio::test]
    async fn executor_publishes_query_lifecycle() {
        let events = EventBus::default();
        let mut subscription = events.subscribe();
        let executor = QueryExecutor::new(events);
        let mut session = QuerySession::new(ConnectionId::new(), "master".to_string());

        executor
            .execute(&mut session, "SELECT 1", Some("tempdb"))
            .await
            .unwrap();

        assert_eq!(session.database, "tempdb");
        assert_eq!(session.query_text.as_deref(), Some("SELECT 1"));
        assert!(matches!(
            subscription.try_recv().unwrap().payload,
            EventPayload::QueryStarted { .. }
        ));
        assert!(matches!(
            subscription.try_recv().unwrap().payload,
            EventPayload::Message { .. }
        ));
        assert!(matches!(
            subscription.try_recv().unwrap().payload,
            EventPayload::QueryCompleted { .. }
        ));
    }
}
