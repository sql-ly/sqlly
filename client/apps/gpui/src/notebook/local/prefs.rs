//! Local data preferences, kept beside the notebook preferences: which
//! connections may have their results copied to this computer, the size
//! limits for each notebook's local data, how many cells run at once on one
//! connection, and the files outside a notebook's folder the user approved.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub const DEFAULT_PER_CONNECTION: usize = 2;
pub const MAX_PER_CONNECTION: usize = 8;
const MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalPrefs {
    /// Profile ids whose results may be materialized locally (the
    /// per-connection opt-in, asked the first time).
    pub materialize_allowed: BTreeSet<String>,
    /// Rows one local relation may hold before it is kept partial.
    pub max_rows_per_relation: u64,
    /// One notebook's local data size, in MiB, at which new relations are refused.
    pub max_disk_mb: u64,
    /// Concurrent read cells per connection.
    pub per_connection: usize,
    /// Absolute attachment paths approved on this machine, by notebook id.
    pub approved_files: BTreeMap<String, BTreeSet<String>>,
}

impl Default for LocalPrefs {
    fn default() -> Self {
        let quota = sqlly_services_quota();
        Self {
            materialize_allowed: BTreeSet::new(),
            max_rows_per_relation: quota.0,
            max_disk_mb: quota.1 / MIB,
            per_connection: DEFAULT_PER_CONNECTION,
            approved_files: BTreeMap::new(),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn sqlly_services_quota() -> (u64, u64) {
    let quota = sqlly_services::local_analytics::WorkspaceQuota::default();
    (quota.max_rows_per_relation, quota.max_disk_bytes)
}

#[cfg(target_arch = "wasm32")]
fn sqlly_services_quota() -> (u64, u64) {
    (10_000_000, 4 * 1024 * MIB)
}

fn prefs_path() -> PathBuf {
    super::super::autosave::data_root().join("notebook-local-preferences.json")
}

impl LocalPrefs {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn load() -> Self {
        match crate::fs_safe::load_json_store::<Self>(&prefs_path()) {
            crate::fs_safe::StoreLoad::Loaded(prefs) => prefs.sanitized(),
            _ => Self::default(),
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn load() -> Self {
        Self::default()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save(&self) {
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = crate::fs_safe::write_atomic(&prefs_path(), &json);
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub fn save(&self) {}

    fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        if self.max_rows_per_relation == 0 {
            self.max_rows_per_relation = defaults.max_rows_per_relation;
        }
        if self.max_disk_mb == 0 {
            self.max_disk_mb = defaults.max_disk_mb;
        }
        self.per_connection = self.per_connection.clamp(1, MAX_PER_CONNECTION);
        self
    }

    pub fn allows_materialize(&self, profile_id: &str) -> bool {
        self.materialize_allowed.contains(profile_id)
    }

    pub fn is_file_approved(&self, notebook_id: &str, path: &str) -> bool {
        self.approved_files
            .get(notebook_id)
            .is_some_and(|set| set.contains(path))
    }

    pub fn approve_file(&mut self, notebook_id: &str, path: &str) {
        self.approved_files
            .entry(notebook_id.to_string())
            .or_default()
            .insert(path.to_string());
    }

    /// The workspace quota these preferences ask for.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn quota(&self) -> sqlly_services::local_analytics::WorkspaceQuota {
        sqlly_services::local_analytics::WorkspaceQuota {
            max_rows_per_relation: self.max_rows_per_relation,
            max_disk_bytes: self.max_disk_mb.saturating_mul(MIB),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_repair_bad_values() {
        let dir = tempfile::tempdir().unwrap();
        crate::notebook::autosave::set_test_root(Some(dir.path().to_path_buf()));
        assert_eq!(LocalPrefs::load(), LocalPrefs::default());
        let mut prefs = LocalPrefs::default();
        prefs.materialize_allowed.insert("p1".into());
        prefs.approve_file("nb", "/data/a.csv");
        prefs.per_connection = 99;
        prefs.max_disk_mb = 0;
        prefs.save();
        let back = LocalPrefs::load();
        assert!(back.allows_materialize("p1"));
        assert!(!back.allows_materialize("p2"));
        assert!(back.is_file_approved("nb", "/data/a.csv"));
        assert!(!back.is_file_approved("other", "/data/a.csv"));
        assert_eq!(back.per_connection, MAX_PER_CONNECTION);
        assert_eq!(back.max_disk_mb, LocalPrefs::default().max_disk_mb);
        assert_eq!(back.quota().max_disk_bytes, back.max_disk_mb * MIB);
        crate::notebook::autosave::set_test_root(None);
    }
}
