//! The async face of the notebook workspaces. Every DuckDB call runs on the
//! blocking pool; row streams from the execute path are bridged into the
//! ingest through a bounded channel, so a slow ingest slows the server read
//! and a stopped ingest (quota, cancel) stops it.

use crate::notebook_security::workspace_keys::WorkspaceKeyStore;
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn, RowReceiver};
use sqlly_engines::duckdb::local_store::{
    self, CancelToken, EncryptionKey, EvictionReport, ExportFormat, ExportOutcome, ExportSource,
    ImportFormat, ImportRequest, LocalResult, LocalResultSet, LocalStoreError, LocalWorkspace,
    MaterializeOutcome, MaterializeRequest, ProvenanceRecord, RelationInfo, SourceBatch,
    SourceInfo, WorkspaceOptions, WorkspaceQuota, WorkspaceUsage, WriteMode,
};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};

/// Rows per batch handed to the appender from an in-memory snapshot.
const SNAPSHOT_BATCH_ROWS: usize = 2048;
/// Batches buffered between a row stream and the ingest.
const STREAM_BUFFER_BATCHES: usize = 4;

#[derive(Debug, Clone)]
pub struct LocalAnalyticsConfig {
    /// The directory holding one folder per notebook.
    pub root: PathBuf,
    /// Limits for each workspace.
    pub quota: WorkspaceQuota,
    /// The budget for all workspaces under `root`; closed workspaces beyond
    /// it are evicted, least recently used first.
    pub max_total_bytes: u64,
}

impl LocalAnalyticsConfig {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            quota: WorkspaceQuota::default(),
            max_total_bytes: 20 * 1024 * 1024 * 1024,
        }
    }

    /// `<data dir>/SQLLY/notebooks/Local`: `~/Library/Application
    /// Support/SQLLY/…` on macOS, `~/.local/share/SQLLY/…` on Linux,
    /// `%APPDATA%\SQLLY\…` on Windows.
    pub fn default_root() -> PathBuf {
        crate::notebook::data_dir().join("local")
    }
}

/// How long [`LocalAnalytics::recreate`] waits for other handles on the
/// workspace to drop.
pub const HANDLE_RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// How one notebook's workspace opens.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    pub encryption_key: Option<EncryptionKey>,
    pub delete_on_close: bool,
}

