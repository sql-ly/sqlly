//! The Filter rows dialog's Recent filters list: the filter expressions
//! applied most recently, kept per saved-filter scope (connection, database
//! and table — see [`crate::saved_filters::filter_scope`]) and persisted in
//! its own sidecar file (`recent-filters.json`) so the list survives a
//! restart.
//!
//! Recents are per table rather than global: an expression names the
//! table's columns (`status = active`), so another table's recents would
//! mostly fail to parse there.
//!
//! The pure store is testable anywhere; the process-wide memory loads it on
//! first use and writes it back off the UI thread after every change, like
//! the Redis key browser's filter memory in `redis_key_filters`.

use std::collections::BTreeMap;

use crate::preferences::ResultFormattingPreferences;

/// Recent filters kept per table unless Settings says otherwise.
pub const DEFAULT_LIMIT: u32 = 10;
/// Upper bound of the Settings stepper.
pub const MAX_LIMIT: u32 = 50;

pub(crate) fn default_limit() -> u32 {
    DEFAULT_LIMIT
}

/// Whether recents are recorded and shown, and how many each table keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecentFilterPolicy {
    pub enabled: bool,
    pub limit: usize,
}

impl RecentFilterPolicy {
    pub fn from_prefs(prefs: &ResultFormattingPreferences) -> Self {
        let limit = prefs.recent_filters_limit.min(MAX_LIMIT) as usize;
        Self {
            enabled: prefs.remember_recent_filters && limit > 0,
            limit,
        }
    }
}

/// The on-disk store: scope key → expressions, most recent first.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecentFilterStore {
    #[serde(default)]
    scopes: BTreeMap<String, Vec<String>>,
}

impl RecentFilterStore {
    pub const FILE: &'static str = "recent-filters.json";

    /// The recents for `scope`, most recent first, at most `policy.limit`;
    /// nothing while the policy is off.
    pub fn recent(&self, scope: &str, policy: RecentFilterPolicy) -> Vec<String> {
        if !policy.enabled {
            return Vec::new();
        }
        self.scopes
            .get(scope)
            .map(|list| list.iter().take(policy.limit).cloned().collect())
            .unwrap_or_default()
    }

    /// Move `expression` to the front of `scope`'s list (deduplicated,
    /// capped at the policy's limit). Returns whether anything changed;
    /// blank expressions and a disabled policy record nothing.
    pub fn record(&mut self, scope: &str, expression: &str, policy: RecentFilterPolicy) -> bool {
        let expression = expression.trim();
        if !policy.enabled || expression.is_empty() {
            return false;
        }
        let list = self.scopes.entry(scope.to_string()).or_default();
        if list.first().map(String::as_str) == Some(expression) && list.len() <= policy.limit {
            return false;
        }
        list.retain(|known| known != expression);
        list.insert(0, expression.to_string());
        list.truncate(policy.limit);
        true
    }

    /// Forget every recent filter. Returns whether there was any.
    pub fn clear(&mut self) -> bool {
        let had_any = !self.scopes.is_empty();
        self.scopes.clear();
        had_any
    }

    /// Errors are for the log only (never shown), so they stay plain text.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store::<Self>(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                Err(crate::localization::message(error))
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        let data = serde_json::to_vec_pretty(self).map_err(|error| error.to_string())?;
        crate::fs_safe::write_atomic(path, &data).map_err(|error| error.to_string())
    }
}

/// The process-wide memory: loaded on first use, written back off the UI
/// thread after every change. An unreadable file blocks recording (the
/// user's recents are in it) until Clear recent filters replaces it.
#[derive(Default)]
struct PersistentRecents {
    loaded: bool,
    store: RecentFilterStore,
    blocked: Option<String>,
}

static PERSISTENT_RECENTS: std::sync::LazyLock<std::sync::Mutex<PersistentRecents>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(PersistentRecents::default()));

