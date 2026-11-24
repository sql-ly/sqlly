//! Where notebook runs and snapshots live: run records in the core's
//! JSON-lines store (`<notebooks dir>/runs/<notebook>/<notebook>.runs.jsonl`,
//! the same files the command-line runner writes), and cell outputs (each
//! run's and pinned snapshots) encrypted in the notebook cache. The
//! snapshot index next to the run records holds names and metadata only,
//! never rows.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlly_services::notebook::run_record::RunRecord;
use sqlly_services::notebook::run_record::RunStore;
use sqlly_services::notebook::{CellOutput, ConnectionIdentity};

/// A pinned snapshot's metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub name: String,
    pub cell_id: String,
    /// The run the output came from (empty for a pin outside a run).
    #[serde(default)]
    pub run_id: String,
    pub pinned_at: DateTime<Utc>,
    pub row_count: u64,
    /// Whose results these are; decides the cache scope it is read under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<ConnectionIdentity>,
}

/// The directory name for a notebook id: the id itself when it is plain,
/// else its SHA-256 (the command-line runner's rule).
pub fn notebook_stem(notebook_id: &str) -> String {
    let plain = !notebook_id.is_empty()
        && notebook_id.len() <= 128
        && notebook_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if plain {
        notebook_id.to_string()
    } else {
        sqlly_services::notebook::run_record::sql_hash(notebook_id)
    }
}

/// Why history could not be read or written; the UI words it with
/// `localization::message` (writes run on the background executor).
pub type StoreError = sqlly_core::DisplayMessage;

pub struct HistoryStores {
    root: PathBuf,
    #[cfg(not(target_arch = "wasm32"))]
    snapshots: Result<sqlly_services::notebook_security::SnapshotStore, StoreError>,
}

const SNAPSHOT_RUN: &str = "snapshot";
/// Where the browser build keeps run records in its notebooks folder.
#[cfg(target_arch = "wasm32")]
const BROWSER_RUNS: &str = "runs";

