//! # sqlly-driver
//!
//! The shared vocabulary of every database driver crate. `sqlly-tds` grew the
//! original shapes (`SqlServerTransport`, `ConnectionService`, the row
//! batching in `execute_on_client`); this crate generalizes them so the
//! PostgreSQL-wire, MySQL-wire, and SQLite drivers speak the exact same
//! streaming contract without re-implementing it:
//!
//! - [`SqlTransport`] — the per-engine transport trait (test/list/execute),
//!   identical in shape to `crate::tds::SqlServerTransport`.
//! - [`ConnectionService`] — lifecycle-event wrapper around a transport,
//!   a generalized copy of the TDS one.
//! - [`QueryStreamer`] — the event/row-batching state machine every driver's
//!   `execute_on_client` feeds: `QueryStarted → ResultSetStarted{columns} →
//!   row batches (≤256 rows or 60 ms old) → RowCount → QueryCompleted`, rows
//!   over the bounded [`RowSender`] when provided (backpressure, zero drops)
//!   or the broadcast [`EventBus`] otherwise.
//!
//! Timer-driven partial flushes: the age check on `push_row` covers steadily
//! flowing streams, and drivers whose item loop has real await points (the
//! PostgreSQL and MySQL wire drivers) additionally race
//! [`QueryStreamer::pending_flush_deadline`] against their next item so a
//! stalled server stream still flushes its partial batch at the shared
//! cadence — the same guarantee the TDS pump gets from its internal `select!`
//! timer. The in-process SQLite driver steps synchronously with no await
//! points between rows, so a timer could never run mid-statement there; its
//! rows either flow (age check) or the statement finishes (final flush).

#![forbid(unsafe_code)]

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, DatabaseSummary, EventBus, EventPayload, ResultColumn, RowBatch,
    RowSender, SessionId, StudioResult,
};
use std::time::{Duration, Instant};

/// Rows are coalesced into batches of this size before being flushed to the
/// bounded row channel. Matches the TDS driver.
pub const ROW_BATCH_TARGET: usize = 256;

/// Maximum age of a partially-filled batch before the next pushed row flushes
/// it regardless of size, so the grid fills progressively. Matches TDS.
pub const ROW_FLUSH_INTERVAL: Duration = Duration::from_millis(60);

/// The transport seam every native driver implements — same shape as
/// `crate::tds::SqlServerTransport`, engine-neutral. Connections are opened
/// per call from the profile; long-lived clients are the driver crates'
/// `open_client`/`execute_on_client` pairs.
#[async_trait]
pub trait SqlTransport: Send + Sync {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()>;
    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>>;
    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()>;
}

/// Lifecycle-event wrapper around a transport: publishes
/// ConnectionTesting/Opened/Closed and DatabasesListed around the transport
/// calls, exactly like the TDS `ConnectionService`.
#[derive(Clone)]
pub struct ConnectionService<T> {
    transport: T,
    events: EventBus,
}

impl<T> ConnectionService<T>
where
    T: SqlTransport,
{
    pub fn new(transport: T, events: EventBus) -> Self {
        Self { transport, events }
    }

    pub async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _ = self.events.publish(EventPayload::ConnectionTesting {
            connection_id: profile.id,
        });
        self.transport.test_connection(profile).await?;
        self.events.publish(EventPayload::ConnectionOpened {
            connection_id: profile.id,
        })?;
        Ok(())
    }

    pub fn disconnect(&self, connection_id: sqlly_core::ConnectionId) -> StudioResult<()> {
        self.events
            .publish(EventPayload::ConnectionClosed { connection_id })?;
        Ok(())
    }

    pub async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let databases = self.transport.list_databases(profile).await?;
        self.events.publish(EventPayload::DatabasesListed {
            connection_id: profile.id,
            databases: databases.clone(),
        })?;
        Ok(databases)
    }
}

/// The streaming state machine a driver feeds while it walks its native
/// result stream. Owns session id, result-set indexing, per-set row counts,
/// and the batching/flush policy; guarantees the TDS event sequence.
pub struct QueryStreamer<'a> {
    events: &'a EventBus,
    rows_tx: Option<&'a RowSender>,
    session_id: SessionId,
    started: Instant,
    result_set_index: usize,
    current_result_set: Option<usize>,
    current_row_count: u64,
    pending_rows: Vec<Vec<CellValue>>,
    pending_rsi: Option<usize>,
    pending_since: Option<Instant>,
    consumer_gone: bool,
}

