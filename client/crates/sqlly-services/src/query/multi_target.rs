//! Fan-out orchestration: run one SQL statement on many connections with bounded
//! concurrency and remapped result-set indices.

use std::sync::{Arc, Mutex};

use futures::stream::{FuturesUnordered, StreamExt as _};
use sqlly_core::ConnectionProfile;
use tokio::sync::{Semaphore, mpsc};

use super::{QueryEvent, QueryExecutor};

/// One fan-out target: which profile, which executor (already routed by the
/// host per ADR-018), and a stable display name for result-set labels.
pub struct MultiTargetSpec {
    pub target_index: usize,
    pub display_name: String,
    pub profile: ConnectionProfile,
    pub executor: Arc<dyn QueryExecutor>,
    /// Warm-connection lane; `Some(format!("{tab_id}#{target_index}"))` from the app.
    pub session: Option<String>,
}

/// Result-set indices are remapped to `target_index * INDEX_STRIDE + local_index`
/// so every target's sets are unique inside one results view.
pub const INDEX_STRIDE: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOutcome {
    Completed { elapsed_ms: u128 },
    Failed { message: String },
    Cancelled,
}

#[derive(Debug)]
pub enum MultiTargetEvent {
    /// Executor accepted the query; `task_id` is what `cancel` needs.
    TargetStarted {
        target_index: usize,
        task_id: String,
    },
    /// A per-target stream event with `ResultSet`/`Rows`/`RowCount`/`RowsProgress`/
    /// `SpillWindow` indices already remapped by `INDEX_STRIDE`.
    Target {
        target_index: usize,
        event: QueryEvent,
    },
    TargetFinished {
        target_index: usize,
        outcome: TargetOutcome,
    },
    /// Terminal: every target finished. `any_failed`/`any_cancelled` summarize.
    AllFinished {
        any_failed: bool,
        any_cancelled: bool,
        max_elapsed_ms: u128,
    },
}

pub struct MultiTargetHandle {
    pub events: mpsc::Receiver<MultiTargetEvent>,
    /// `task_id` per target once known — the host cancels through this.
    pub task_ids: Arc<Mutex<Vec<Option<String>>>>,
}

fn remap_index(target_index: usize, local_index: usize) -> usize {
    target_index * INDEX_STRIDE + local_index
}

fn remap_event(target_index: usize, event: QueryEvent) -> QueryEvent {
    let map = |index: usize| remap_index(target_index, index);
    match event {
        QueryEvent::ResultSet { index, columns } => QueryEvent::ResultSet {
            index: map(index),
            columns,
        },
        QueryEvent::Rows { index, rows } => QueryEvent::Rows {
            index: map(index),
            rows,
        },
        QueryEvent::TruncatedCells { index, cells } => QueryEvent::TruncatedCells {
            index: map(index),
            cells,
        },
        QueryEvent::RowCount {
            index,
            rows_affected,
        } => QueryEvent::RowCount {
            index: map(index),
            rows_affected,
        },
        QueryEvent::RowsProgress { index, rows_seen } => QueryEvent::RowsProgress {
            index: map(index),
            rows_seen,
        },
        QueryEvent::SpillWindow {
            index,
            spill_file,
            rows_available,
        } => QueryEvent::SpillWindow {
            index: map(index),
            spill_file,
            rows_available,
        },
        QueryEvent::SpillManifest { mut manifest } => {
            for rs in &mut manifest.result_sets {
                rs.result_set_index = map(rs.result_set_index);
            }
            QueryEvent::SpillManifest { manifest }
        }
        other => other,
    }
}