impl HistoryStores {
    /// Stores under `root` (the notebooks data folder), with the cache key
    /// from `keys`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open(
        root: impl Into<PathBuf>,
        keys: &dyn sqlly_services::notebook_security::CacheKeyProvider,
    ) -> Self {
        use sqlly_services::notebook_security::cache::SnapshotStoreConfig;
        let root = root.into();
        let snapshots = sqlly_services::notebook_security::SnapshotStore::open(
            SnapshotStoreConfig::new(root.join("cache")),
            keys,
        )
        .map_err(|e| e.message());
        Self { root, snapshots }
    }

    /// The app's stores: the notebooks data folder and the OS keychain.
    #[cfg(all(not(target_arch = "wasm32"), not(test)))]
    pub fn open_default() -> Self {
        Self::open(
            sqlly_services::notebook::data_dir(),
            &sqlly_services::notebook_security::KeyringCacheKey::default(),
        )
    }

    /// Tests never touch the OS keychain.
    #[cfg(all(not(target_arch = "wasm32"), test))]
    pub fn open_default() -> Self {
        use sqlly_services::notebook_security::MemoryCacheKey;
        use sqlly_services::notebook_security::cache::MasterKey;
        Self::open(
            sqlly_services::notebook::data_dir(),
            &MemoryCacheKey::with_key(MasterKey::from_bytes([1; 32])),
        )
    }

    #[cfg(target_arch = "wasm32")]
    pub fn open_default() -> Self {
        Self {
            root: PathBuf::new(),
        }
    }

    pub fn runs_dir(&self, scope: &str) -> PathBuf {
        self.root.join("runs").join(notebook_stem(scope))
    }

    fn index_path(&self, scope: &str) -> PathBuf {
        self.runs_dir(scope).join("snapshots.json")
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn run_store(&self, scope: &str) -> sqlly_services::notebook::JsonlRunStore {
        sqlly_services::notebook::JsonlRunStore::new(self.runs_dir(scope))
    }

    /// Browser build: the same JSON lines, in the page's notebooks folder.
    #[cfg(target_arch = "wasm32")]
    fn run_store(
        &self,
        _scope: &str,
    ) -> sqlly_services::notebook::TextRunStore<&'static dyn sqlly_services::notebook::TextStore>
    {
        sqlly_services::notebook::TextRunStore::new(super::super::browser::store(), BROWSER_RUNS)
    }

    /// Keep `record` under data scope `scope` (the notebook's trust
    /// record's; see [`sqlly_services::notebook_security::TrustRecord::data_scope`]).
    pub fn append(&self, scope: &str, record: &RunRecord) -> Result<(), StoreError> {
        self.run_store(scope)
            .append(record)
            .map_err(|e| e.message())
    }

    /// A notebook's runs kept under `scope`, oldest first.
    pub fn list(&self, scope: &str, notebook_id: &str) -> Result<Vec<RunRecord>, StoreError> {
        self.run_store(scope)
            .list(notebook_id)
            .map_err(|e| e.message())
    }

    /// Why outputs cannot be kept (no cache key), if so. The browser has no
    /// key store the page can read synchronously, so run outputs and pinned
    /// snapshots are never written there: History keeps the runs, not
    /// their rows.
    pub fn outputs_unavailable(&self) -> Option<StoreError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.snapshots.as_ref().err().cloned()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Some(
                crate::browser_preview::unavailable(&crate::localization::text(
                    "notebook.browser.history.outputs",
                ))
                .into(),
            )
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn cache_scope(
        store: &sqlly_services::notebook_security::SnapshotStore,
        scope: &str,
        identity: Option<&ConnectionIdentity>,
    ) -> sqlly_services::notebook_security::CacheScope {
        match identity {
            Some(identity) => store.scope_for(identity),
            None => store.local_scope(scope),
        }
    }

    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    fn put(
        &self,
        scope: &str,
        identity: Option<&ConnectionIdentity>,
        run_id: &str,
        cell_id: &str,
        output: &CellOutput,
        pinned: bool,
    ) -> Result<(), StoreError> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_services::notebook_security::cache::{EntryKey, PutOptions};
            let store = self.snapshots.as_ref().map_err(Clone::clone)?;
            let scope = Self::cache_scope(store, scope, identity);
            store
                .put(
                    &scope,
                    &EntryKey::Run {
                        run_id: run_id.to_string(),
                        cell_id: cell_id.to_string(),
                    },
                    output,
                    PutOptions { ttl: None, pinned },
                    Utc::now().timestamp_millis(),
                )
                .map(|_| ())
                .map_err(|e| e.message())
        }
        #[cfg(target_arch = "wasm32")]
        {
            Err(crate::localization::deferred_text(
                "notebook.analysis.history.outputs_unavailable",
            ))
        }
    }

    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    fn get(
        &self,
        scope: &str,
        identity: Option<&ConnectionIdentity>,
        run_id: &str,
        cell_id: &str,
    ) -> Option<CellOutput> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_services::notebook_security::cache::{EntryKey, GetOutcome};
            let store = self.snapshots.as_ref().ok()?;
            let scope = Self::cache_scope(store, scope, identity);
            match store.get(
                &scope,
                &EntryKey::Run {
                    run_id: run_id.to_string(),
                    cell_id: cell_id.to_string(),
                },
                Utc::now().timestamp_millis(),
            ) {
                Ok(GetOutcome::Hit(hit)) => Some(hit.output),
                _ => None,
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            None
        }
    }

    /// Keep `output` as cell `cell_id`'s output in run `run_id`.
    pub fn put_run_output(
        &self,
        scope: &str,
        identity: Option<&ConnectionIdentity>,
        run_id: &str,
        cell_id: &str,
        output: &CellOutput,
    ) -> Result<(), StoreError> {
        self.put(scope, identity, run_id, cell_id, output, false)
    }

    pub fn run_output(
        &self,
        scope: &str,
        identity: Option<&ConnectionIdentity>,
        run_id: &str,
        cell_id: &str,
    ) -> Option<CellOutput> {
        self.get(scope, identity, run_id, cell_id)
            .map(|o| o.snapshot(o.rows.len()))
    }

    /// Pinned snapshots, newest last. The browser never pins one (see
    /// [`Self::outputs_unavailable`]), so it has none to list.
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    pub fn snapshots(&self, scope: &str) -> Vec<SnapshotEntry> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            std::fs::read_to_string(self.index_path(scope))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
                .unwrap_or_default()
        }
        #[cfg(target_arch = "wasm32")]
        {
            Vec::new()
        }
    }

    /// Pin `output` under `name` (replacing a snapshot of that name).
    pub fn pin(
        &self,
        scope: &str,
        entry: SnapshotEntry,
        output: &CellOutput,
    ) -> Result<(), StoreError> {
        let key = format!("{SNAPSHOT_RUN}:{}", entry.name);
        self.put(
            scope,
            entry.identity.as_ref(),
            &key,
            &entry.cell_id,
            output,
            true,
        )?;
        let mut index = self.snapshots(scope);
        index.retain(|e| e.name != entry.name);
        index.push(entry);
        let text = serde_json::to_string_pretty(&index).unwrap_or_default();
        let path = self.index_path(scope);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::raw(e.to_string()))?;
        }
        crate::fs_safe::write_atomic(&path, text.as_bytes())
            .map_err(|e| StoreError::raw(e.to_string()))
    }

    pub fn snapshot_output(&self, scope: &str, name: &str) -> Option<(SnapshotEntry, CellOutput)> {
        let entry = self.snapshots(scope).into_iter().find(|e| e.name == name)?;
        let key = format!("{SNAPSHOT_RUN}:{name}");
        let output = self.get(scope, entry.identity.as_ref(), &key, &entry.cell_id)?;
        Some((entry, output.snapshot(output.rows.len())))
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_services::notebook::fingerprint::Fingerprint;
    use sqlly_services::notebook::{Completeness, Provenance};
    use sqlly_services::notebook_security::MemoryCacheKey;
    use sqlly_services::notebook_security::cache::MasterKey;

    fn output(n: i64) -> CellOutput {
        CellOutput::capped(
            vec![sqlly_core::ResultColumn {
                name: "n".into(),
                data_type: "int".into(),
                nullable: false,
            }],
            (0..n)
                .map(|i| vec![sqlly_core::CellValue::I64(i)])
                .collect(),
            None,
            100,
            1,
            "r_1",
            Fingerprint("f".into()),
            Provenance::default(),
        )
    }

    #[test]
    fn snapshots_are_encrypted_and_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let keys = MemoryCacheKey::with_key(MasterKey::from_bytes([7; 32]));
        let stores = HistoryStores::open(dir.path(), &keys);
        assert!(stores.outputs_unavailable().is_none());
        let alice = ConnectionIdentity {
            profile_id: "p1".into(),
            login_identity: "alice".into(),
            ..ConnectionIdentity::default()
        };
        let entry = SnapshotEntry {
            name: "golden".into(),
            cell_id: "c1".into(),
            run_id: "r_1".into(),
            pinned_at: Utc::now(),
            row_count: 3,
            identity: Some(alice.clone()),
        };
        stores.pin("nb_1", entry, &output(3)).unwrap();
        let (entry, out) = stores.snapshot_output("nb_1", "golden").unwrap();
        assert_eq!(entry.cell_id, "c1");
        assert_eq!(out.rows.len(), 3);
        assert_eq!(out.completeness, Completeness::Complete);
        let index = std::fs::read_to_string(stores.index_path("nb_1")).unwrap();
        assert!(!index.contains("\"rows\""));
        // Another principal's scope sees nothing under the same names.
        assert!(
            stores
                .get(
                    "nb_1",
                    Some(&ConnectionIdentity::default()),
                    "snapshot:golden",
                    "c1"
                )
                .is_none()
        );
        stores
            .put_run_output("nb_1", None, "r_2", "c2", &output(2))
            .unwrap();
        assert_eq!(
            stores
                .run_output("nb_1", None, "r_2", "c2")
                .unwrap()
                .rows
                .len(),
            2
        );
        assert!(stores.run_output("nb_1", None, "r_9", "c2").is_none());
        assert!(stores.runs_dir("../x").starts_with(dir.path().join("runs")));
        assert_eq!(stores.runs_dir("../x").file_name().unwrap().len(), 64);
    }
}
