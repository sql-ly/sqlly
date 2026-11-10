use super::*;
use std::collections::{HashMap, HashSet};

/// Overlay built-in + user snippets onto a catalog without mutating the
/// cached Arc. Idempotent: re-merging a catalog that already carries user
/// snippets does not duplicate them. Extra non-default prefixes already on
/// the catalog (none today) are preserved.
pub(super) fn merge_user_snippets(
    mut catalog: CompletionCatalog,
    prefs: &crate::snippets::SnippetPreferences,
) -> CompletionCatalog {
    let mut snippets = sqlly_intel::intellisense::default_snippets();
    for snippet in &prefs.snippets {
        if snippet.prefix.trim().is_empty() {
            continue;
        }
        if let Some(existing) = snippets
            .iter_mut()
            .find(|(prefix, _)| prefix.eq_ignore_ascii_case(&snippet.prefix))
        {
            *existing = (snippet.prefix.clone(), snippet.body.clone());
        } else {
            snippets.push((snippet.prefix.clone(), snippet.body.clone()));
        }
    }
    let known: HashSet<String> = snippets
        .iter()
        .map(|(prefix, _)| prefix.to_ascii_lowercase())
        .collect();
    for (prefix, body) in &catalog.snippets {
        if !known.contains(&prefix.to_ascii_lowercase()) {
            snippets.push((prefix.clone(), body.clone()));
        }
    }
    catalog.snippets = snippets;
    catalog
}

#[derive(Clone)]
pub(crate) struct CachedDataModel {
    pub(crate) catalog: Arc<CompletionCatalog>,
    pub(crate) procedures: Arc<Vec<ProcedureInfo>>,
    pub(crate) snapshot: Arc<sqlly_lang::sqlmodel::ModelSnapshot>,
    pub(crate) fetched_at: chrono::DateTime<chrono::Utc>,
    /// The on-disk cache file this snapshot was persisted to, so a clear from
    /// Settings ▸ IntelliSense Cache can be reconciled exactly: when the file is
    /// gone the in-memory entry is dropped too. `None` on wasm (no disk cache)
    /// and for any entry whose disk write failed — both keep the entry.
    pub(crate) cache_file: Option<std::path::PathBuf>,
}

/// Human-readable age of a data-model snapshot for the refresh button's
/// tooltip: the local wall-clock time it was pulled (`yyyy-MM-dd HH:mm`) plus a
/// coarse, plain-language "how long ago".
pub(crate) fn format_snapshot_age(fetched_at: chrono::DateTime<chrono::Utc>) -> String {
    let local = fetched_at.with_timezone(&chrono::Local);
    let stamp = local.format("%Y-%m-%d %H:%M");
    let secs = chrono::Utc::now()
        .signed_duration_since(fetched_at)
        .num_seconds()
        .max(0);
    let ago = if secs < 60 {
        crate::localization::text("shell.schema_age.just_now")
    } else {
        let (count, key) = if secs < 3_600 {
            let minutes = secs / 60;
            (
                minutes,
                if minutes == 1 {
                    "shell.schema_age.minute"
                } else {
                    "shell.schema_age.minutes"
                },
            )
        } else if secs < 86_400 {
            let hours = secs / 3_600;
            (
                hours,
                if hours == 1 {
                    "shell.schema_age.hour"
                } else {
                    "shell.schema_age.hours"
                },
            )
        } else {
            let days = secs / 86_400;
            (
                days,
                if days == 1 {
                    "shell.schema_age.day"
                } else {
                    "shell.schema_age.days"
                },
            )
        };
        crate::localization::format(key, &[("count", &count.to_string())])
    };
    crate::localization::format(
        "shell.schema_age.timestamp",
        &[("timestamp", &stamp.to_string()), ("age", &ago)],
    )
}