/// Runs `sql` on every target with at most `max_concurrency` in flight.
/// Returns the handle immediately plus a driver future the caller must spawn.
pub fn run_multi_target(
    targets: Vec<MultiTargetSpec>,
    sql: String,
    row_buffer: usize,
    max_concurrency: usize,
) -> (
    MultiTargetHandle,
    impl std::future::Future<Output = ()> + Send,
) {
    let (tx, rx) = mpsc::channel::<MultiTargetEvent>(row_buffer.max(1));
    let task_ids = Arc::new(Mutex::new(vec![None; targets.len()]));
    let sem = Arc::new(Semaphore::new(max_concurrency.max(1)));
    let driver_task_ids = task_ids.clone();

    let driver = async move {
        let mut futures = FuturesUnordered::new();
        for spec in targets {
            let tx = tx.clone();
            let sem = sem.clone();
            let sql = sql.clone();
            let task_ids = driver_task_ids.clone();
            futures.push(async move {
                let _permit = match sem.acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => {
                        return TargetOutcome::Failed {
                            message: "concurrency gate closed".into(),
                        };
                    }
                };
                run_one_target(spec, sql, row_buffer, tx, task_ids).await
            });
        }

        let mut any_failed = false;
        let mut any_cancelled = false;
        let mut max_elapsed_ms = 0u128;

        while let Some(outcome) = futures.next().await {
            match &outcome {
                TargetOutcome::Completed { elapsed_ms } => {
                    max_elapsed_ms = max_elapsed_ms.max(*elapsed_ms);
                }
                TargetOutcome::Failed { .. } => any_failed = true,
                TargetOutcome::Cancelled => any_cancelled = true,
            }
        }

        let _ = tx
            .send(MultiTargetEvent::AllFinished {
                any_failed,
                any_cancelled,
                max_elapsed_ms,
            })
            .await;
    };

    (
        MultiTargetHandle {
            events: rx,
            task_ids,
        },
        driver,
    )
}