impl<'a> QueryStreamer<'a> {
    /// Publish `QueryStarted` and hand back the streamer.
    pub fn begin(events: &'a EventBus, rows_tx: Option<&'a RowSender>) -> StudioResult<Self> {
        let session_id = SessionId::new();
        events.publish(EventPayload::QueryStarted {
            session_id: session_id.clone(),
        })?;
        Ok(Self {
            events,
            rows_tx,
            session_id,
            started: Instant::now(),
            result_set_index: 0,
            current_result_set: None,
            current_row_count: 0,
            pending_rows: Vec::with_capacity(ROW_BATCH_TARGET),
            pending_rsi: None,
            pending_since: None,
            consumer_gone: false,
        })
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// True once the row consumer dropped its receiver — the driver should
    /// stop pulling from the server.
    pub fn consumer_gone(&self) -> bool {
        self.consumer_gone
    }

    /// Start the next result set: flushes any pending batch, closes the
    /// previous set with its `RowCount`, publishes `ResultSetStarted`.
    pub async fn begin_result_set(&mut self, columns: Vec<ResultColumn>) -> StudioResult<()> {
        self.flush_pending().await;
        if self.current_result_set.is_some() {
            self.events.publish(EventPayload::RowCount {
                session_id: self.session_id.clone(),
                rows_affected: self.current_row_count,
            })?;
        }
        self.events.publish(EventPayload::ResultSetStarted {
            session_id: self.session_id.clone(),
            result_set_index: self.result_set_index,
            columns,
        })?;
        self.current_result_set = Some(self.result_set_index);
        self.result_set_index += 1;
        self.current_row_count = 0;
        Ok(())
    }

    /// Push one row of the current result set. Returns `Ok(false)` when the
    /// row consumer is gone and the driver should stop reading.
    pub async fn push_row(&mut self, cells: Vec<CellValue>) -> StudioResult<bool> {
        if self.consumer_gone {
            return Ok(false);
        }
        self.current_row_count += 1;
        let rsi = self.current_result_set.unwrap_or(0);
        match self.rows_tx {
            Some(_) => {
                if self.pending_rows.is_empty() {
                    self.pending_since = Some(Instant::now());
                }
                self.pending_rsi = Some(rsi);
                self.pending_rows.push(cells);
                let size_ready = self.pending_rows.len() >= ROW_BATCH_TARGET;
                let age_ready = self
                    .pending_since
                    .is_some_and(|since| since.elapsed() >= ROW_FLUSH_INTERVAL);
                if size_ready || age_ready {
                    self.flush_pending().await;
                }
                Ok(!self.consumer_gone)
            }
            None => {
                self.events.publish(EventPayload::Rows {
                    session_id: self.session_id.clone(),
                    result_set_index: rsi,
                    rows: vec![cells],
                })?;
                Ok(true)
            }
        }
    }

    /// Whether a partially-filled batch is waiting to be flushed.
    pub fn has_pending_rows(&self) -> bool {
        !self.pending_rows.is_empty()
    }

    /// The instant at which the currently-pending partial batch goes stale
    /// (`None` when nothing is pending). Drivers with awaitable item loops
    /// race this against their next item and call [`Self::flush_now`] when it
    /// fires, so a stalled stream still surfaces buffered rows on time.
    pub fn pending_flush_deadline(&self) -> Option<Instant> {
        self.pending_since
            .filter(|_| !self.pending_rows.is_empty())
            .map(|since| since + ROW_FLUSH_INTERVAL)
    }

    /// Flush the pending partial batch immediately, regardless of age.
    pub async fn flush_now(&mut self) {
        self.flush_pending().await;
    }

    /// Report a statement that produced no result set (INSERT/UPDATE/DDL):
    /// publishes its affected-row count without opening a result set.
    pub fn statement_completed(&mut self, rows_affected: u64) -> StudioResult<()> {
        self.events.publish(EventPayload::RowCount {
            session_id: self.session_id.clone(),
            rows_affected,
        })?;
        Ok(())
    }

    /// Announce that the session's database is no longer the one the profile
    /// names — a `USE` the engine confirmed against the server. The app retargets
    /// the tab and notes it in Messages; without it the UI keeps showing the
    /// profile's database while statements run somewhere else.
    pub fn database_changed(&self, name: impl Into<String>) -> StudioResult<()> {
        self.events.publish(EventPayload::DatabaseChanged {
            session_id: self.session_id.clone(),
            name: name.into(),
        })?;
        Ok(())
    }

    /// Surface an informational engine message (notices, warnings).
    pub fn message(&self, text: impl Into<String>) -> StudioResult<()> {
        self.events.publish(EventPayload::Message {
            session_id: self.session_id.clone(),
            text: text.into(),
        })?;
        Ok(())
    }

    /// Flush remaining rows, close the open result set with its `RowCount`,
    /// and publish `QueryCompleted` with the elapsed time.
    pub async fn finish(mut self) -> StudioResult<()> {
        self.flush_pending().await;
        if self.current_result_set.is_some() {
            self.events.publish(EventPayload::RowCount {
                session_id: self.session_id.clone(),
                rows_affected: self.current_row_count,
            })?;
        }
        self.events.publish(EventPayload::QueryCompleted {
            session_id: self.session_id,
            elapsed_ms: self.started.elapsed().as_millis(),
        })?;
        Ok(())
    }

    async fn flush_pending(&mut self) {
        let (Some(tx), Some(rsi)) = (self.rows_tx, self.pending_rsi) else {
            return;
        };
        if self.pending_rows.is_empty() {
            return;
        }
        let rows = std::mem::take(&mut self.pending_rows);
        self.pending_since = None;
        if tx
            .send(RowBatch::new(self.session_id.clone(), rsi, rows))
            .await
            .is_err()
        {
            self.consumer_gone = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef, row_channel};

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "t",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        )
    }