/// A fresh random key for an encrypted workspace (32 random bytes, hex).
/// The app stores it in the OS keychain under the notebook's id.
pub fn generate_encryption_key() -> EncryptionKey {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    EncryptionKey::new(bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Owns the workspace root. One per process (the app keeps it in its
/// services); cheap to share behind an `Arc`.
pub struct LocalAnalytics {
    config: LocalAnalyticsConfig,
    open: Mutex<HashMap<String, Weak<LocalWorkspace>>>,
}

impl LocalAnalytics {
    pub fn new(config: LocalAnalyticsConfig) -> Self {
        Self {
            config,
            open: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &LocalAnalyticsConfig {
        &self.config
    }

    fn live(&self, notebook_id: &str) -> Option<Arc<LocalWorkspace>> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|_, weak| weak.strong_count() > 0);
        open.get(notebook_id).and_then(Weak::upgrade)
    }

    fn open_ids(&self) -> Vec<String> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.retain(|_, weak| weak.strong_count() > 0);
        open.keys().cloned().collect()
    }

    /// The workspace for `notebook_id`, shared with anyone who already has
    /// it open (their open options win). Opening also evicts closed
    /// workspaces beyond the root's budget.
    pub async fn open(
        &self,
        notebook_id: &str,
        options: OpenOptions,
    ) -> LocalResult<NotebookWorkspace> {
        if let Some(workspace) = self.live(notebook_id) {
            return Ok(NotebookWorkspace(workspace));
        }
        let id = notebook_id.to_string();
        let root = self.config.root.clone();
        let workspace_options = WorkspaceOptions {
            quota: self.config.quota.clone(),
            encryption_key: options.encryption_key,
            delete_on_close: options.delete_on_close,
        };
        let workspace =
            blocking(move || LocalWorkspace::open(&id, &root, workspace_options)).await?;
        let workspace = {
            let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
            match open.get(notebook_id).and_then(Weak::upgrade) {
                // Another caller opened it meanwhile; both handles share
                // one DuckDB instance, so keep theirs.
                Some(existing) => existing,
                None => {
                    let workspace = Arc::new(workspace);
                    open.insert(notebook_id.to_string(), Arc::downgrade(&workspace));
                    workspace
                }
            }
        };
        if let Err(error) = self.evict().await {
            tracing::warn!(%error, "notebook workspace eviction failed");
        }
        Ok(NotebookWorkspace(workspace))
    }

    /// [`Self::open`], encrypted with the key `keys` holds for
    /// `notebook_id` when there is one. A keychain that can't be read opens
    /// the workspace as if it had no key (an encrypted file then reports
    /// [`LocalStoreError::EncryptionKey`]); a key left behind by a switch
    /// back to plain that could not forget it opens the plain file.
    pub async fn open_with_keys(
        &self,
        notebook_id: &str,
        keys: Arc<dyn WorkspaceKeyStore>,
    ) -> LocalResult<NotebookWorkspace> {
        let id = notebook_id.to_string();
        let stored = tokio::task::spawn_blocking(move || keys.load(&id))
            .await
            .map_err(|error| LocalStoreError::WorkerStopped(error.to_string()))?;
        let encryption_key = match stored {
            Ok(key) => key.map(EncryptionKey::new),
            Err(error) => {
                tracing::warn!(%error, "could not read the notebook workspace key");
                None
            }
        };
        let keyed = encryption_key.is_some();
        match self
            .open(
                notebook_id,
                OpenOptions {
                    encryption_key,
                    ..OpenOptions::default()
                },
            )
            .await
        {
            Err(LocalStoreError::NotEncrypted) if keyed => {
                self.open(notebook_id, OpenOptions::default()).await
            }
            opened => opened,
        }
    }

    /// Replace an empty workspace with a new one opened with `options`, to
    /// turn its encryption on or off (a DuckDB file can't change that in
    /// place). Refused while it holds relations, while another handle on
    /// it stays alive past [`HANDLE_RELEASE_WAIT`], or while a client on its
    /// database stays open; the workspace handed in is closed either way.
    pub async fn recreate(
        &self,
        workspace: NotebookWorkspace,
        options: OpenOptions,
    ) -> LocalResult<NotebookWorkspace> {
        let id = workspace.notebook_id().to_string();
        if !workspace.relations().await?.is_empty() {
            return Err(LocalStoreError::WorkspaceNotEmpty(id));
        }
        // A handle a short read (the Local data page's refresh) still holds
        // is given a moment to go before the switch is refused.
        let deadline = std::time::Instant::now() + HANDLE_RELEASE_WAIT;
        let mut shared = workspace.0;
        let inner = loop {
            match Arc::try_unwrap(shared) {
                Ok(inner) => break inner,
                Err(back) if std::time::Instant::now() < deadline => {
                    shared = back;
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(_) => return Err(LocalStoreError::WorkspaceOpen(id)),
            }
        };
        if !blocking(move || Ok(inner.close_and_remove())).await? {
            return Err(LocalStoreError::WorkspaceOpen(id));
        }
        self.open(&id, options).await
    }

    /// Evict closed workspaces until the root fits its budget.
    pub async fn evict(&self) -> LocalResult<EvictionReport> {
        let root = self.config.root.clone();
        let limit = self.config.max_total_bytes;
        let keep = self.open_ids();
        blocking(move || {
            let keep: Vec<&str> = keep.iter().map(String::as_str).collect();
            local_store::evict_lru(&root, limit, &keep)
        })
        .await
    }

    /// Every workspace under the root, least recently used first.
    pub async fn usages(&self) -> LocalResult<Vec<WorkspaceUsage>> {
        let root = self.config.root.clone();
        blocking(move || local_store::workspace_usages(&root)).await
    }

    /// Delete a notebook's workspace. Refused while it is open.
    pub async fn remove(&self, notebook_id: &str) -> LocalResult<bool> {
        if self.live(notebook_id).is_some() {
            return Err(LocalStoreError::WorkspaceOpen(notebook_id.to_string()));
        }
        let root = self.config.root.clone();
        let id = notebook_id.to_string();
        blocking(move || local_store::remove_workspace(&root, &id)).await
    }
}

async fn blocking<T, F>(work: F) -> LocalResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> LocalResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| LocalStoreError::WorkerStopped(error.to_string()))?
}

/// Rows from a shared snapshot (a cell output's capped row `Arc`), cloned
/// out batch by batch rather than all at once.
pub struct SnapshotRows {
    rows: Arc<Vec<Vec<CellValue>>>,
    next: usize,
}

impl SnapshotRows {
    pub fn new(rows: Arc<Vec<Vec<CellValue>>>) -> Self {
        Self { rows, next: 0 }
    }
}

impl Iterator for SnapshotRows {
    type Item = SourceBatch;

    fn next(&mut self) -> Option<SourceBatch> {
        if self.next >= self.rows.len() {
            return None;
        }
        let end = (self.next + SNAPSHOT_BATCH_ROWS).min(self.rows.len());
        let batch = self.rows[self.next..end].to_vec();
        self.next = end;
        Some(Ok(batch))
    }
}

/// A file import: the relation to create, the path the user approved, and
/// the cell and run doing it (for provenance).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSpec {
    pub relation: String,
    pub path: PathBuf,
    pub format: ImportFormat,
    pub mode: WriteMode,
    pub cell_id: Option<String>,
    pub run_id: Option<String>,
}

