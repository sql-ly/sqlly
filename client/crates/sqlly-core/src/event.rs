use crate::{ConnectionId, DatabaseSummary, ObjectNode, SessionId, StudioError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventId(pub Uuid);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultColumn {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CellValue {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
}

/// An owned SQL parameter value for parameterized statements (`@P1`, `@P2`, …).
/// Lives in core (rather than the TDS crate that binds it to tiberius) so the
/// pure SQL builders in `sqlly-services::row_editing` compile on every target,
/// including wasm; `sqlly-tds` re-exports it for its callers.
///
/// `Null` binds as an untyped (nvarchar) NULL; callers that need a typed NULL
/// (e.g. assigning NULL to `varbinary`) should render `NULL` into the SQL text
/// instead of binding a parameter, since SQL Server does not implicitly
/// convert nvarchar NULL to every type.
#[derive(Debug, Clone, PartialEq)]
pub enum SqlParam {
    Null,
    Bool(bool),
    I64(i64),
    F64(f64),
    String(String),
    Bytes(Vec<u8>),
}

impl From<&CellValue> for SqlParam {
    fn from(value: &CellValue) -> Self {
        match value {
            CellValue::Null => SqlParam::Null,
            CellValue::Bool(b) => SqlParam::Bool(*b),
            CellValue::I64(n) => SqlParam::I64(*n),
            CellValue::F64(f) => SqlParam::F64(*f),
            CellValue::String(s) => SqlParam::String(s.clone()),
            CellValue::Bytes(b) => SqlParam::Bytes(b.clone()),
        }
    }
}

/// One fully materialized result set (columns + all rows). Shared shape for
/// small targeted reads; `sqlly-tds` re-exports it for its callers.
#[derive(Debug, Clone, Default)]
pub struct MaterializedResultSet {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventPayload {
    ConnectionTesting {
        connection_id: ConnectionId,
    },
    ConnectionOpened {
        connection_id: ConnectionId,
    },
    ConnectionClosed {
        connection_id: ConnectionId,
    },
    DatabasesListed {
        connection_id: ConnectionId,
        databases: Vec<DatabaseSummary>,
    },
    QueryStarted {
        session_id: SessionId,
    },
    ResultSetStarted {
        session_id: SessionId,
        result_set_index: usize,
        columns: Vec<ResultColumn>,
    },
    Rows {
        session_id: SessionId,
        result_set_index: usize,
        rows: Vec<Vec<CellValue>>,
    },
    Message {
        session_id: SessionId,
        text: String,
    },
    RowCount {
        session_id: SessionId,
        rows_affected: u64,
    },
    QueryCompleted {
        session_id: SessionId,
        elapsed_ms: u128,
    },
    QueryCancelled {
        session_id: SessionId,
    },
    /// The connection's current database changed during execution (a `USE`
    /// statement took effect). Carries the new database name so the UI can
    /// retarget the tab's session (db picker, status bar, IntelliSense) without
    /// reconnecting. The warm client is discarded afterward since its live
    /// database no longer matches its pool key.
    DatabaseChanged {
        session_id: SessionId,
        name: String,
    },
    ObjectNodesLoaded {
        parent: String,
        nodes: Vec<ObjectNode>,
    },
    Error {
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub occurred_at: DateTime<Utc>,
    pub payload: EventPayload,
}

impl Event {
    pub fn new(payload: EventPayload) -> Self {
        Self {
            id: EventId(Uuid::new_v4()),
            occurred_at: Utc::now(),
            payload,
        }
    }
}

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

pub type EventSubscription = broadcast::Receiver<Event>;

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn publish(&self, payload: EventPayload) -> Result<usize, StudioError> {
        self.tx
            .send(Event::new(payload))
            .map_err(|err| StudioError::InvalidInput(err.to_string()))
    }

    pub fn subscribe(&self) -> EventSubscription {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(1024)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::TryRecvError;

    /// A subscriber that drains concurrently (awaiting each event) receives the
    /// full stream even when it far exceeds the channel capacity — this is the
    /// behavior the CLI `Query` arm now relies on to avoid truncated output.
    /// Uses the multi-thread runtime to mirror the CLI's `#[tokio::main]` so the
    /// producer and the drain task make progress in parallel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_drain_receives_all_events_past_capacity() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let total = 5_000usize; // well past the 1024 default ring.
        let capacity = 1024usize;
        let bus = EventBus::new(capacity);
        let mut sub = bus.subscribe();

        // Tracks how many events the drain task has consumed, so the producer can
        // apply real backpressure instead of guessing with a timed sleep.
        let received_counter = Arc::new(AtomicUsize::new(0));
        let drain_counter = Arc::clone(&received_counter);

        let drain = tokio::spawn(async move {
            loop {
                match sub.recv().await {
                    Ok(_) => {
                        drain_counter.fetch_add(1, Ordering::Release);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        panic!("concurrent drain should not lag");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            drain_counter.load(Ordering::Acquire)
        });

        for i in 0..total {
            // Consumer-driven backpressure: never let the producer get more than
            // half the ring ahead of the drain task. This makes ring overflow
            // impossible regardless of how the runtime schedules the two tasks,
            // so the test is deterministic on fast and slow machines alike (the
            // old timed-sleep throttle could let a slow CI runner overflow and
            // lag). The CLI sees per-row events far slower than this loop, so the
            // "concurrent drain keeps up" property still holds in practice.
            while i.saturating_sub(received_counter.load(Ordering::Acquire)) >= capacity / 2 {
                tokio::task::yield_now().await;
            }
            bus.publish(EventPayload::Message {
                session_id: SessionId::new(),
                text: format!("event {i}"),
            })
            .expect("there is an active subscriber");
        }
        drop(bus); // close the channel so the drain loop ends.

        let received = drain.await.expect("drain task joins");
        assert_eq!(received, total);
    }

    /// Demonstrates the OLD failure mode: publishing the entire stream before
    /// draining overflows the ring, so the first `try_recv` reports `Lagged`.
    #[tokio::test]
    async fn post_hoc_try_recv_lags_when_stream_exceeds_capacity() {
        let bus = EventBus::new(1024);
        let mut sub = bus.subscribe();
        for i in 0..5_000 {
            bus.publish(EventPayload::Message {
                session_id: SessionId::new(),
                text: format!("event {i}"),
            })
            .expect("there is an active subscriber");
        }
        assert!(matches!(sub.try_recv(), Err(TryRecvError::Lagged(_))));
    }
}