async fn run_one_target(
    spec: MultiTargetSpec,
    sql: String,
    row_buffer: usize,
    tx: mpsc::Sender<MultiTargetEvent>,
    task_ids: Arc<Mutex<Vec<Option<String>>>>,
) -> TargetOutcome {
    let target_index = spec.target_index;
    let handle = match spec
        .executor
        .execute_in_session(&spec.profile, spec.session.as_deref(), &sql, row_buffer)
        .await
    {
        Ok(handle) => handle,
        Err(e) => {
            let outcome = TargetOutcome::Failed {
                message: e.to_string(),
            };
            let _ = tx
                .send(MultiTargetEvent::TargetFinished {
                    target_index,
                    outcome: outcome.clone(),
                })
                .await;
            return outcome;
        }
    };

    {
        let mut ids = task_ids.lock().unwrap();
        if target_index < ids.len() {
            ids[target_index] = Some(handle.task_id.clone());
        }
    }

    if tx
        .send(MultiTargetEvent::TargetStarted {
            target_index,
            task_id: handle.task_id.clone(),
        })
        .await
        .is_err()
    {
        return TargetOutcome::Failed {
            message: "receiver dropped".into(),
        };
    }

    let mut outcome: Option<TargetOutcome> = None;
    let mut events = handle.events;
    while let Some(event) = events.recv().await {
        match &event {
            QueryEvent::Error { message } => {
                outcome = Some(TargetOutcome::Failed {
                    message: message.clone(),
                });
                let remapped = remap_event(target_index, event);
                if tx
                    .send(MultiTargetEvent::Target {
                        target_index,
                        event: remapped,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
                break;
            }
            QueryEvent::Completed { elapsed_ms } => {
                outcome = Some(TargetOutcome::Completed {
                    elapsed_ms: *elapsed_ms,
                });
                break;
            }
            QueryEvent::Cancelled => {
                outcome = Some(TargetOutcome::Cancelled);
                break;
            }
            _ => {
                let remapped = remap_event(target_index, event);
                if tx
                    .send(MultiTargetEvent::Target {
                        target_index,
                        event: remapped,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    let outcome = outcome.unwrap_or(TargetOutcome::Failed {
        message: "stream ended without a terminal event".into(),
    });
    let _ = tx
        .send(MultiTargetEvent::TargetFinished {
            target_index,
            outcome: outcome.clone(),
        })
        .await;
    outcome
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{
        MultiTargetEvent, MultiTargetSpec, QueryEvent, QueryExecutor, TargetOutcome,
        run_multi_target,
    };
    use crate::QueryHandle;
    use async_trait::async_trait;
    use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, SecretProvider, SecretRef};
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::sync::mpsc;

    #[derive(Clone)]
    enum Script {
        Rows(usize),
        Error(String),
        ConnectError(String),
        Cancel,
        BlockUntilReleased,
    }

    struct FakeQueries {
        scripts: HashMap<String, Script>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
        cancelled: Arc<Mutex<Vec<String>>>,
        release: Option<Arc<tokio::sync::Notify>>,
    }

    impl FakeQueries {
        fn new(scripts: HashMap<String, Script>) -> Arc<Self> {
            Arc::new(Self {
                scripts,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(Mutex::new(Vec::new())),
                release: None,
            })
        }

        fn with_release(
            scripts: HashMap<String, Script>,
            release: Arc<tokio::sync::Notify>,
        ) -> Arc<Self> {
            Arc::new(Self {
                scripts,
                in_flight: Arc::new(AtomicUsize::new(0)),
                max_in_flight: Arc::new(AtomicUsize::new(0)),
                cancelled: Arc::new(Mutex::new(Vec::new())),
                release: Some(release),
            })
        }

        fn enter(&self) {
            let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            let mut max = self.max_in_flight.load(Ordering::SeqCst);
            while n > max {
                match self.max_in_flight.compare_exchange_weak(
                    max,
                    n,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(current) => max = current,
                }
            }
        }

        fn leave(&self) {
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl QueryExecutor for FakeQueries {
        async fn execute(
            &self,
            profile: &ConnectionProfile,
            sql: &str,
            row_buffer: usize,
        ) -> sqlly_core::StudioResult<QueryHandle> {
            self.execute_in_session(profile, None, sql, row_buffer)
                .await
        }

        async fn execute_in_session(
            &self,
            profile: &ConnectionProfile,
            _session: Option<&str>,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<QueryHandle> {
            self.enter();
            let script = self
                .scripts
                .get(&profile.name)
                .cloned()
                .unwrap_or(Script::Rows(0));
            if let Script::ConnectError(msg) = script {
                self.leave();
                return Err(sqlly_core::StudioError::Connection(msg));
            }
            let (tx, rx) = mpsc::channel(16);
            let name = profile.name.clone();
            let task_id = format!("task-{}", name);
            if let Script::BlockUntilReleased = script {
                if let Some(notify) = &self.release {
                    notify.notified().await;
                }
                let _ = tx
                    .send(QueryEvent::ResultSet {
                        index: 0,
                        columns: vec![ResultColumn {
                            name: "n".into(),
                            data_type: "int".into(),
                            nullable: false,
                        }],
                    })
                    .await;
                let _ = tx.send(QueryEvent::Completed { elapsed_ms: 5 }).await;
                self.leave();
                return Ok(QueryHandle {
                    task_id,
                    events: rx,
                });
            }
            let scripts = self.scripts.clone();
            let in_flight = self.in_flight.clone();
            let max_in_flight = self.max_in_flight.clone();
            tokio::spawn(async move {
                let script = scripts.get(&name).cloned().unwrap_or(Script::Rows(0));
                match script {
                    Script::ConnectError(_) | Script::BlockUntilReleased => {}
                    Script::Rows(n) => {
                        let _ = tx
                            .send(QueryEvent::ResultSet {
                                index: 0,
                                columns: vec![ResultColumn {
                                    name: "n".into(),
                                    data_type: "int".into(),
                                    nullable: false,
                                }],
                            })
                            .await;
                        if n > 0 {
                            let rows: Vec<_> =
                                (0..n).map(|i| vec![CellValue::I64(i as i64)]).collect();
                            let _ = tx.send(QueryEvent::Rows { index: 0, rows }).await;
                        }
                        let _ = tx.send(QueryEvent::Completed { elapsed_ms: 5 }).await;
                    }
                    Script::Error(msg) => {
                        let _ = tx
                            .send(QueryEvent::Error {
                                message: msg.clone(),
                            })
                            .await;
                    }
                    Script::Cancel => {
                        let _ = tx.send(QueryEvent::Cancelled).await;
                    }
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                let _ = max_in_flight;
            });
            Ok(QueryHandle {
                task_id,
                events: rx,
            })
        }

        async fn cancel(&self, task_id: &str) -> sqlly_core::StudioResult<()> {
            self.cancelled.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
    }

    fn profile(name: &str) -> ConnectionProfile {
        ConnectionProfile::sql_login(
            name,
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "k".into(),
            },
        )
    }

    fn spec(index: usize, name: &str, executor: Arc<dyn QueryExecutor>) -> MultiTargetSpec {
        let profile = profile(name);
        MultiTargetSpec {
            target_index: index,
            display_name: name.to_string(),
            profile,
            executor,
            session: None,
        }
    }

    async fn drain(mut rx: mpsc::Receiver<MultiTargetEvent>) -> Vec<MultiTargetEvent> {
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            out.push(ev);
        }
        out
    }

    #[tokio::test]
    async fn three_targets_yield_three_named_result_sets_with_remapped_indices() {
        let fake = FakeQueries::new(HashMap::from([
            ("a".into(), Script::Rows(1)),
            ("b".into(), Script::Rows(1)),
            ("c".into(), Script::Rows(1)),
        ]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let targets = vec![
            spec(0, "a", exec.clone()),
            spec(1, "b", exec.clone()),
            spec(2, "c", exec.clone()),
        ];
        let (handle, driver) = run_multi_target(targets, "SELECT 1".into(), 8, 4);
        tokio::spawn(driver);
        let events = drain(handle.events).await;
        let indices: Vec<usize> = events
            .iter()
            .filter_map(|ev| match ev {
                MultiTargetEvent::Target {
                    event: QueryEvent::ResultSet { index, .. },
                    ..
                } => Some(*index),
                _ => None,
            })
            .collect();
        assert_eq!(indices, vec![0, 1000, 2000]);
        assert!(events.iter().any(|ev| matches!(
            ev,
            MultiTargetEvent::AllFinished {
                any_failed: false,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn one_target_connect_error_does_not_stop_others() {
        let fake = FakeQueries::new(HashMap::from([
            ("a".into(), Script::Rows(1)),
            ("b".into(), Script::ConnectError("nope".into())),
            ("c".into(), Script::Rows(1)),
        ]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let targets = vec![
            spec(0, "a", exec.clone()),
            spec(1, "b", exec.clone()),
            spec(2, "c", exec.clone()),
        ];
        let (handle, driver) = run_multi_target(targets, "SELECT 1".into(), 8, 4);
        tokio::spawn(driver);
        let events = drain(handle.events).await;
        let finished: Vec<_> = events
            .iter()
            .filter_map(|ev| match ev {
                MultiTargetEvent::TargetFinished {
                    target_index,
                    outcome,
                } => Some((*target_index, outcome.clone())),
                _ => None,
            })
            .collect();
        assert!(
            finished
                .iter()
                .any(|(i, o)| *i == 1 && matches!(o, TargetOutcome::Failed { .. }))
        );
        assert!(
            finished
                .iter()
                .any(|(i, o)| *i == 0 && matches!(o, TargetOutcome::Completed { .. }))
        );
        assert!(
            finished
                .iter()
                .any(|(i, o)| *i == 2 && matches!(o, TargetOutcome::Completed { .. }))
        );
        assert!(events.iter().any(|ev| matches!(
            ev,
            MultiTargetEvent::AllFinished {
                any_failed: true,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn mid_stream_error_is_forwarded_then_finished_failed() {
        let fake = FakeQueries::new(HashMap::from([("a".into(), Script::Error("boom".into()))]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let (handle, driver) = run_multi_target(vec![spec(0, "a", exec)], "SELECT 1".into(), 8, 1);
        tokio::spawn(driver);
        let events = drain(handle.events).await;
        let err_pos = events.iter().position(|ev| {
            matches!(
                ev,
                MultiTargetEvent::Target {
                    event: QueryEvent::Error { .. },
                    ..
                }
            )
        });
        let fin_pos = events.iter().position(|ev| {
            matches!(
                ev,
                MultiTargetEvent::TargetFinished {
                    outcome: TargetOutcome::Failed { .. },
                    ..
                }
            )
        });
        assert!(err_pos.is_some() && fin_pos.is_some());
        assert!(err_pos.unwrap() < fin_pos.unwrap());
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let release = Arc::new(tokio::sync::Notify::new());
        let mut scripts = HashMap::new();
        for i in 0..6 {
            scripts.insert(format!("t{i}"), Script::BlockUntilReleased);
        }
        let fake = FakeQueries::with_release(scripts, release.clone());
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let targets: Vec<_> = (0..6)
            .map(|i| spec(i, &format!("t{i}"), exec.clone()))
            .collect();
        let (handle, driver) = run_multi_target(targets, "SELECT 1".into(), 8, 2);
        tokio::spawn(driver);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(fake.max_in_flight.load(Ordering::SeqCst) <= 2);
        for _ in 0..6 {
            release.notify_one();
        }
        drop(handle.events);
    }

    #[tokio::test]
    async fn task_ids_are_recorded_per_target() {
        let fake = FakeQueries::new(HashMap::from([
            ("a".into(), Script::Rows(0)),
            ("b".into(), Script::ConnectError("nope".into())),
            ("c".into(), Script::Rows(0)),
        ]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let targets = vec![
            spec(0, "a", exec.clone()),
            spec(1, "b", exec.clone()),
            spec(2, "c", exec.clone()),
        ];
        let (handle, driver) = run_multi_target(targets, "SELECT 1".into(), 8, 4);
        let task_ids = handle.task_ids.clone();
        tokio::spawn(driver);
        let _ = drain(handle.events).await;
        let ids = task_ids.lock().unwrap();
        assert!(ids[0].as_ref().is_some_and(|s| s.starts_with("task-")));
        assert!(ids[1].is_none());
        assert!(ids[2].as_ref().is_some_and(|s| s.starts_with("task-")));
    }

    #[tokio::test]
    async fn cancelled_target_reports_cancelled_and_all_finished_flags_it() {
        let fake = FakeQueries::new(HashMap::from([("a".into(), Script::Cancel)]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let (handle, driver) = run_multi_target(vec![spec(0, "a", exec)], "SELECT 1".into(), 8, 1);
        tokio::spawn(driver);
        let events = drain(handle.events).await;
        assert!(events.iter().any(|ev| {
            matches!(
                ev,
                MultiTargetEvent::TargetFinished {
                    outcome: TargetOutcome::Cancelled,
                    ..
                }
            )
        }));
        assert!(events.iter().any(|ev| matches!(
            ev,
            MultiTargetEvent::AllFinished {
                any_cancelled: true,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn receiver_drop_ends_driver() {
        let fake = FakeQueries::new(HashMap::from([("a".into(), Script::Rows(0))]));
        let exec: Arc<dyn QueryExecutor> = fake.clone();
        let (handle, driver) = run_multi_target(vec![spec(0, "a", exec)], "SELECT 1".into(), 8, 1);
        let driver = tokio::spawn(driver);
        drop(handle.events);
        tokio::time::timeout(std::time::Duration::from_secs(2), driver)
            .await
            .expect("driver should finish when receiver drops")
            .expect("driver join");
    }
}