impl AppView {
    /// Load the user's IntelliSense rule config from disk (JSONC). Best-effort:
    /// returns the empty default when the file is missing or unparseable so the
    /// editor always has a valid config.
    pub(super) fn load_rule_config() -> IntelliSenseRuleConfig {
        #[cfg(target_arch = "wasm32")]
        return {
            // Browser build: the rule config lives in localStorage.
            match crate::host_storage::load_text(
                crate::host_storage::StorageArea::Config,
                "intellisense-rules.jsonc",
            ) {
                Some(text) => parse_rules(&strip_jsonc(&text))
                    .unwrap_or_else(|_| IntelliSenseRuleConfig::default()),
                None => IntelliSenseRuleConfig::default(),
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let path = match dirs::config_dir() {
                Some(dir) => dir.join("SQLLY").join("intellisense-rules.jsonc"),
                None => return IntelliSenseRuleConfig::default(),
            };
            match std::fs::read_to_string(&path) {
                Ok(text) => parse_rules(&strip_jsonc(&text))
                    .unwrap_or_else(|_| IntelliSenseRuleConfig::default()),
                Err(_) => IntelliSenseRuleConfig::default(),
            }
        }
    }

    /// Build the live completion catalog + procedure snippet list from the
    /// connected database's metadata and push them to every editor tab. Runs
    /// fully async — the UI thread is never blocked on metadata I/O.
    /// Data-model cache key for a profile: its connection identity plus the
    /// database the snapshot describes. A profile with no database resolves to
    /// `master`, matching `refresh_completion_catalog`.
    pub(super) fn datamodel_key(profile: &ConnectionProfile) -> (uuid::Uuid, String) {
        (
            profile.id.0,
            profile
                .database
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "master".to_string()),
        )
    }

    pub(super) fn lru_touch<K: PartialEq + Clone>(order: &mut Vec<K>, key: &K) {
        if order.last() == Some(key) {
            return;
        }
        if let Some(i) = order.iter().position(|k| k == key) {
            let k = order.remove(i);
            order.push(k);
        } else {
            order.push(key.clone());
        }
    }

    fn lru_evict<K, V>(cache: &mut HashMap<K, V>, order: &mut Vec<K>, limit: usize)
    where
        K: PartialEq + Eq + std::hash::Hash + Clone,
    {
        while cache.len() > limit {
            let Some(oldest) = order.first().cloned() else {
                break;
            };
            order.remove(0);
            cache.remove(&oldest);
        }
    }

    pub(super) fn touch_datamodel_cache(&mut self, key: &(uuid::Uuid, String)) {
        Self::lru_touch(&mut self.datamodel_cache_order, key);
    }

    fn insert_datamodel_cache(&mut self, key: (uuid::Uuid, String), value: CachedDataModel) {
        self.datamodel_cache.insert(key.clone(), value);
        Self::lru_touch(&mut self.datamodel_cache_order, &key);
        Self::lru_evict(
            &mut self.datamodel_cache,
            &mut self.datamodel_cache_order,
            super::DATAMODEL_CACHE_LIMIT,
        );
    }

    pub(super) fn insert_value_lookup_cache(
        &mut self,
        key: String,
        values: Vec<sqlly_services::ValueLookupEntry>,
    ) {
        self.value_lookup_cache.insert(key.clone(), values);
        Self::lru_touch(&mut self.value_lookup_cache_order, &key);
        Self::lru_evict(
            &mut self.value_lookup_cache,
            &mut self.value_lookup_cache_order,
            super::VALUE_LOOKUP_CACHE_LIMIT,
        );
    }

