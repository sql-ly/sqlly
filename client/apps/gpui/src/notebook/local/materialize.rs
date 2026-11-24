//! Copying results into a notebook's local data. Three sources: a cell
//! output already in memory (complete and uncut), a connection query run
//! again with full values (when the output on screen is partial, spilled or
//! capped), and a local query over relations already there (a local cell
//! read by another local cell). Every copy carries its provenance; rows
//! stream into the workspace in bounded batches with live progress and a
//! cancel that also stops the server query.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(test)]
use sqlly_core::ResultColumn;
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, DisplayMessage, StoredMessage};
use sqlly_services::local_analytics::{
    CancelToken, LocalStoreError, MaterializeOutcome, NotebookWorkspace, SnapshotRows, SourceInfo,
    SourceKind, WriteMode, error_message,
};
use sqlly_services::notebook::CellOutput;
use sqlly_services::{QueryEvent, QueryExecutor};

/// Rows per batch handed to the workspace from a query.
const QUERY_BATCH_ROWS: usize = 2048;
/// Batches buffered between the query and the workspace.
const QUERY_BUFFER_BATCHES: usize = 4;
const ROW_BUFFER: usize = 256;

type Batch = Result<Vec<Vec<CellValue>>, String>;

/// Copies run on the tokio runtime, where the UI language is not visible:
/// failures carry key and arguments and are worded after the await.
pub type Failure = DisplayMessage;

fn store_error(error: &LocalStoreError) -> Failure {
    error_message(error)
}

fn canceled() -> Failure {
    crate::localization::deferred_text("notebook.local.materialize.canceled")
}

/// A catalog message saved in provenance as key and arguments (never
/// translated text), worded when shown by [`super::partial_text`].
pub fn stored_detail(message: &DisplayMessage) -> String {
    match message.key() {
        Some(key) => serde_json::to_string(&StoredMessage {
            key: key.to_string(),
            arguments: message
                .arguments()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect(),
        })
        .unwrap_or_else(|_| message.fallback().to_string()),
        None => message.fallback().to_string(),
    }
}

/// Rows copied so far, read by the activity panel while a copy runs.
#[derive(Debug, Default)]
pub struct Progress(AtomicU64);

impl Progress {
    pub fn rows(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }

    fn add(&self, rows: usize) {
        self.0.fetch_add(rows as u64, Ordering::Relaxed);
    }
}

/// Copy an in-memory output. The caller decides it is whole; anything else
/// arrives here with `source.source_incomplete` set.
pub async fn from_snapshot(
    ws: &NotebookWorkspace,
    relation: &str,
    output: &CellOutput,
    source: SourceInfo,
    cancel: CancelToken,
    progress: Arc<Progress>,
) -> Result<MaterializeOutcome, Failure> {
    let rows = Arc::clone(&output.rows);
    let count = rows.len();
    let outcome = ws
        .materialize(
            relation,
            output.columns.clone(),
            SnapshotRows::new(rows),
            source,
            WriteMode::Replace,
            cancel,
        )
        .await
        .map_err(|e| store_error(&e))?;
    progress.add(count);
    Ok(outcome)
}