/// Serializes background writes so an older snapshot never lands last.
#[cfg(not(target_arch = "wasm32"))]
static PERSISTENT_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Where the store lives; `None` in unit tests, which must not touch the
/// user's data directory.
#[cfg(not(target_arch = "wasm32"))]
fn store_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    crate::host_storage::artifact_path(
        crate::host_storage::StorageArea::Data,
        RecentFilterStore::FILE,
    )
}

impl PersistentRecents {
    fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = store_path() {
            match RecentFilterStore::load_from(&path) {
                Ok(store) => self.store = store,
                Err(error) => self.blocked = Some(error),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.store = crate::preferences::wasm_load_json(RecentFilterStore::FILE);
        }
    }
}

/// Write the current store in the background (native) or to browser storage.
fn persist(store: RecentFilterStore) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let Some(path) = store_path() else {
            return;
        };
        std::thread::spawn(move || {
            let _serial = PERSISTENT_WRITE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            // Write the newest state, not necessarily the one this thread was
            // spawned with, so racing writes converge on the latest.
            let latest = PERSISTENT_RECENTS
                .lock()
                .map(|recents| recents.store.clone())
                .unwrap_or(store);
            if let Err(error) = latest.save_to(&path) {
                tracing::warn!(%error, "recent filter store write failed");
            }
        });
    }
    #[cfg(target_arch = "wasm32")]
    crate::preferences::wasm_save_json(RecentFilterStore::FILE, &store);
}

/// The recent filters for `scope`, most recent first.
pub fn recent_filters(scope: &str, policy: RecentFilterPolicy) -> Vec<String> {
    let Ok(mut recents) = PERSISTENT_RECENTS.lock() else {
        return Vec::new();
    };
    recents.ensure_loaded();
    recents.store.recent(scope, policy)
}

/// Record `expression` as `scope`'s most recent filter and write the store
/// back. A store that failed to load is left alone and the reason logged.
pub fn remember_filter(scope: &str, expression: &str, policy: RecentFilterPolicy) {
    let Ok(mut recents) = PERSISTENT_RECENTS.lock() else {
        return;
    };
    recents.ensure_loaded();
    if let Some(reason) = &recents.blocked {
        tracing::warn!(%reason, "recent filter not remembered");
        return;
    }
    if !recents.store.record(scope, expression, policy) {
        return;
    }
    let snapshot = recents.store.clone();
    drop(recents);
    persist(snapshot);
}

