use super::*;

impl AppView {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn cache_run_results(
        &mut self,
        results: &Entity<ResultsView>,
        cx: &mut Context<Self>,
    ) {
        if !self.result_cache_prefs.enabled {
            return;
        }
        let Some(tab_idx) = self.tabs.iter().position(|t| &t.results == results) else {
            return;
        };
        let tab = &self.tabs[tab_idx];
        if tab.snapshot_only || tab.ddl_view.is_some() {
            return;
        }
        let sets = results.read(cx).snapshot_sets(cx);
        let engine = results.read(cx).source_db_type().display_name().to_string();
        let query = tab.last_run_sql.clone().unwrap_or_default();
        let completed_unix_ms = chrono::Utc::now().timestamp_millis();
        let existing_key = tab.result_cache_key.clone();
        if sets.is_empty() {
            if let Some(key) = existing_key {
                let store = crate::result_cache::ResultCache::default_store();
                let async_cx = cx.to_async();
                cx.spawn(async move |_this, _cx| {
                    if let Err(error) = async_cx
                        .background_executor()
                        .spawn(async move { store.remove(&key) })
                        .await
                    {
                        tracing::warn!(%error, "failed to remove empty result cache");
                    }
                })
                .detach();
            }
            return;
        }

        let mint_key = existing_key.is_none();
        let key = existing_key.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if mint_key {
            if let Some(tab) = self.tabs.get_mut(tab_idx) {
                tab.result_cache_key = Some(key.clone());
            }
            self.schedule_session_autosave(cx);
        }

        let prefs = self.result_cache_prefs;
        let max_rows = prefs.max_rows();
        let max_bytes = prefs.max_bytes();
        let store = crate::result_cache::ResultCache::default_store();
        let input = crate::result_cache::CacheInput {
            engine,
            query,
            completed_unix_ms,
        };
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let result = async_cx
                .background_executor()
                .spawn(async move { store.store(&key, input, &sets, max_rows, max_bytes) })
                .await;
            if let Err(error) = result {
                tracing::warn!(%error, "failed to cache tab results");
            }
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn remove_cached_results_for_tab(&mut self, tab_idx: usize) {
        let Some(key) = self
            .tabs
            .get(tab_idx)
            .and_then(|tab| tab.result_cache_key.clone())
        else {
            return;
        };
        let store = crate::result_cache::ResultCache::default_store();
        if let Err(error) = store.remove(&key) {
            tracing::warn!(%error, "failed to remove cached results for tab");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn restore_cached_result_sets(&mut self, cx: &mut Context<Self>) {
        if !self.result_cache_prefs.enabled {
            return;
        }
        let loads: Vec<(String, String)> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                tab.result_cache_key
                    .clone()
                    .map(|key| (tab.id.clone(), key))
            })
            .collect();
        for (tab_id, key) in loads {
            let store = crate::result_cache::ResultCache::default_store();
            let weak_self = cx.weak_entity();
            let mut async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let loaded = async_cx
                    .background_executor()
                    .spawn(async move { store.load(&key) })
                    .await;
                let Ok(Some((meta, sets))) = loaded else {
                    if let Err(error) = loaded {
                        tracing::warn!(%error, "failed to load cached tab results");
                    }
                    return;
                };
                weak_self
                    .update(&mut async_cx, |this, cx| {
                        let Some(tab_idx) = this.tab_index_by_id(&tab_id) else {
                            return;
                        };
                        let banner = crate::localization::format(
                            "shell.results.cached_banner",
                            &[(
                                "timestamp",
                                &crate::history_panel::HistoryPanelView::format_time(
                                    meta.completed_unix_ms,
                                ),
                            )],
                        );
                        if let Some(tab) = this.tabs.get(tab_idx) {
                            // The engine the cached results ran on, as the
                            // cache recorded it (the tab may be connected
                            // elsewhere now); unknown when unrecorded.
                            let db = crate::result_cache::engine_from_name(&meta.engine);
                            tab.results.update(cx, |r, cx| {
                                r.set_results_engine(db);
                                r.set_historical_results(banner, sets, cx);
                            });
                        }
                    })
                    .ok();
            })
            .detach();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn purge_orphaned_result_caches(&self) {
        let mut keys = std::collections::HashSet::new();
        for tab in &self.tabs {
            if let Some(key) = &tab.result_cache_key {
                keys.insert(key.clone());
            }
        }
        let session =
            crate::workspace_session::load_session(&crate::workspace_session::session_file());
        for tab in session.tabs {
            if let Some(key) = tab.result_cache_key {
                keys.insert(key);
            }
        }
        let workspace_store = crate::workspace_session::NamedWorkspaceStore::at_default();
        let store_index = workspace_store.load_index();
        for workspace in store_index.workspaces {
            if let Some(session) = workspace_store.load(&workspace.id) {
                for tab in session.tabs {
                    if let Some(key) = tab.result_cache_key {
                        keys.insert(key);
                    }
                }
            }
        }
        let live: Vec<String> = keys.into_iter().collect();
        let cache = crate::result_cache::ResultCache::default_store();
        if let Err(error) = cache.retain_keys(&live) {
            tracing::warn!(%error, "failed to purge orphaned result caches");
        }
    }
}
