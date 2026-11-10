//! Connection pooling and concurrency discipline (ADR-011 §4.1–4.2).
//!
//! Every database read flows through a [`ConnectionPool`]: pool max size
//! equals the profile's `db_concurrency`, so the in-flight request limit is
//! structural — request N+1 waits for a pooled connection rather than opening
//! a new one. No engine code path may construct a raw connection.

use async_trait::async_trait;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Semaphore;

pub type PoolError = String;

/// A single pooled connection. Implementations wrap a driver connection
/// (Tiberius, tokio-postgres, rusqlite-on-blocking, …).
#[async_trait]
pub trait PooledConnection: Send {
    /// Run a read-only query with positional string parameters, returning
    /// rows of optional strings. Collection and value-mode completion only
    /// need this shape; richer typed access stays in the driver crates.
    async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError>;
}

/// Factory for pooled connections — one per connection profile.
#[async_trait]
pub trait PooledDriver: Send + Sync {
    async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError>;
}

/// Bounded async connection pool. `max_size` doubles as the concurrency cap:
/// acquiring waits on a semaphore whose permit count equals pool capacity.
pub struct ConnectionPool {
    driver: Arc<dyn PooledDriver>,
    permits: Arc<Semaphore>,
    idle: Mutex<VecDeque<Box<dyn PooledConnection>>>,
    max_size: usize,
    /// Telemetry for tests and `gain`-style introspection.
    in_flight: AtomicUsize,
    high_water: AtomicUsize,
}

impl ConnectionPool {
    pub fn new(driver: Arc<dyn PooledDriver>, max_size: usize) -> Arc<Self> {
        Arc::new(Self {
            driver,
            permits: Arc::new(Semaphore::new(max_size.max(1))),
            idle: Mutex::new(VecDeque::new()),
            max_size: max_size.max(1),
            in_flight: AtomicUsize::new(0),
            high_water: AtomicUsize::new(0),
        })
    }

    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Highest number of simultaneously checked-out connections observed.
    pub fn high_water_mark(&self) -> usize {
        self.high_water.load(Ordering::SeqCst)
    }

    /// Check out a connection, waiting if the pool is exhausted.
    pub async fn acquire(self: &Arc<Self>) -> Result<PooledGuard, PoolError> {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| "pool closed".to_string())?;
        let existing = self.idle.lock().pop_front();
        let conn = match existing {
            Some(c) => c,
            None => self.driver.connect().await?,
        };
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.high_water.fetch_max(now, Ordering::SeqCst);
        Ok(PooledGuard {
            pool: Arc::clone(self),
            conn: Some(conn),
            _permit: permit,
        })
    }

    /// Return a connection to the idle list. Synchronous (the idle lock is
    /// held only for the push, never across an await) so a dropped guard's
    /// connection is available to the very next `acquire` with no task-spawn
    /// latency gap.
    fn give_back(&self, conn: Box<dyn PooledConnection>) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        let mut idle = self.idle.lock();
        if idle.len() < self.max_size {
            idle.push_back(conn);
        }
        // else: drop the connection (shrinks naturally after a cap decrease)
    }
}

/// RAII checkout. Returns the connection to the pool on drop.
pub struct PooledGuard {
    pool: Arc<ConnectionPool>,
    conn: Option<Box<dyn PooledConnection>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl PooledGuard {
    pub async fn query(
        &mut self,
        sql: &str,
        params: &[String],
    ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
        self.conn
            .as_mut()
            .expect("connection present until drop")
            .query(sql, params)
            .await
    }
}

impl Drop for PooledGuard {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Synchronous return: the connection is idle again BEFORE the
            // permit releases, so an immediately following `acquire` finds it
            // instead of dialing a fresh connection.
            debug_assert!(self.pool.in_flight.load(Ordering::SeqCst) > 0);
            self.pool.give_back(conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Mock driver that records concurrent query count.
    pub struct MockDriver {
        pub active: Arc<AtomicUsize>,
        pub peak: Arc<AtomicUsize>,
        pub delay: Duration,
        pub connects: Arc<AtomicUsize>,
    }

    struct MockConn {
        active: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        delay: Duration,
    }

    #[async_trait]
    impl PooledConnection for MockConn {
        async fn query(
            &mut self,
            _sql: &str,
            _params: &[String],
        ) -> Result<Vec<Vec<Option<String>>>, PoolError> {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(vec![])
        }
    }

    #[async_trait]
    impl PooledDriver for MockDriver {
        async fn connect(&self) -> Result<Box<dyn PooledConnection>, PoolError> {
            self.connects.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(MockConn {
                active: Arc::clone(&self.active),
                peak: Arc::clone(&self.peak),
                delay: self.delay,
            }))
        }
    }

    #[tokio::test]
    async fn pool_never_exceeds_cap_and_queues() {
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let connects = Arc::new(AtomicUsize::new(0));
        let driver = Arc::new(MockDriver {
            active: Arc::clone(&active),
            peak: Arc::clone(&peak),
            delay: Duration::from_millis(20),
            connects: Arc::clone(&connects),
        });
        let cap = 3usize;
        let pool = ConnectionPool::new(driver, cap);

        // 12 concurrent requests against a cap of 3: all succeed, peak ≤ 3.
        let mut handles = Vec::new();
        for _ in 0..12 {
            let pool = Arc::clone(&pool);
            handles.push(tokio::spawn(async move {
                let mut conn = pool.acquire().await.expect("acquire");
                conn.query("select 1", &[]).await.expect("query");
            }));
        }
        for h in handles {
            h.await.expect("task");
        }
        assert!(
            peak.load(Ordering::SeqCst) <= cap,
            "peak {} > cap {}",
            peak.load(Ordering::SeqCst),
            cap
        );
        assert!(pool.high_water_mark() <= cap);
        // Pool reuses connections rather than opening one per request.
        assert!(
            connects.load(Ordering::SeqCst) <= cap,
            "opened {} conns",
            connects.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn pool_reuses_idle_connections() {
        let driver = Arc::new(MockDriver {
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            delay: Duration::from_millis(1),
            connects: Arc::new(AtomicUsize::new(0)),
        });
        let connects = Arc::clone(&driver.connects);
        let pool = ConnectionPool::new(driver, 2);
        for _ in 0..5 {
            let mut c = pool.acquire().await.unwrap();
            c.query("select 1", &[]).await.unwrap();
            // Synchronous give-back: the immediately following acquire must
            // find the idle connection with no yield or sleep in between.
            drop(c);
        }
        assert_eq!(
            connects.load(Ordering::SeqCst),
            1,
            "sequential use should reuse one connection"
        );
    }
}