/// Forget every recent filter (Settings ▸ Results ▸ General). An unreadable
/// store is replaced, since the user asked for it to be emptied.
pub fn clear_recent_filters() {
    let Ok(mut recents) = PERSISTENT_RECENTS.lock() else {
        return;
    };
    recents.ensure_loaded();
    let was_blocked = recents.blocked.take().is_some();
    if !recents.store.clear() && !was_blocked {
        return;
    }
    let snapshot = recents.store.clone();
    drop(recents);
    persist(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: RecentFilterPolicy = RecentFilterPolicy {
        enabled: true,
        limit: 10,
    };

    fn policy(limit: usize) -> RecentFilterPolicy {
        RecentFilterPolicy {
            enabled: true,
            limit,
        }
    }

    #[test]
    fn records_most_recent_first() {
        let mut store = RecentFilterStore::default();
        assert!(store.record("t", "a = 1", ON));
        assert!(store.record("t", "b = 2", ON));
        assert!(store.record("t", "c = 3", ON));
        assert_eq!(store.recent("t", ON), vec!["c = 3", "b = 2", "a = 1"]);
    }

    #[test]
    fn re_applying_moves_to_front_without_duplicating() {
        let mut store = RecentFilterStore::default();
        store.record("t", "a = 1", ON);
        store.record("t", "b = 2", ON);
        assert!(store.record("t", "  a = 1  ", ON));
        assert_eq!(store.recent("t", ON), vec!["a = 1", "b = 2"]);
        // Already at the front: nothing to write.
        assert!(!store.record("t", "a = 1", ON));
        assert_eq!(store.recent("t", ON), vec!["a = 1", "b = 2"]);
    }

    #[test]
    fn caps_at_the_limit_dropping_the_oldest() {
        let mut store = RecentFilterStore::default();
        for i in 0..5 {
            store.record("t", &format!("n = {i}"), policy(3));
        }
        assert_eq!(
            store.recent("t", policy(3)),
            vec!["n = 4", "n = 3", "n = 2"]
        );
        // A lower limit applies to what is shown and to the next record.
        assert_eq!(store.recent("t", policy(2)), vec!["n = 4", "n = 3"]);
        store.record("t", "n = 5", policy(2));
        assert_eq!(store.recent("t", policy(10)), vec!["n = 5", "n = 4"]);
    }

    #[test]
    fn scopes_are_kept_apart() {
        let mut store = RecentFilterStore::default();
        store.record("orders", "status = open", ON);
        store.record("customers", "name ^Acme", ON);
        assert_eq!(store.recent("orders", ON), vec!["status = open"]);
        assert_eq!(store.recent("customers", ON), vec!["name ^Acme"]);
        assert!(store.recent("products", ON).is_empty());
    }

    #[test]
    fn disabled_records_and_shows_nothing() {
        let off = RecentFilterPolicy {
            enabled: false,
            limit: 10,
        };
        let mut store = RecentFilterStore::default();
        assert!(!store.record("t", "a = 1", off));
        assert_eq!(store, RecentFilterStore::default());
        store.record("t", "a = 1", ON);
        assert!(store.recent("t", off).is_empty());
    }

    #[test]
    fn blank_expressions_are_not_recorded() {
        let mut store = RecentFilterStore::default();
        assert!(!store.record("t", "   ", ON));
        assert_eq!(store, RecentFilterStore::default());
    }

    #[test]
    fn clear_forgets_every_scope() {
        let mut store = RecentFilterStore::default();
        store.record("a", "x = 1", ON);
        store.record("b", "y = 2", ON);
        assert!(store.clear());
        assert!(store.recent("a", ON).is_empty());
        assert!(store.recent("b", ON).is_empty());
        assert!(!store.clear());
    }

    #[test]
    fn policy_follows_settings() {
        let mut prefs = ResultFormattingPreferences::default();
        assert_eq!(RecentFilterPolicy::from_prefs(&prefs), policy(10));
        prefs.remember_recent_filters = false;
        assert!(!RecentFilterPolicy::from_prefs(&prefs).enabled);
        prefs.remember_recent_filters = true;
        prefs.recent_filters_limit = 0;
        assert!(!RecentFilterPolicy::from_prefs(&prefs).enabled);
        prefs.recent_filters_limit = 500;
        assert_eq!(
            RecentFilterPolicy::from_prefs(&prefs).limit,
            MAX_LIMIT as usize
        );
    }

    #[test]
    fn settings_from_an_older_file_default_on_with_ten() {
        let prefs: ResultFormattingPreferences = serde_json::from_str("{}").unwrap();
        assert!(prefs.remember_recent_filters);
        assert_eq!(prefs.recent_filters_limit, DEFAULT_LIMIT);
        let prefs: ResultFormattingPreferences =
            serde_json::from_str(r#"{"density":"compact"}"#).unwrap();
        assert!(prefs.remember_recent_filters);
        assert_eq!(prefs.recent_filters_limit, 10);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn persistence_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RecentFilterStore::FILE);
        assert_eq!(
            RecentFilterStore::load_from(&path).unwrap(),
            RecentFilterStore::default()
        );
        let mut store = RecentFilterStore::default();
        store.record("t", "a = 1", ON);
        store.record("t", "b = 2", ON);
        store.record("u", "c ~x", ON);
        store.save_to(&path).unwrap();
        let loaded = RecentFilterStore::load_from(&path).unwrap();
        assert_eq!(loaded, store);
        assert_eq!(loaded.recent("t", ON), vec!["b = 2", "a = 1"]);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn corrupt_file_is_reported_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(RecentFilterStore::FILE);
        std::fs::write(&path, b"{not json").unwrap();
        assert!(RecentFilterStore::load_from(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"{not json");
    }
}