/// Run `sql` on `profile` with full values and stream its first result set
/// into `relation`. Dropping out early (a quota, a cancel) cancels the query.
#[allow(clippy::too_many_arguments)]
pub async fn from_query(
    ws: &NotebookWorkspace,
    executor: Arc<dyn QueryExecutor>,
    profile: ConnectionProfile,
    lane: Option<String>,
    sql: String,
    relation: &str,
    source: SourceInfo,
    cancel: CancelToken,
    progress: Arc<Progress>,
) -> Result<MaterializeOutcome, Failure> {
    let handle = executor
        .execute_full_values(&profile, lane.as_deref(), &sql, ROW_BUFFER)
        .await
        .map_err(|e| e.message())?;
    let task_id = handle.task_id.clone();
    let mut events = handle.events;
    let columns = loop {
        if cancel.is_canceled() {
            let _ = executor.cancel(&task_id).await;
            return Err(canceled());
        }
        match events.recv().await {
            Some(QueryEvent::ResultSet { index: 0, columns }) => break columns,
            Some(QueryEvent::Error { message }) => return Err(DisplayMessage::raw(message)),
            Some(QueryEvent::Cancelled) => return Err(canceled()),
            Some(QueryEvent::Completed { .. }) | None => {
                return Err(crate::localization::deferred_text(
                    "notebook.local.materialize.no_rows",
                ));
            }
            Some(_) => {}
        }
    };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Batch>(QUERY_BUFFER_BATCHES);
    let batches = std::iter::from_fn(move || rx.blocking_recv());
    let ingest = ws.materialize(
        relation,
        columns,
        batches,
        source,
        WriteMode::Replace,
        cancel.clone(),
    );
    let pump_progress = Arc::clone(&progress);
    let pump = async move {
        let mut pending: Vec<Vec<CellValue>> = Vec::new();
        let mut stopped = false;
        let mut server_canceled = false;
        loop {
            let event = tokio::select! {
                event = events.recv() => event,
                () = tx.closed() => { stopped = true; break; }
            };
            match event {
                Some(QueryEvent::Rows { index: 0, rows }) => {
                    pump_progress.add(rows.len());
                    pending.extend(rows);
                    if pending.len() >= QUERY_BATCH_ROWS
                        && tx.send(Ok(std::mem::take(&mut pending))).await.is_err()
                    {
                        stopped = true;
                        break;
                    }
                }
                Some(QueryEvent::Error { message }) => {
                    let _ = tx.send(Err(message)).await;
                    break;
                }
                Some(QueryEvent::Cancelled) => {
                    server_canceled = true;
                    let _ = tx.send(Err(canceled().into_fallback())).await;
                    break;
                }
                Some(QueryEvent::Completed { .. }) | None => {
                    if !pending.is_empty() {
                        let _ = tx.send(Ok(std::mem::take(&mut pending))).await;
                    }
                    break;
                }
                Some(_) => {}
            }
        }
        drop(tx);
        (stopped, server_canceled)
    };
    let (outcome, (stopped, server_canceled)) = tokio::join!(ingest, pump);
    if stopped {
        let _ = executor.cancel(&task_id).await;
    }
    outcome.map_err(|e| {
        if server_canceled {
            canceled()
        } else {
            store_error(&e)
        }
    })
}

/// Copy the result of a local query (another local cell's SQL, its
/// references already replaced by relation names) into `relation`.
pub async fn from_local_query(
    ws: &NotebookWorkspace,
    sql: String,
    relation: &str,
    mut source: SourceInfo,
    cancel: CancelToken,
    progress: Arc<Progress>,
) -> Result<MaterializeOutcome, Failure> {
    let mut sets = ws
        .query(sql, None, cancel.clone())
        .await
        .map_err(|e| store_error(&e))?;
    if sets.is_empty() {
        return Err(crate::localization::deferred_text(
            "notebook.local.materialize.no_rows",
        ));
    }
    let set = sets.remove(0);
    if set.truncated && source.source_incomplete.is_none() {
        source.source_incomplete = Some(stored_detail(&crate::localization::deferred(
            "notebook.local.materialize.local_query_capped",
            &[("count", set.rows.len())],
        )));
    }
    let output = CellOutput {
        columns: set.columns,
        rows: Arc::new(set.rows),
        completeness: sqlly_services::notebook::Completeness::Complete,
        row_count: 0,
        elapsed_ms: 0,
        run_id: String::new(),
        fingerprint: sqlly_services::notebook::Fingerprint(String::new()),
        provenance: Default::default(),
        freshness: Default::default(),
    };
    from_snapshot(ws, relation, &output, source, cancel, progress).await
}

/// Provenance for a local cell's derived relation.
pub fn local_source(cell_id: &str, run_id: Option<String>, upstream: Vec<String>) -> SourceInfo {
    let mut source = SourceInfo::query(DatabaseType::DuckDb, super::LOCAL_LABEL_KEY);
    source.kind = SourceKind::Local;
    source.cell_id = Some(cell_id.to_string());
    source.run_id = run_id;
    source.upstream = upstream;
    source
}

/// Columns of a result set, for tests and fakes.
#[cfg(test)]
pub(crate) fn columns(names: &[(&str, &str)]) -> Vec<ResultColumn> {
    names
        .iter()
        .map(|(name, ty)| ResultColumn {
            name: name.to_string(),
            data_type: ty.to_string(),
            nullable: true,
        })
        .collect()
}