    /// Bring IntelliSense onto `profile`'s (server, database) data model. When a
    /// snapshot for that combo is already cached it is applied instantly with no
    /// engine round-trip; otherwise it is fetched in the background and cached
    /// on arrival. `force` skips the cache — the query-bar refresh button passes
    /// it to guarantee a fresh pull.
    pub(super) fn load_data_model(
        &mut self,
        profile: ConnectionProfile,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        // A Redis profile has no schema to pull: its IntelliSense is the
        // command vocabulary plus scanned key names, keyed by database.
        if profile.database_type.is_command_engine() {
            self.connect_redis_intellisense(profile, cx);
            return;
        }
        if !force {
            let key = Self::datamodel_key(&profile);
            if let Some(cached) = self.datamodel_cache.get(&key).cloned() {
                self.touch_datamodel_cache(&key);
                self.apply_completion_state(cached.catalog, cached.procedures, cached.snapshot, cx);
                return;
            }
        }
        if !force {
            // Nothing in memory for this (server, database): whatever catalog
            // is live right now describes a *different* database, and leaving
            // it up while the fetch runs made completions lie after a switch.
            // Clear immediately, then seed from the persistent on-disk cache
            // (near-instant when this combo was ever pulled before) while the
            // background refresh revalidates against the server.
            self.completion_catalog = Arc::new(merge_user_snippets(
                CompletionCatalog::default(),
                &self.snippet_prefs,
            ));
            self.completion_procedures = Arc::new(Vec::new());
            self.completion_snapshot = Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::empty());
            self.push_completion_state_to_editors(cx);
            self.seed_from_disk_cache(profile.clone(), cx);
        }
        self.refresh_completion_catalog(profile, cx);
    }

    /// Connect-time data-model load: seed IntelliSense instantly from the
    /// in-memory cache (or, failing that, the persistent on-disk cache), and
    /// *always* revalidate against the server — a new connection deserves a
    /// fresh pull even when a cached snapshot exists, unlike a mid-session
    /// database switch which trusts the session's cache.
    pub(super) fn connect_data_model(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        if profile.database_type.is_command_engine() {
            self.connect_redis_intellisense(profile, cx);
            return;
        }
        let key = Self::datamodel_key(&profile);
        if let Some(cached) = self.datamodel_cache.get(&key).cloned() {
            self.touch_datamodel_cache(&key);
            self.apply_completion_state(cached.catalog, cached.procedures, cached.snapshot, cx);
        } else {
            // Connecting to a combo this session hasn't pulled: drop whatever
            // catalog a previous connection left live, then seed from disk.
            self.completion_catalog = Arc::new(merge_user_snippets(
                CompletionCatalog::default(),
                &self.snippet_prefs,
            ));
            self.completion_procedures = Arc::new(Vec::new());
            self.completion_snapshot = Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::empty());
            self.push_completion_state_to_editors(cx);
            self.seed_from_disk_cache(profile.clone(), cx);
        }
        self.refresh_completion_catalog(profile, cx);
    }

    /// Install a completion state as the live one: set the canonical fields,
    /// push to every editor tab, and seed the explorer tree from the snapshot
    /// so the whole database is searchable without manual expansion.
    pub(super) fn apply_completion_state(
        &mut self,
        catalog: Arc<CompletionCatalog>,
        procedures: Arc<Vec<ProcedureInfo>>,
        snapshot: Arc<sqlly_lang::sqlmodel::ModelSnapshot>,
        cx: &mut Context<Self>,
    ) {
        self.completion_catalog =
            Arc::new(merge_user_snippets((*catalog).clone(), &self.snippet_prefs));
        self.completion_procedures = procedures;
        self.completion_snapshot = snapshot;
        self.push_completion_state_to_editors(cx);
        self.hydrate_tree_from_active_snapshot(cx);
    }

    /// Reload snippet preferences from disk, rebuild the palette registry, and
    /// remount snippets onto the live completion catalog.
    pub(super) fn apply_snippet_preferences(&mut self, cx: &mut Context<Self>) {
        self.snippet_prefs = crate::snippets::SnippetPreferences::load();
        self.command_registry = crate::command_palette::CommandRegistry::default_registry();
        self.completion_catalog = Arc::new(merge_user_snippets(
            (*self.completion_catalog).clone(),
            &self.snippet_prefs,
        ));
        self.push_completion_state_to_editors(cx);
    }

    /// Seed the sidebar's schema tree for the active (server, database) from
    /// the live completion snapshot. No-op when nothing is loaded yet.
    pub(super) fn hydrate_tree_from_active_snapshot(&mut self, cx: &mut Context<Self>) {
        let Some(profile) = self.active_profile.clone() else {
            return;
        };
        if self.completion_snapshot.is_empty()
            || self.completion_snapshot_epoch != self.schema_change_epoch
        {
            return;
        }
        let (profile_id, database) = Self::datamodel_key(&profile);
        let snapshot = self.completion_snapshot.clone();
        let schemas = self.completion_catalog.schemas.clone();
        self.sidebar.update(cx, |s, cx| {
            s.hydrate_schema_tree(profile_id, &database, &snapshot, &schemas, cx)
        });
    }

    /// Try to seed IntelliSense for `profile`'s (server, database) from the
    /// persistent on-disk data-model cache. Runs the file read off the UI
    /// thread; the result is applied only while that combo is still the active
    /// one and no fresh server pull has landed for it in the meantime.
    pub(super) fn seed_from_disk_cache(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        let key = Self::datamodel_key(&profile);
        let database = key.1.clone();
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            let persisted = {
                let profile = profile.clone();
                let database = database.clone();
                async_cx
                    .background_executor()
                    .spawn(
                        async move { sqlly_services::datamodel_cache::load(&profile, &database) },
                    )
                    .await
            };
            let Some(persisted) = persisted else {
                return;
            };
            let state = sqlly_services::completion_state_from_snapshot(
                persisted.snapshot,
                &database,
                &persisted.schemas,
            );
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    let still_active =
                        this.active_profile.as_ref().map(Self::datamodel_key) == Some(key.clone());
                    // A fresh pull that landed first wins over the disk seed —
                    // it inserted its snapshot into `datamodel_cache`.
                    if !still_active || this.datamodel_cache.contains_key(&key) {
                        return;
                    }
                    this.apply_completion_state(
                        Arc::new(state.catalog),
                        Arc::new(state.procedures),
                        Arc::new(state.snapshot),
                        cx,
                    );
                })
                .ok();
        })
        .detach();
    }

    /// Fold the live snapshot/catalog/procedures back into the cached entry for
    /// the active (server, database), so columns and proc parameters loaded
    /// incrementally on tree expansion survive a switch away and back. No-op
    /// when nothing is cached for the active combo yet.
    pub(super) fn sync_active_datamodel_cache(&mut self) {
        let Some(key) = self.active_profile.as_ref().map(Self::datamodel_key) else {
            return;
        };
        let catalog = self.completion_catalog.clone();
        let procedures = self.completion_procedures.clone();
        let snapshot = self.completion_snapshot.clone();
        if let Some(cached) = self.datamodel_cache.get_mut(&key) {
            cached.catalog = catalog;
            cached.procedures = procedures;
            cached.snapshot = snapshot;
            self.touch_datamodel_cache(&key);
        }
    }

    /// Drop every in-memory data-model snapshot whose on-disk cache file has
    /// been deleted (from Settings ▸ IntelliSense Cache). This keeps a cleared
    /// cache actually cleared: the next visit to that (server, database) finds
    /// nothing cached in memory *or* on disk and does a full server pull,
    /// instead of silently serving the stale session snapshot.
    ///
    /// Deliberately does NOT touch the live completion catalog: if the user
    /// clears the cache for the database they are actively editing, IntelliSense
    /// keeps working from the in-flight snapshot until they switch away — only
    /// the *cached* copy is evicted, so the switch back triggers the refresh.
    /// Entries with no `cache_file` (wasm, or a snapshot whose disk write
    /// failed) are kept; there is no file whose absence could mean "cleared".
    pub(super) fn reconcile_datamodel_cache_with_disk(&mut self) {
        self.datamodel_cache
            .retain(|_, cached| cached.cache_file.as_ref().is_none_or(|path| path.exists()));
        self.datamodel_cache_order
            .retain(|k| self.datamodel_cache.contains_key(k));
    }

    pub(super) fn refresh_completion_catalog(
        &mut self,
        profile: ConnectionProfile,
        cx: &mut Context<Self>,
    ) {
        // Metadata + catalog sync run through the connection's services: a
        // Via-Relay profile pulls its schema over the relay, a Direct one uses
        // the in-process engine exactly as before.
        let services = self.services_for_profile(&profile);
        let database = profile
            .database
            .clone()
            .unwrap_or_else(|| "master".to_string());
        let cache_key = (profile.id.0, database.clone());
        // The exact file the fresh pull will be persisted to (below), captured
        // now so the cached entry can be reconciled against disk later. Mirrors
        // the path `datamodel_cache::save` writes to.
        #[cfg(not(target_arch = "wasm32"))]
        let cache_file = Some(sqlly_services::datamodel_cache::cache_path(
            &profile, &database,
        ));
        #[cfg(target_arch = "wasm32")]
        let cache_file: Option<std::path::PathBuf> = None;
        let pull_epoch = self.schema_change_epoch;
        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        self.datamodel_refreshes_in_flight += 1;
        self.sync_query_builder_catalog(cx);
        cx.spawn(async move |_this, _cx| {
            // Shared metadata->catalog path (same one the LSP uses). Lazy
            // parameter loading: proc params fill in on tree expansion via
            // ProcedureParametersLoaded, so pass load_parameters=false here.
            let result = sqlly_services::build_completion_state(
                services.metadata.as_ref(),
                &profile,
                &database,
                false,
            )
            .await;

            // Keep the validation catalog (per-connection SQLite store used by
            // validate_sql) in lockstep with the live metadata snapshot, so the
            // validator resolves the same objects completion sees. Without this
            // the store only ever holds whatever an engine synced in the past —
            // possibly nothing — and validation reports false "unknown table"s.
            if let Ok(state) = &result {
                // The per-connection validation catalog (a SQLite store) is a
                // native-only feature — `catalog_objects_from_snapshot` and the
                // catalog sync are not built for wasm.
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let objects = sqlly_services::catalog_objects_from_snapshot(&state.snapshot);
                    if !objects.is_empty() {
                        let services = services.clone();
                        let profile = profile.clone();
                        crate::tokio_handle().spawn(async move {
                            if let Err(e) = services
                                .catalog
                                .sync_catalog(
                                    &profile,
                                    objects,
                                    sqlly_services::SyncCoverage::FullSource,
                                )
                                .await
                            {
                                tracing::warn!("validation catalog sync failed: {e}");
                            }
                        });
                    }
                }

                // Persist the fresh pull so the next launch (or the next visit
                // to this database) seeds IntelliSense and the explorer tree
                // instantly instead of starting empty.
                let snapshot = state.snapshot.clone();
                let schemas = state.catalog.schemas.clone();
                let profile = profile.clone();
                let database = database.clone();
                let fetched_at_unix = chrono::Utc::now().timestamp();
                async_cx
                    .background_executor()
                    .spawn(async move {
                        if let Err(e) = sqlly_services::datamodel_cache::save(
                            &profile,
                            &database,
                            &snapshot,
                            &schemas,
                            fetched_at_unix,
                        ) {
                            tracing::warn!("failed to persist data-model cache: {e}");
                        }
                    })
                    .detach();
            }

            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    this.datamodel_refreshes_in_flight =
                        this.datamodel_refreshes_in_flight.saturating_sub(1);
                    match result {
                        Ok(state) => {
                            // Cache the freshly pulled snapshot for this (server,
                            // database) so re-selecting the combo is instant, and
                            // stamp it so the refresh button can show its age.
                            let catalog = Arc::new(state.catalog);
                            let procedures = Arc::new(state.procedures);
                            let snapshot = Arc::new(state.snapshot);
                            this.insert_datamodel_cache(
                                cache_key.clone(),
                                CachedDataModel {
                                    catalog: catalog.clone(),
                                    procedures: procedures.clone(),
                                    snapshot: snapshot.clone(),
                                    fetched_at: chrono::Utc::now(),
                                    cache_file: cache_file.clone(),
                                },
                            );
                            // Apply only while this (server, database) is still the
                            // active one — the user may have switched again while
                            // the pull was in flight, and a stale result must not
                            // clobber the newer database's IntelliSense.
                            if this.active_profile.as_ref().map(Self::datamodel_key)
                                == Some(cache_key)
                            {
                                // A pull that started before the latest schema
                                // change cannot vouch for the tree.
                                if pull_epoch == this.schema_change_epoch {
                                    this.completion_snapshot_epoch = pull_epoch;
                                }
                                this.apply_completion_state(catalog, procedures, snapshot, cx);
                            }
                        }
                        Err(e) => {
                            tracing::warn!("failed to load completion catalog: {e}");
                        }
                    }
                    this.sync_query_builder_catalog(cx);
                })
                .ok();
        })
        .detach();
    }

    /// Hand the live catalog (and whether a pull is still running) to an open
    /// Query Builder, which otherwise keeps the catalog it was opened with.
    pub(super) fn sync_query_builder_catalog(&mut self, cx: &mut Context<Self>) {
        if let Some(view) = self.overlays.query_builder.clone() {
            let catalog = self.completion_catalog.clone();
            let loading = self.datamodel_refreshes_in_flight > 0;
            view.update(cx, |v, cx| v.set_catalog(catalog, loading, cx));
        }
    }

    /// Push the canonical catalog + procedures + rules to every open editor tab
    /// so they all share the connected database's intellisense.
    pub(super) fn push_completion_state_to_editors(&mut self, cx: &mut Context<Self>) {
        let catalog = self.completion_catalog.clone();
        let snapshot = self.completion_snapshot.clone();
        let procedures = self.completion_procedures.clone();
        let rules = self.completion_rules.clone();
        let database_type = self
            .active_profile
            .as_ref()
            .map(|profile| profile.database_type)
            .unwrap_or(sqlly_lang::sqlmodel::DatabaseType::SqlServer);
        for tab in &self.tabs {
            tab.editor.update(cx, |e, cx| {
                e.set_completion_catalog(catalog.clone(), cx);
                e.set_schema_snapshot(snapshot.clone(), cx);
                e.set_completion_procedures(procedures.clone(), cx);
                e.set_completion_rules(rules.clone(), cx);
                e.set_database_type(database_type, cx);
            });
        }
        self.sync_query_builder_catalog(cx);
    }

    /// Reset the completion catalog + procedures to empty on disconnect and push
    /// the cleared state to every editor tab.
    pub(super) fn clear_completion_catalogs(&mut self, cx: &mut Context<Self>) {
        self.completion_catalog = Arc::new(merge_user_snippets(
            CompletionCatalog::default(),
            &self.snippet_prefs,
        ));
        self.completion_procedures = Arc::new(Vec::new());
        self.completion_snapshot = Arc::new(sqlly_lang::sqlmodel::ModelSnapshot::empty());
        self.value_lookup_cache.clear();
        self.value_lookup_cache_order.clear();
        for tab in &self.tabs {
            tab.editor.update(cx, |e, cx| {
                e.set_completion_catalog(self.completion_catalog.clone(), cx);
                e.set_schema_snapshot(self.completion_snapshot.clone(), cx);
                e.set_completion_procedures(self.completion_procedures.clone(), cx);
            });
        }
        self.sync_query_builder_catalog(cx);
    }
}

