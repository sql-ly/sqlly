//! Bounded, back-pressured row transport for the query/result path.
//!
//! The lifecycle/control events of a query (started, result-set metadata,
//! messages, completion, cancellation) flow over the broadcast [`EventBus`],
//! which is fine because those are low-volume. The *rows themselves* are the
//! high-volume, unbounded part of a result set, and broadcasting them has two
//! problems for an unbounded stream:
//!
//! 1. A [`tokio::sync::broadcast`] ring silently drops the oldest items when a
//!    slow consumer falls behind (`recv` returns `Lagged`) — so a large result
//!    set is *under-counted* with no error.
//! 2. Broadcast has no producer-side blocking, so there is no real
//!    backpressure: the producer races ahead and the ring decides what to drop.
//!
//! [`RowChannel`] fixes both by using a **bounded [`tokio::sync::mpsc`]**.
//! When the buffer is full, [`RowSender::send`] *awaits* — the TDS read loop
//! (the producer) blocks until the consumer drains a slot. No row is ever
//! dropped; memory on the channel is capped at `capacity` batches regardless of
//! how many rows the server streams.
//!
//! [`EventBus`]: crate::event::EventBus

use crate::event::CellValue;
use crate::session::SessionId;
use tokio::sync::mpsc;

/// A contiguous chunk of rows for a single result set. Batching amortizes the
/// per-message overhead of the channel; the producer chooses the batch size.
#[derive(Debug, Clone, PartialEq)]
pub struct RowBatch {
    pub session_id: SessionId,
    pub result_set_index: usize,
    pub rows: Vec<Vec<CellValue>>,
}

impl RowBatch {
    pub fn new(session_id: SessionId, result_set_index: usize, rows: Vec<Vec<CellValue>>) -> Self {
        Self {
            session_id,
            result_set_index,
            rows,
        }
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

/// Producer half of the bounded row channel. Cloneable so multiple producers
/// can feed one consumer, though the common case is a single TDS read loop.
#[derive(Clone)]
pub struct RowSender {
    tx: mpsc::Sender<RowBatch>,
}

/// Error returned when the consumer half has been dropped before all rows were
/// delivered. The producer should treat this as "stop reading from the server".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowChannelClosed;

impl std::fmt::Display for RowChannelClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("row channel consumer was dropped")
    }
}

impl std::error::Error for RowChannelClosed {}

impl RowSender {
    /// Sends one batch, **awaiting (blocking the producer) when the channel is
    /// full**. This is the backpressure point: it guarantees the batch is
    /// enqueued (never dropped) unless the consumer has gone away.
    pub async fn send(&self, batch: RowBatch) -> Result<(), RowChannelClosed> {
        self.tx.send(batch).await.map_err(|_| RowChannelClosed)
    }

    /// Convenience for sending a single row.
    pub async fn send_row(
        &self,
        session_id: SessionId,
        result_set_index: usize,
        row: Vec<CellValue>,
    ) -> Result<(), RowChannelClosed> {
        self.send(RowBatch::new(session_id, result_set_index, vec![row]))
            .await
    }

    /// Current number of free permits before [`send`](Self::send) would block.
    /// Exposed for tests and instrumentation.
    pub fn capacity(&self) -> usize {
        self.tx.capacity()
    }
}

/// Consumer half of the bounded row channel.
pub struct RowReceiver {
    rx: mpsc::Receiver<RowBatch>,
}

impl RowReceiver {
    /// Receives the next batch, awaiting until one is available. Returns `None`
    /// once every [`RowSender`] has been dropped and the buffer is drained —
    /// the natural end-of-stream signal.
    pub async fn recv(&mut self) -> Option<RowBatch> {
        self.rx.recv().await
    }
}

/// Creates a bounded row channel. `capacity` is the number of *batches* the
/// buffer holds before the producer blocks; it must be >= 1.
pub fn row_channel(capacity: usize) -> (RowSender, RowReceiver) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (RowSender { tx }, RowReceiver { rx })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(i: usize) -> Vec<CellValue> {
        vec![CellValue::I64(i as i64), CellValue::String(format!("r{i}"))]
    }

    /// The defining property: with a slow consumer and a stream far larger than
    /// the channel capacity, **every row is delivered** (zero dropped). The
    /// producer blocks on `send` instead of overflowing a ring.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn slow_consumer_drops_zero_rows_past_capacity() {
        let total = 50_000usize;
        let capacity = 8usize; // tiny buffer relative to the stream.
        let (tx, mut rx) = row_channel(capacity);
        let session = SessionId::new();

        let producer = tokio::spawn(async move {
            for i in 0..total {
                tx.send_row(session.clone(), 0, sample_row(i))
                    .await
                    .expect("consumer alive");
            }
            // tx dropped here -> closes the channel.
        });

        let mut received = 0usize;
        while let Some(batch) = rx.recv().await {
            // Simulate a slow consumer to force the producer to block.
            if received.is_multiple_of(4096) {
                tokio::time::sleep(std::time::Duration::from_micros(20)).await;
            }
            received += batch.len();
        }

        producer.await.expect("producer joins");
        assert_eq!(received, total, "no rows may be dropped under backpressure");
    }

    /// `send` actually blocks (does not resolve) while the buffer is full and no
    /// one is receiving — proving real producer-side backpressure rather than a
    /// drop-and-continue ring.
    #[tokio::test]
    async fn send_blocks_when_buffer_full() {
        let (tx, mut rx) = row_channel(2);
        let session = SessionId::new();
        // Fill the buffer.
        tx.send_row(session.clone(), 0, sample_row(0))
            .await
            .unwrap();
        tx.send_row(session.clone(), 0, sample_row(1))
            .await
            .unwrap();

        // The third send must not complete until a slot frees up.
        let pending = tx.send_row(session.clone(), 0, sample_row(2));
        tokio::pin!(pending);
        let timed_out = tokio::time::timeout(std::time::Duration::from_millis(30), &mut pending)
            .await
            .is_err();
        assert!(timed_out, "send should block while the buffer is full");

        // Draining one frees a slot and unblocks the pending send.
        let first = rx.recv().await.expect("a batch is buffered");
        assert_eq!(first.rows[0], sample_row(0));
        pending.await.expect("send completes once a slot is free");
    }

    /// When the consumer is dropped, the producer learns via `RowChannelClosed`
    /// instead of hanging — so the TDS read loop can stop pulling from the
    /// server promptly on a cancelled/closed query.
    #[tokio::test]
    async fn send_reports_closed_when_consumer_dropped() {
        let (tx, rx) = row_channel(1);
        drop(rx);
        let err = tx
            .send_row(SessionId::new(), 0, sample_row(0))
            .await
            .unwrap_err();
        assert_eq!(err, RowChannelClosed);
    }

    /// End-of-stream: once all senders drop and the buffer drains, `recv`
    /// returns `None`.
    #[tokio::test]
    async fn recv_returns_none_after_all_senders_dropped() {
        let (tx, mut rx) = row_channel(4);
        let session = SessionId::new();
        tx.send_row(session, 0, sample_row(0)).await.unwrap();
        drop(tx);
        assert!(rx.recv().await.is_some());
        assert!(rx.recv().await.is_none());
    }
}