/// A query service that plays scripted events and records cancels.
#[cfg(test)]
pub(crate) mod fake {
    use super::*;
    use sqlly_services::QueryHandle;
    use std::sync::Mutex;

    /// Plays scripted events, records cancels.
    pub(crate) struct ScriptedExecutor {
        pub(crate) events: Mutex<Option<Vec<QueryEvent>>>,
        pub(crate) canceled: Mutex<Vec<String>>,
    }

    impl ScriptedExecutor {
        pub(crate) fn new(events: Vec<QueryEvent>) -> Arc<Self> {
            Arc::new(Self {
                events: Mutex::new(Some(events)),
                canceled: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl QueryExecutor for ScriptedExecutor {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<QueryHandle> {
            let events = self.events.lock().unwrap().take().unwrap_or_default();
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            tokio::spawn(async move {
                for event in events {
                    if tx.send(event).await.is_err() {
                        break;
                    }
                }
            });
            Ok(QueryHandle {
                task_id: "task-1".into(),
                events: rx,
            })
        }

        async fn cancel(&self, task_id: &str) -> sqlly_core::StudioResult<()> {
            self.canceled.lock().unwrap().push(task_id.to_string());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::ScriptedExecutor;
    use super::*;
    use sqlly_services::local_analytics::{
        LocalAnalytics, LocalAnalyticsConfig, OpenOptions, PartialReason,
    };

    fn rows(from: i64, n: i64) -> Vec<Vec<CellValue>> {
        (from..from + n)
            .map(|i| vec![CellValue::I64(i), CellValue::String(format!("r{i}"))])
            .collect()
    }

    async fn workspace(
        dir: &std::path::Path,
        max_rows: u64,
    ) -> (LocalAnalytics, NotebookWorkspace) {
        let mut config = LocalAnalyticsConfig::new(dir);
        config.quota.max_rows_per_relation = max_rows;
        let analytics = LocalAnalytics::new(config);
        let ws = analytics
            .open("nb_materialize", OpenOptions::default())
            .await
            .unwrap();
        (analytics, ws)
    }

    fn profile(name: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            name,
            "db.example",
            "reader",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "hunter2-secret".into(),
            },
        );
        p.environment = sqlly_core::ConnectionEnvironment::Production;
        p.database = Some("Sales".into());
        p
    }

    fn query_events(total: i64) -> Vec<QueryEvent> {
        let mut events = vec![
            QueryEvent::Started,
            QueryEvent::ResultSet {
                index: 0,
                columns: columns(&[("id", "int"), ("name", "nvarchar")]),
            },
        ];
        let mut at = 0;
        while at < total {
            let n = (total - at).min(500);
            events.push(QueryEvent::Rows {
                index: 0,
                rows: rows(at, n),
            });
            at += n;
        }
        events.push(QueryEvent::Completed { elapsed_ms: 5 });
        events
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_streams_every_row_with_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000_000).await;
        let exec = ScriptedExecutor::new(query_events(5_000));
        let source_profile = profile("Sales (prod)");
        let mut source = SourceInfo::from_profile(&source_profile);
        source.cell_id = Some("orders".into());
        source.sql_hash = Some(sqlly_services::local_analytics::source_sql_hash("SELECT 1"));
        let progress = Arc::new(Progress::default());
        let outcome = from_query(
            &ws,
            exec.clone(),
            source_profile.clone(),
            None,
            "SELECT id, name FROM orders".into(),
            "orders",
            source,
            CancelToken::new(),
            Arc::clone(&progress),
        )
        .await
        .unwrap();
        assert_eq!(outcome.row_count, 5_000);
        assert!(outcome.is_complete());
        assert_eq!(progress.rows(), 5_000);
        assert!(exec.canceled.lock().unwrap().is_empty());
        let record = ws.provenance("orders").await.unwrap().unwrap();
        assert_eq!(record.source_label, "Sales (prod)");
        assert_eq!(record.environment.as_deref(), Some("PRODUCTION"));
        assert_eq!(record.database.as_deref(), Some("Sales"));
        assert_eq!(record.cell_id.as_deref(), Some("orders"));
        assert!(record.complete);
        let text = serde_json::to_string(&record).unwrap();
        assert!(!text.contains("hunter2"), "no secret in provenance");
        assert!(!text.contains("reader"), "no user name in provenance");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_row_quota_keeps_a_partial_relation_and_stops_the_query() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000).await;
        let exec = ScriptedExecutor::new(query_events(20_000));
        let progress = Arc::new(Progress::default());
        let outcome = from_query(
            &ws,
            exec.clone(),
            profile("Warehouse"),
            None,
            "SELECT * FROM big".into(),
            "big",
            SourceInfo::from_profile(&profile("Warehouse")),
            CancelToken::new(),
            progress,
        )
        .await
        .unwrap();
        assert_eq!(outcome.row_count, 1_000);
        assert_eq!(
            outcome.partial,
            Some(PartialReason::RowLimit { limit: 1_000 })
        );
        let record = ws.provenance("big").await.unwrap().unwrap();
        assert!(!record.complete);
        assert_eq!(exec.canceled.lock().unwrap().as_slice(), ["task-1"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_errors_and_empty_results_leave_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000).await;
        let failing = ScriptedExecutor::new(vec![
            QueryEvent::ResultSet {
                index: 0,
                columns: columns(&[("id", "int"), ("name", "nvarchar")]),
            },
            QueryEvent::Rows {
                index: 0,
                rows: rows(0, 10),
            },
            QueryEvent::Error {
                message: "deadlock victim".into(),
            },
        ]);
        let err = from_query(
            &ws,
            failing,
            profile("A"),
            None,
            "SELECT 1".into(),
            "failed",
            SourceInfo::from_profile(&profile("A")),
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap_err();
        assert!(err.fallback().contains("deadlock"), "{err}");
        assert!(ws.provenance("failed").await.unwrap().is_none());
        let empty = ScriptedExecutor::new(vec![QueryEvent::Completed { elapsed_ms: 1 }]);
        assert!(
            from_query(
                &ws,
                empty,
                profile("A"),
                None,
                "UPDATE x SET y = 1".into(),
                "none",
                SourceInfo::from_profile(&profile("A")),
                CancelToken::new(),
                Arc::new(Progress::default()),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_connections_join_locally_and_lineage_names_both() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000_000).await;
        let mut sql_server = profile("Orders (SQL Server)");
        sql_server.database_type = DatabaseType::SqlServer;
        let mut postgres = profile("CRM (Postgres)");
        postgres.database_type = DatabaseType::Postgres;
        postgres.environment = sqlly_core::ConnectionEnvironment::Dev;
        let orders = CellOutput {
            columns: columns(&[("customer_id", "int"), ("total", "decimal(10,2)")]),
            rows: Arc::new(vec![
                vec![CellValue::I64(1), CellValue::String("10.50".into())],
                vec![CellValue::I64(2), CellValue::String("7.25".into())],
            ]),
            completeness: sqlly_services::notebook::Completeness::Complete,
            row_count: 2,
            elapsed_ms: 0,
            run_id: "run_o".into(),
            fingerprint: sqlly_services::notebook::Fingerprint(String::new()),
            provenance: Default::default(),
            freshness: Default::default(),
        };
        let mut o_source = SourceInfo::from_profile(&sql_server);
        o_source.cell_id = Some("orders".into());
        o_source.extracted_at = Some("2026-11-18T09:00:00Z".into());
        from_snapshot(
            &ws,
            "orders",
            &orders,
            o_source,
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap();
        let exec = ScriptedExecutor::new(vec![
            QueryEvent::ResultSet {
                index: 0,
                columns: columns(&[("id", "int4"), ("name", "text")]),
            },
            QueryEvent::Rows {
                index: 0,
                rows: vec![
                    vec![CellValue::I64(1), CellValue::String("Ada".into())],
                    vec![CellValue::I64(2), CellValue::String("Lin".into())],
                ],
            },
            QueryEvent::Completed { elapsed_ms: 1 },
        ]);
        let mut c_source = SourceInfo::from_profile(&postgres);
        c_source.cell_id = Some("customers".into());
        c_source.source_incomplete = Some("capped at 2 rows".into());
        from_query(
            &ws,
            exec,
            postgres.clone(),
            None,
            "SELECT id, name FROM customers".into(),
            "customers",
            c_source,
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap();
        let joined = from_local_query(
            &ws,
            "SELECT c.name, o.total FROM orders o JOIN customers c ON c.id = o.customer_id ORDER BY c.name".into(),
            "revenue",
            local_source("revenue", Some("run_r".into()), vec!["orders".into(), "customers".into()]),
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap();
        assert_eq!(joined.row_count, 2);
        let lineage = ws.lineage("revenue").await.unwrap();
        let labels: Vec<&str> = lineage.iter().map(|r| r.source_label.as_str()).collect();
        assert!(labels.contains(&"Orders (SQL Server)"), "{labels:?}");
        assert!(labels.contains(&"CRM (Postgres)"), "{labels:?}");
        let crm = lineage.iter().find(|r| r.relation == "customers").unwrap();
        assert!(!crm.complete, "the partial extract is reported");
        let orders_record = lineage.iter().find(|r| r.relation == "orders").unwrap();
        assert!(
            orders_record
                .extracted_at
                .starts_with("2026-11-18T09:00:00")
        );
        assert_eq!(orders_record.environment.as_deref(), Some("PRODUCTION"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn copy_failures_carry_keys_for_the_ui_thread_to_word() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000).await;
        let empty = ScriptedExecutor::new(vec![QueryEvent::Completed { elapsed_ms: 1 }]);
        let err = from_query(
            &ws,
            empty,
            profile("A"),
            None,
            "SELECT 1".into(),
            "none",
            SourceInfo::from_profile(&profile("A")),
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.key(), Some("notebook.local.materialize.no_rows"));
        let canceled = CancelToken::new();
        canceled.cancel();
        let err = from_query(
            &ws,
            ScriptedExecutor::new(query_events(10)),
            profile("A"),
            None,
            "SELECT 1".into(),
            "stopped",
            SourceInfo::from_profile(&profile("A")),
            canceled,
            Arc::new(Progress::default()),
        )
        .await
        .unwrap_err();
        assert_eq!(err.key(), Some("notebook.local.materialize.canceled"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn provenance_keeps_keys_and_is_worded_in_the_language_shown() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000).await;
        let output = CellOutput {
            columns: columns(&[("id", "int")]),
            rows: Arc::new(vec![vec![CellValue::I64(1)]]),
            completeness: sqlly_services::notebook::Completeness::Complete,
            row_count: 1,
            elapsed_ms: 0,
            run_id: "run_l".into(),
            fingerprint: sqlly_services::notebook::Fingerprint(String::new()),
            provenance: Default::default(),
            freshness: Default::default(),
        };
        let mut source = local_source("derived", None, vec!["orders".into()]);
        source.source_incomplete = Some(stored_detail(&crate::localization::deferred(
            "notebook.local.materialize.local_query_capped",
            &[("count", 5)],
        )));
        from_snapshot(
            &ws,
            "derived",
            &output,
            source,
            CancelToken::new(),
            Arc::new(Progress::default()),
        )
        .await
        .unwrap();
        let record = ws.provenance("derived").await.unwrap().unwrap();
        assert_eq!(record.source_label, super::super::LOCAL_LABEL_KEY);
        let saved = serde_json::to_string(&record).unwrap();
        assert!(
            !saved.contains("only the first"),
            "no translated text is saved: {saved}"
        );
        crate::localization::activate(crate::localization::Language::Spanish, None).unwrap();
        let view = super::super::provenance_view(&record);
        let spanish_label = crate::localization::text(super::super::LOCAL_LABEL_KEY);
        let spanish_detail = crate::localization::format(
            "notebook.local.materialize.local_query_capped",
            &[("count", 5)],
        );
        crate::localization::activate(crate::localization::Language::English, None).unwrap();
        assert_eq!(view.label, spanish_label);
        let partial = view.partial.unwrap();
        assert!(partial.contains(&spanish_detail), "{partial}");
        assert!(!partial.contains("only the first"), "{partial}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cancel_stops_the_copy() {
        let dir = tempfile::tempdir().unwrap();
        let (_a, ws) = workspace(dir.path(), 1_000_000).await;
        let exec = ScriptedExecutor::new(query_events(10_000));
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = from_query(
            &ws,
            exec.clone(),
            profile("A"),
            None,
            "SELECT 1".into(),
            "canceled",
            SourceInfo::from_profile(&profile("A")),
            cancel,
            Arc::new(Progress::default()),
        )
        .await;
        assert!(err.is_err());
        assert!(ws.provenance("canceled").await.unwrap().is_none());
    }
}