#[cfg(test)]
mod lru_tests {
    use super::*;
    use std::collections::HashMap;
    use uuid::Uuid;

    fn key(n: u128) -> (Uuid, String) {
        (Uuid::from_u128(n), "db".into())
    }

    #[test]
    fn datamodel_lru_evicts_oldest_beyond_cap() {
        let mut cache = HashMap::new();
        let mut order = Vec::new();
        for i in 0..18u128 {
            let k = key(i);
            cache.insert(k.clone(), i);
            AppView::lru_touch(&mut order, &k);
            AppView::lru_evict(&mut cache, &mut order, 16);
        }
        assert_eq!(cache.len(), 16);
        assert!(!cache.contains_key(&key(0)));
        assert!(!cache.contains_key(&key(1)));
        assert!(cache.contains_key(&key(2)));
        assert!(cache.contains_key(&key(17)));
        assert_eq!(order.len(), 16);
        assert_eq!(order.first(), Some(&key(2)));
        assert_eq!(order.last(), Some(&key(17)));
    }

    #[test]
    fn datamodel_lru_touch_moves_existing_key_to_back() {
        let mut order = vec![key(1), key(2), key(3)];
        AppView::lru_touch(&mut order, &key(1));
        assert_eq!(order, vec![key(2), key(3), key(1)]);
        AppView::lru_touch(&mut order, &key(1));
        assert_eq!(order, vec![key(2), key(3), key(1)]);
    }