    fn col(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: "text".into(),
            nullable: true,
        }
    }

    struct MockTransport {
        fail_test: bool,
    }

    #[async_trait]
    impl SqlTransport for MockTransport {
        async fn test_connection(&self, _profile: &ConnectionProfile) -> StudioResult<()> {
            if self.fail_test {
                Err(sqlly_core::StudioError::Connection("nope".into()))
            } else {
                Ok(())
            }
        }

        async fn list_databases(
            &self,
            _profile: &ConnectionProfile,
        ) -> StudioResult<Vec<DatabaseSummary>> {
            Ok(vec![DatabaseSummary {
                name: "db1".into(),
                state: "ONLINE".into(),
                compatibility_level: None,
            }])
        }

        async fn execute_query(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            events: &EventBus,
            rows_tx: Option<&RowSender>,
        ) -> StudioResult<()> {
            let mut s = QueryStreamer::begin(events, rows_tx)?;
            s.begin_result_set(vec![ResultColumn {
                name: "n".into(),
                data_type: "int".into(),
                nullable: false,
            }])
            .await?;
            s.push_row(vec![CellValue::I64(1)]).await?;
            s.finish().await
        }
    }

    fn payloads(sub: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
        let mut out = Vec::new();
        while let Ok(event) = sub.try_recv() {
            out.push(event.payload);
        }
        out
    }

    #[tokio::test]
    async fn connection_service_publishes_lifecycle_events() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let service = ConnectionService::new(MockTransport { fail_test: false }, events);
        service.test_connection(&profile()).await.unwrap();
        let seen = payloads(&mut sub);
        assert!(matches!(seen[0], EventPayload::ConnectionTesting { .. }));
        assert!(matches!(seen[1], EventPayload::ConnectionOpened { .. }));
    }

    #[tokio::test]
    async fn connection_service_failure_skips_opened_event() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let service = ConnectionService::new(MockTransport { fail_test: true }, events);
        assert!(service.test_connection(&profile()).await.is_err());
        let seen = payloads(&mut sub);
        assert!(matches!(seen[0], EventPayload::ConnectionTesting { .. }));
        assert_eq!(seen.len(), 1);
    }

    #[tokio::test]
    async fn connection_service_lists_databases_and_publishes() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let service = ConnectionService::new(MockTransport { fail_test: false }, events);
        let dbs = service.list_databases(&profile()).await.unwrap();
        assert_eq!(dbs.len(), 1);
        assert!(payloads(&mut sub).iter().any(
            |p| matches!(p, EventPayload::DatabasesListed { databases, .. } if databases.len() == 1)
        ));
    }

    #[tokio::test]
    async fn streamer_event_sequence_matches_tds_contract() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let (tx, mut rx) = row_channel(16);

        let mut s = QueryStreamer::begin(&events, Some(&tx)).unwrap();
        s.begin_result_set(vec![col("a")]).await.unwrap();
        s.push_row(vec![CellValue::I64(1)]).await.unwrap();
        s.push_row(vec![CellValue::I64(2)]).await.unwrap();
        s.begin_result_set(vec![col("b")]).await.unwrap();
        s.push_row(vec![CellValue::String("x".into())])
            .await
            .unwrap();
        s.finish().await.unwrap();
        drop(tx);

        let seen = payloads(&mut sub);
        assert!(matches!(seen[0], EventPayload::QueryStarted { .. }));
        assert!(
            matches!(&seen[1], EventPayload::ResultSetStarted { result_set_index: 0, columns, .. } if columns[0].name == "a")
        );
        // First set closed with its row count when the second begins.
        assert!(matches!(
            seen[2],
            EventPayload::RowCount {
                rows_affected: 2,
                ..
            }
        ));
        assert!(matches!(
            &seen[3],
            EventPayload::ResultSetStarted {
                result_set_index: 1,
                ..
            }
        ));
        assert!(matches!(
            seen[4],
            EventPayload::RowCount {
                rows_affected: 1,
                ..
            }
        ));
        assert!(matches!(seen[5], EventPayload::QueryCompleted { .. }));

        // Rows arrived on the row channel, attributed to the right sets.
        let batch1 = rx.recv().await.unwrap();
        assert_eq!(batch1.result_set_index, 0);
        assert_eq!(batch1.rows.len(), 2);
        let batch2 = rx.recv().await.unwrap();
        assert_eq!(batch2.result_set_index, 1);
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn streamer_flushes_on_batch_size() {
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let (tx, mut rx) = row_channel(16);
        let mut s = QueryStreamer::begin(&events, Some(&tx)).unwrap();
        s.begin_result_set(vec![col("a")]).await.unwrap();
        for i in 0..ROW_BATCH_TARGET {
            s.push_row(vec![CellValue::I64(i as i64)]).await.unwrap();
        }
        // A full batch flushed without waiting for finish().
        let batch = rx.recv().await.expect("size-triggered flush");
        assert_eq!(batch.rows.len(), ROW_BATCH_TARGET);
        s.finish().await.unwrap();
    }

    #[tokio::test]
    async fn streamer_age_flush_surfaces_trickling_rows() {
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let (tx, mut rx) = row_channel(16);
        let mut s = QueryStreamer::begin(&events, Some(&tx)).unwrap();
        s.begin_result_set(vec![col("a")]).await.unwrap();
        s.push_row(vec![CellValue::I64(1)]).await.unwrap();
        tokio::time::sleep(ROW_FLUSH_INTERVAL + Duration::from_millis(5)).await;
        s.push_row(vec![CellValue::I64(2)]).await.unwrap();
        let batch = rx.recv().await.expect("age-triggered flush");
        assert_eq!(batch.rows.len(), 2);
        s.finish().await.unwrap();
    }

    #[tokio::test]
    async fn streamer_falls_back_to_bus_without_row_channel() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let mut s = QueryStreamer::begin(&events, None).unwrap();
        s.begin_result_set(vec![col("a")]).await.unwrap();
        s.push_row(vec![CellValue::I64(1)]).await.unwrap();
        s.finish().await.unwrap();
        assert!(
            payloads(&mut sub)
                .iter()
                .any(|p| matches!(p, EventPayload::Rows { rows, .. } if rows.len() == 1))
        );
    }

    #[tokio::test]
    async fn streamer_reports_dropped_consumer() {
        let events = EventBus::new(64);
        let _sub = events.subscribe();
        let (tx, rx) = row_channel(1);
        drop(rx);
        let mut s = QueryStreamer::begin(&events, Some(&tx)).unwrap();
        s.begin_result_set(vec![col("a")]).await.unwrap();
        // Fill a whole batch so the flush runs and detects the closed channel.
        let mut keep_going = true;
        for i in 0..ROW_BATCH_TARGET {
            keep_going = s.push_row(vec![CellValue::I64(i as i64)]).await.unwrap();
        }
        assert!(!keep_going, "closed channel must stop the driver");
        assert!(s.consumer_gone());
        s.finish().await.unwrap();
    }

    #[tokio::test]
    async fn statement_completed_publishes_row_count() {
        let events = EventBus::new(64);
        let mut sub = events.subscribe();
        let mut s = QueryStreamer::begin(&events, None).unwrap();
        s.statement_completed(7).unwrap();
        s.finish().await.unwrap();
        assert!(payloads(&mut sub).iter().any(|p| matches!(
            p,
            EventPayload::RowCount {
                rows_affected: 7,
                ..
            }
        )));
    }
}