impl ImportSpec {
    pub fn new(
        relation: impl Into<String>,
        path: impl Into<PathBuf>,
        format: ImportFormat,
    ) -> Self {
        Self {
            relation: relation.into(),
            path: path.into(),
            format,
            mode: WriteMode::FailIfExists,
            cell_id: None,
            run_id: None,
        }
    }
}

/// What to export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportTarget {
    Relation(String),
    /// One read-only query against the workspace.
    Query(String),
}

/// One open notebook workspace. Clones share it; the workspace closes when
/// the last clone (and every app client on its database) drops.
#[derive(Clone)]
pub struct NotebookWorkspace(Arc<LocalWorkspace>);

impl std::fmt::Debug for NotebookWorkspace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl NotebookWorkspace {
    pub fn workspace(&self) -> &Arc<LocalWorkspace> {
        &self.0
    }

    pub fn notebook_id(&self) -> &str {
        self.0.notebook_id()
    }

    pub fn db_path(&self) -> &Path {
        self.0.db_path()
    }

    /// A read-only DuckDB profile on this workspace for the app's execute
    /// path (SQL cells targeting local data).
    pub fn connection_profile(&self, name: &str) -> ConnectionProfile {
        self.0.connection_profile(name)
    }

    async fn run<T, F>(&self, work: F) -> LocalResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&LocalWorkspace) -> LocalResult<T> + Send + 'static,
    {
        let workspace = Arc::clone(&self.0);
        blocking(move || work(&workspace)).await
    }

    /// Materialize rows already in memory (a cell output's snapshot, or any
    /// iterator of batches).
    pub async fn materialize<I>(
        &self,
        relation: impl Into<String>,
        columns: Vec<ResultColumn>,
        batches: I,
        source: SourceInfo,
        mode: WriteMode,
        cancel: CancelToken,
    ) -> LocalResult<MaterializeOutcome>
    where
        I: IntoIterator<Item = SourceBatch> + Send + 'static,
        I::IntoIter: Send,
    {
        let relation = relation.into();
        self.run(move |workspace| {
            workspace.materialize(
                &MaterializeRequest {
                    relation: &relation,
                    columns: &columns,
                    source: &source,
                    mode,
                },
                batches,
                &cancel,
            )
        })
        .await
    }

    /// Materialize result set `result_set_index` of a live row stream as it
    /// arrives. Batches of other result sets are skipped. When ingestion
    /// stops early (a quota, a cancel, an error) the stream is dropped, which
    /// tells the producer to stop reading from the server.
    #[allow(clippy::too_many_arguments)]
    pub async fn materialize_stream(
        &self,
        relation: impl Into<String>,
        columns: Vec<ResultColumn>,
        mut rows: RowReceiver,
        result_set_index: usize,
        source: SourceInfo,
        mode: WriteMode,
        cancel: CancelToken,
    ) -> LocalResult<MaterializeOutcome> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<SourceBatch>(STREAM_BUFFER_BATCHES);
        let batches = std::iter::from_fn(move || rx.blocking_recv());
        let ingest = self.materialize(relation, columns, batches, source, mode, cancel);
        let pump = async move {
            loop {
                let batch = tokio::select! {
                    batch = rows.recv() => batch,
                    () = tx.closed() => break,
                };
                let Some(batch) = batch else { break };
                if batch.result_set_index != result_set_index {
                    continue;
                }
                if tx.send(Ok(batch.rows)).await.is_err() {
                    break;
                }
            }
        };
        let (outcome, ()) = tokio::join!(ingest, pump);
        outcome
    }

    /// Import an approved file into a relation.
    pub async fn import_file(
        &self,
        spec: ImportSpec,
        cancel: CancelToken,
    ) -> LocalResult<MaterializeOutcome> {
        self.run(move |workspace| {
            workspace.import_file(
                &ImportRequest {
                    relation: &spec.relation,
                    path: &spec.path,
                    format: spec.format,
                    mode: spec.mode,
                    cell_id: spec.cell_id,
                    run_id: spec.run_id,
                },
                &cancel,
            )
        })
        .await
    }

    /// Export a relation or a read-only query to an approved path.
    pub async fn export(
        &self,
        target: ExportTarget,
        format: ExportFormat,
        destination: PathBuf,
        overwrite: bool,
        cancel: CancelToken,
    ) -> LocalResult<ExportOutcome> {
        self.run(move |workspace| {
            let source = match &target {
                ExportTarget::Relation(name) => ExportSource::Relation(name),
                ExportTarget::Query(sql) => ExportSource::Query(sql),
            };
            workspace.export(&source, format, &destination, overwrite, &cancel)
        })
        .await
    }

    /// Read-only SQL against the workspace, capped at `max_rows` (the
    /// quota's query cap when `None`).
    pub async fn query(
        &self,
        sql: impl Into<String>,
        max_rows: Option<usize>,
        cancel: CancelToken,
    ) -> LocalResult<Vec<LocalResultSet>> {
        let sql = sql.into();
        self.run(move |workspace| workspace.query(&sql, max_rows, &cancel))
            .await
    }

    /// DuckDB `SUMMARIZE` statistics for a relation.
    pub async fn summarize(
        &self,
        relation: impl Into<String>,
        cancel: CancelToken,
    ) -> LocalResult<LocalResultSet> {
        let relation = relation.into();
        self.run(move |workspace| workspace.summarize(&relation, &cancel))
            .await
    }

    pub async fn relations(&self) -> LocalResult<Vec<RelationInfo>> {
        self.run(|workspace| workspace.relations()).await
    }

    pub async fn drop_relation(&self, relation: impl Into<String>) -> LocalResult<bool> {
        let relation = relation.into();
        self.run(move |workspace| workspace.drop_relation(&relation))
            .await
    }

    pub async fn clear(&self) -> LocalResult<()> {
        self.run(|workspace| workspace.clear()).await
    }

    pub async fn provenance(
        &self,
        relation: impl Into<String>,
    ) -> LocalResult<Option<ProvenanceRecord>> {
        let relation = relation.into();
        self.run(move |workspace| workspace.provenance(&relation))
            .await
    }

    /// Provenance of a relation and of everything it was derived from.
    pub async fn lineage(&self, relation: impl Into<String>) -> LocalResult<Vec<ProvenanceRecord>> {
        let relation = relation.into();
        self.run(move |workspace| workspace.lineage(&relation))
            .await
    }

    pub async fn disk_usage(&self) -> LocalResult<u64> {
        self.run(|workspace| Ok(workspace.disk_usage())).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, RowBatch, SessionId, row_channel};

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn service(root: &Path) -> LocalAnalytics {
        LocalAnalytics::new(LocalAnalyticsConfig::new(root))
    }

    fn rows(count: i64) -> Vec<Vec<CellValue>> {
        (0..count)
            .map(|i| vec![CellValue::I64(i), CellValue::String(format!("r{i}"))])
            .collect()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn materialized_snapshots_join_across_sources() {
        let root = tempfile::tempdir().unwrap();
        let analytics = service(root.path());
        let workspace = analytics
            .open("nb_join", OpenOptions::default())
            .await
            .unwrap();
        let snapshot = Arc::new(rows(5000));
        let outcome = workspace
            .materialize(
                "left_side",
                vec![col("id", "int"), col("name", "nvarchar")],
                SnapshotRows::new(Arc::clone(&snapshot)),
                SourceInfo::query(DatabaseType::SqlServer, "Prod"),
                WriteMode::FailIfExists,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.row_count, 5000);
        workspace
            .materialize(
                "right_side",
                vec![col("id", "int8"), col("score", "float8")],
                vec![Ok(vec![
                    vec![CellValue::I64(1), CellValue::F64(0.5)],
                    vec![CellValue::I64(4999), CellValue::F64(2.0)],
                ])],
                SourceInfo::query(DatabaseType::Postgres, "Analytics"),
                WriteMode::FailIfExists,
                CancelToken::new(),
            )
            .await
            .unwrap();
        let sets = workspace
            .query(
                "SELECT l.name, r.score FROM left_side l JOIN right_side r USING (id) ORDER BY id",
                None,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            sets[0].rows,
            vec![
                vec![CellValue::String("r1".into()), CellValue::F64(0.5)],
                vec![CellValue::String("r4999".into()), CellValue::F64(2.0)],
            ]
        );

        // A second open shares the same workspace.
        let again = analytics
            .open("nb_join", OpenOptions::default())
            .await
            .unwrap();
        assert!(Arc::ptr_eq(again.workspace(), workspace.workspace()));
        assert_eq!(again.relations().await.unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_ingest_with_backpressure_and_stop_the_producer_at_the_quota() {
        let root = tempfile::tempdir().unwrap();
        let mut config = LocalAnalyticsConfig::new(root.path());
        config.quota.max_rows_per_relation = 250;
        let analytics = LocalAnalytics::new(config);
        let workspace = analytics
            .open("nb_stream", OpenOptions::default())
            .await
            .unwrap();

        let (tx, rx) = row_channel(2);
        let session = SessionId::new();
        let producer = tokio::spawn(async move {
            let mut sent = 0;
            for chunk in 0..1000 {
                // Result set 0 is someone else's; only set 1 is materialized.
                let index = usize::from(chunk % 5 != 0);
                let batch = RowBatch::new(session.clone(), index, rows(100));
                if tx.send(batch).await.is_err() {
                    return (sent, true);
                }
                sent += 1;
            }
            (sent, false)
        });
        let outcome = workspace
            .materialize_stream(
                "streamed",
                vec![col("id", "int4"), col("name", "text")],
                rx,
                1,
                SourceInfo::query(DatabaseType::Postgres, "pg"),
                WriteMode::FailIfExists,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(outcome.row_count, 250);
        assert!(!outcome.is_complete());
        let (sent, stopped) = producer.await.unwrap();
        assert!(stopped, "the producer should see the stream close");
        assert!(sent < 20, "sent {sent} batches before stopping");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn removal_is_refused_while_open_and_eviction_keeps_open_notebooks() {
        let root = tempfile::tempdir().unwrap();
        let mut config = LocalAnalyticsConfig::new(root.path());
        config.max_total_bytes = 0;
        let analytics = LocalAnalytics::new(config);
        let first = analytics
            .open("nb_a", OpenOptions::default())
            .await
            .unwrap();
        assert!(analytics.remove("nb_a").await.is_err());
        let path = first.db_path().to_path_buf();
        drop(first);
        // Opening another notebook evicts the closed one: the budget is zero.
        let _second = analytics
            .open("nb_b", OpenOptions::default())
            .await
            .unwrap();
        let ids: Vec<String> = analytics
            .usages()
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.notebook_id)
            .collect();
        assert_eq!(ids, ["nb_b"]);
        assert!(!path.exists());
        assert!(!analytics.remove("nb_a").await.unwrap());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn generated_keys_open_encrypted_workspaces() {
        let root = tempfile::tempdir().unwrap();
        let analytics = service(root.path());
        let key = generate_encryption_key();
        assert_ne!(key, generate_encryption_key());
        let workspace = analytics
            .open(
                "nb_secure",
                OpenOptions {
                    encryption_key: Some(key),
                    delete_on_close: false,
                },
            )
            .await
            .unwrap();
        assert!(workspace.workspace().is_encrypted());
        let summary = workspace
            .query("SELECT 1 AS one", None, CancelToken::new())
            .await
            .unwrap();
        assert_eq!(summary[0].rows, vec![vec![CellValue::I64(1)]]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_empty_workspace_is_recreated_encrypted_and_back_with_its_stored_key() {
        use crate::notebook_security::workspace_keys::{
            MemoryWorkspaceKeys, WorkspaceKeyStore, generate_workspace_key,
        };
        let root = tempfile::tempdir().unwrap();
        let analytics = service(root.path());
        let keys = Arc::new(MemoryWorkspaceKeys::default());
        let plain = analytics
            .open_with_keys("nb_switch", keys.clone())
            .await
            .unwrap();
        assert!(!plain.workspace().is_encrypted());
        let db_path = plain.db_path().to_path_buf();

        let key = generate_workspace_key();
        keys.store("nb_switch", &key).unwrap();
        let encrypted = analytics
            .recreate(
                plain,
                OpenOptions {
                    encryption_key: Some(EncryptionKey::new(key)),
                    delete_on_close: false,
                },
            )
            .await
            .unwrap();
        assert!(encrypted.workspace().is_encrypted());
        encrypted
            .materialize(
                "secret_rows",
                vec![col("id", "int"), col("name", "text")],
                SnapshotRows::new(Arc::new(rows(3))),
                SourceInfo::query(DatabaseType::Postgres, "CRM"),
                WriteMode::FailIfExists,
                CancelToken::new(),
            )
            .await
            .unwrap();
        let other = encrypted.clone();
        assert_eq!(
            analytics
                .recreate(other, OpenOptions::default())
                .await
                .unwrap_err(),
            LocalStoreError::WorkspaceNotEmpty("nb_switch".into()),
            "a workspace with tables is never thrown away"
        );
        drop(encrypted);
        let text = String::from_utf8_lossy(&std::fs::read(&db_path).unwrap()).into_owned();
        assert!(!text.contains("secret_rows"), "the file is encrypted");
        assert_eq!(
            analytics
                .open("nb_switch", OpenOptions::default())
                .await
                .unwrap_err(),
            LocalStoreError::EncryptionKey
        );
        let reopened = analytics
            .open_with_keys("nb_switch", keys.clone())
            .await
            .unwrap();
        assert!(reopened.workspace().is_encrypted());
        reopened.clear().await.unwrap();
        let held = reopened.clone();
        assert_eq!(
            analytics
                .recreate(held, OpenOptions::default())
                .await
                .unwrap_err(),
            LocalStoreError::WorkspaceOpen("nb_switch".into()),
            "refused while another handle is alive"
        );
        let plain = analytics
            .recreate(reopened, OpenOptions::default())
            .await
            .unwrap();
        assert!(!plain.workspace().is_encrypted());
        assert!(plain.relations().await.unwrap().is_empty());
        drop(plain);
        let stale = analytics
            .open_with_keys("nb_switch", keys.clone())
            .await
            .unwrap();
        assert!(
            !stale.workspace().is_encrypted(),
            "a key that outlived the switch back still opens the plain file"
        );
        keys.remove("nb_switch").unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_switch_waits_for_a_short_lived_handle_to_go() {
        let root = tempfile::tempdir().unwrap();
        let analytics = service(root.path());
        let workspace = analytics
            .open("nb_brief", OpenOptions::default())
            .await
            .unwrap();
        let reader = workspace.clone();
        let read = tokio::spawn(async move {
            let _ = reader.relations().await;
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(reader);
        });
        let encrypted = analytics
            .recreate(
                workspace,
                OpenOptions {
                    encryption_key: Some(generate_encryption_key()),
                    ..OpenOptions::default()
                },
            )
            .await
            .unwrap();
        assert!(encrypted.workspace().is_encrypted());
        read.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn files_round_trip_through_the_service() {
        let root = tempfile::tempdir().unwrap();
        let files = tempfile::tempdir().unwrap();
        let analytics = service(root.path());
        let workspace = analytics
            .open("nb_files", OpenOptions::default())
            .await
            .unwrap();
        let csv = files.path().join("in.csv");
        std::fs::write(&csv, "a,b\n1,x\n2,y\n").unwrap();
        let imported = workspace
            .import_file(
                ImportSpec::new("incoming", csv, ImportFormat::Csv),
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(imported.row_count, 2);
        let out = files.path().join("out.parquet");
        let exported = workspace
            .export(
                ExportTarget::Relation("incoming".into()),
                ExportFormat::Parquet,
                out.clone(),
                false,
                CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(exported.rows, 2);
        assert!(out.is_file());
        let summary = workspace
            .summarize("incoming", CancelToken::new())
            .await
            .unwrap();
        assert_eq!(summary.rows.len(), 2);
        assert!(workspace.drop_relation("incoming").await.unwrap());
        assert!(workspace.provenance("incoming").await.unwrap().is_none());
    }
}