    #[test]
    fn value_lookup_lru_evicts_oldest_beyond_cap() {
        let mut cache = HashMap::new();
        let mut order = Vec::new();
        for i in 0..105 {
            let k = format!("col-{i}");
            cache.insert(k.clone(), vec![i.to_string()]);
            AppView::lru_touch(&mut order, &k);
            AppView::lru_evict(&mut cache, &mut order, 100);
        }
        assert_eq!(cache.len(), 100);
        assert!(!cache.contains_key("col-0"));
        assert!(!cache.contains_key("col-4"));
        assert!(cache.contains_key("col-5"));
        assert!(cache.contains_key("col-104"));
        assert_eq!(order.first(), Some(&"col-5".to_string()));
        assert_eq!(order.last(), Some(&"col-104".to_string()));
    }

    #[test]
    fn value_lookup_lru_hit_moves_key_to_back() {
        let mut order = vec!["a".into(), "b".into(), "c".into()];
        AppView::lru_touch(&mut order, &"a".to_string());
        assert_eq!(
            order,
            vec!["b".to_string(), "c".to_string(), "a".to_string()]
        );
    }

    #[test]
    fn merge_user_snippets_seeds_defaults_and_appends_user() {
        let prefs = crate::snippets::SnippetPreferences {
            snippets: vec![crate::snippets::UserSnippet {
                prefix: "sel".into(),
                body: "SELECT ${1:100}".into(),
                description: String::new(),
            }],
            ..Default::default()
        };
        let merged = merge_user_snippets(CompletionCatalog::default(), &prefs);
        assert!(merged.snippets.iter().any(|(p, _)| p == "select-top"));
        assert_eq!(
            merged
                .snippets
                .iter()
                .find(|(p, _)| p == "sel")
                .map(|(_, b)| b.as_str()),
            Some("SELECT ${1:100}")
        );
    }

    #[test]
    fn merge_user_snippets_is_idempotent_and_overrides_builtin_prefix() {
        let prefs = crate::snippets::SnippetPreferences {
            snippets: vec![crate::snippets::UserSnippet {
                prefix: "cte".into(),
                body: "WITH ${1:x} AS (SELECT $0)".into(),
                description: String::new(),
            }],
            ..Default::default()
        };
        let once = merge_user_snippets(CompletionCatalog::default(), &prefs);
        let twice = merge_user_snippets(once.clone(), &prefs);
        assert_eq!(once.snippets, twice.snippets);
        let cte_bodies: Vec<_> = twice
            .snippets
            .iter()
            .filter(|(p, _)| p.eq_ignore_ascii_case("cte"))
            .map(|(_, b)| b.as_str())
            .collect();
        assert_eq!(cte_bodies, vec!["WITH ${1:x} AS (SELECT $0)"]);
    }
}
