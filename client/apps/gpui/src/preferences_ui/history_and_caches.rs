//! Query History, IntelliSense Cache, and Performance panes.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    // ============================================================
    // Performance
    // ============================================================

    // ============================================================
    // Editor > Query History
    // ============================================================

    pub(super) fn render_query_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let qh = self.store.query_history();
        #[cfg(not(target_arch = "wasm32"))]
        let rc = self.store.result_cache();
        let mut children = vec![
            self.heading(&crate::localization::text(
                "settings.history_and_caches.heading.query_history",
            )),
            self.intro(&crate::localization::text(
                "settings.history_and_caches.intro.version_history_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.version_history",
            )),
            self.checkbox(
                "qh_enabled",
                &crate::localization::text(
                    "settings.history_and_caches.checkbox.enable_query_version_history",
                ),
                qh.enabled,
                cx,
                |this, cx| {
                    this.set_query_history_field(cx, |p| p.enabled = !p.enabled);
                },
            ),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.idle_seal_interval_seconds",
                ),
                "qh_idle_secs",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.idle_seal_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.manual_commit_mode",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.idle_rollback_seconds",
                ),
                "tx_idle_secs",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.idle_rollback_hint",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.maximum_versions_per_tab",
                ),
                "qh_max_versions",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.max_versions_eviction",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.execution_history_browser",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.execution_retention_days",
                ),
                "qh_execution_days",
            ),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.maximum_execution_entries",
                ),
                "qh_execution_max",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.retention_cleanup_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.result_storage",
            )),
            self.checkbox(
                "qh_store_results",
                &crate::localization::text(
                    "settings.history_and_caches.checkbox.store_query_results_versions",
                ),
                qh.store_results,
                cx,
                |this, cx| {
                    this.set_query_history_field(cx, |p| p.store_results = !p.store_results);
                },
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.stored_results_hint",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.max_version_results_mib",
                ),
                "qh_version_mib",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.version_size_limit_hint",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.max_tab_results_mib",
                ),
                "qh_tab_mib",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.storage_eviction_hint",
            )),
        ];
        #[cfg(not(target_arch = "wasm32"))]
        {
            children.extend([
                self.section_header(&crate::localization::text(
                    "settings.history_and_caches.section_header.result_cache",
                )),
                self.checkbox(
                    "rc_enabled",
                    &crate::localization::text(
                        "settings.history_and_caches.checkbox.persist_last_results",
                    ),
                    rc.enabled,
                    cx,
                    |this, cx| {
                        this.set_result_cache_field(cx, |p| p.enabled = !p.enabled);
                    },
                ),
                self.hint(&crate::localization::text(
                    "settings.history_and_caches.hint.result_cache_hint",
                )),
                self.input_row(
                    &crate::localization::text(
                        "settings.history_and_caches.input_row.maximum_cached_rows_per_tab",
                    ),
                    "rc_max_rows",
                ),
                self.input_row(
                    &crate::localization::text(
                        "settings.history_and_caches.input_row.max_cached_tab_mib",
                    ),
                    "rc_max_mib",
                ),
            ]);
            let armed = self.result_cache_clear_armed;
            let clear_label = crate::localization::text(if armed {
                "settings.history_and_caches.quiet_button.clear_result_cache_confirm"
            } else {
                "settings.history_and_caches.quiet_button.clear_result_cache"
            });
            children.push(
                self.quiet_button(
                    "clear-result-cache",
                    &clear_label,
                    cx,
                    move |this, _w, cx| {
                        this.clear_result_cache(cx);
                    },
                )
                .into_any_element(),
            );
            if armed {
                children.push(self.warning(&crate::localization::text(
                    "settings.history_and_caches.warning.clear_result_cache_warning",
                )));
            }
        }
        children.extend([
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.git",
            )),
            self.checkbox(
                "qh_git_history",
                &crate::localization::text(
                    "settings.history_and_caches.checkbox.git_local_history",
                ),
                qh.show_git_history,
                cx,
                |this, cx| {
                    this.set_query_history_field(cx, |p| p.show_git_history = !p.show_git_history);
                },
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.git_history_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.lifecycle",
            )),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.lifecycle_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.sql_library",
            )),
            self.input_row(
                &crate::localization::text("settings.history_and_caches.input_row.library_folder"),
                "lib_root",
            ),
        ]);
        children.push(self.quiet_button(
            "lib-choose-folder",
            &crate::localization::text("settings.history_and_caches.quiet_button.choose"),
            cx,
            move |this, window, cx| {
                this.choose_library_folder(window, cx);
            },
        ));
        let lib = self.store.sql_library();
        children.push(self.hint(&crate::localization::text(
            "settings.history_and_caches.hint.library_folder_hint",
        )));
        children.push(self.checkbox(
            "lib_show_hint",
            &crate::localization::text(
                "settings.history_and_caches.checkbox.library_connection_hint",
            ),
            lib.show_front_matter_hint,
            cx,
            |this, cx| {
                this.set_sql_library_field(cx, |p| {
                    p.show_front_matter_hint = !p.show_front_matter_hint
                });
            },
        ));
        self.pane_container(children)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn choose_library_folder(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                crate::localization::text(
                    "settings.history_and_caches.static.choose_library_folder",
                )
                .into(),
            ),
        });
        let async_cx = cx.to_async();
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(folder) = paths.into_iter().next()
            {
                this.update_in(cx, |this, window, cx| {
                    let path = folder.to_string_lossy().to_string();
                    this.set_sql_library_field(cx, |p| p.root = Some(path.clone()));
                    this.set_input_value("lib_root", &path, window, cx);
                })
                .ok();
            }
            drop(async_cx);
        })
        .detach();
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn choose_library_folder(&mut self, _window: &mut Window, _cx: &mut Context<Self>) {}

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn clear_result_cache(&mut self, cx: &mut Context<Self>) {
        if !self.result_cache_clear_armed {
            self.result_cache_clear_armed = true;
            cx.notify();
            return;
        }
        self.result_cache_clear_armed = false;
        if let Err(error) = crate::result_cache::ResultCache::default_store().clear_all() {
            tracing::warn!(%error, "failed to clear result cache");
        }
        cx.notify();
    }

    /// The IntelliSense data-model cache manager. Reveals the on-disk cache
    /// folder, lists every cached (server, database) with its size and age, and
    /// clears caches individually or all at once. Clearing deletes the file and
    /// emits [`PreferencesWindowEvent::IntelliSenseCacheChanged`] so the app
    /// drops the matching in-memory snapshot — the next visit to that database
    /// then rebuilds IntelliSense from a fresh server pull. Takes `&mut self`
    /// (unlike the other bodies) because the pane owns the scanned listing.
    pub(super) fn render_intellisense_cache(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Scan the cache directory once per pane visit — reset to `None` on a
        // clear — rather than once per frame. Servers sort alphabetically and
        // databases within a server alphabetically, so the list stays stable.
        if self.intellisense_cache_entries.is_none() {
            let mut entries = sqlly_services::datamodel_cache::list_entries();
            entries.sort_by(|a, b| {
                cache_server_label(a)
                    .to_lowercase()
                    .cmp(&cache_server_label(b).to_lowercase())
                    .then_with(|| a.database.to_lowercase().cmp(&b.database.to_lowercase()))
            });
            self.intellisense_cache_entries = Some(entries);
        }
        let entries = self.intellisense_cache_entries.clone().unwrap_or_default();
        let total_bytes: u64 = entries.iter().map(|e| e.size_bytes).sum();
        let count = entries.len();

        let mut children: Vec<AnyElement> = vec![self.intro(&crate::localization::text(
            "settings.history_and_caches.intro.schema_cache_intro",
        ))];

        // Reveal (safe) on the left; Clear all (destructive) pushed to the far
        // right so the two are never flush against each other — a misclick on a
        // one-click wipe is exactly what the separation prevents. Clear all
        // itself is armed: the first click warns, the second commits.
        let cache_dir = sqlly_services::datamodel_cache::cache_dir();
        let reveal_dir = cache_dir.clone();
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::scaled(8.)))
            .child(self.quiet_button(
                "reveal-cache-folder",
                &reveal_button_label(),
                cx,
                move |_this, _w, _cx| {
                    if let Err(e) = crate::reveal_path::reveal_in_file_manager(&reveal_dir) {
                        tracing::warn!(
                            error = %e,
                            "{}",
                            crate::localization::format(
                                "settings.intellisense_cache.reveal_failed",
                                &[("error", &e.to_string())],
                            )
                        );
                    }
                },
            ));
        if count > 0 {
            let armed = self.cache_clear_all_armed;
            let clear_all_label = if armed {
                crate::localization::format(
                    "settings.history_and_caches.message.click_again_clear",
                    &[("value", &cache_count_label(count, "cache"))],
                )
            } else {
                crate::localization::text("settings.intellisense_cache.clear_all")
            };
            actions = actions.child(self.quiet_button(
                "clear-all-cache",
                &clear_all_label,
                cx,
                move |this, _w, cx| this.clear_all_caches(cx),
            ));
        }
        children.push(actions.into_any_element());
        if count > 0 && self.cache_clear_all_armed {
            children.push(self.warning(&crate::localization::format(
                "settings.intellisense_cache.clear_all_confirmation",
                &[("count", &cache_count_label(count, "model"))],
            )));
        }

        children.push(self.section_header(&crate::localization::text(
            "settings.history_and_caches.section_header.cached_data_models",
        )));
        if entries.is_empty() {
            children.push(self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.no_cached_models",
            )));
            return self.pane_container(children);
        }

        children.push(self.hint(&crate::localization::format(
            "settings.intellisense_cache.disk_summary",
            &[
                ("count", &cache_count_label(count, "database")),
                (
                    "bytes",
                    &crate::project_file_picker_model::format_bytes(total_bytes),
                ),
            ],
        )));

        let now = chrono::Utc::now().timestamp();
        let mut current_server: Option<String> = None;
        for (ix, entry) in entries.iter().enumerate() {
            let server = cache_server_label(entry);
            if current_server.as_deref() != Some(server.as_str()) {
                children.push(self.label_row(&server));
                current_server = Some(server);
            }
            children.push(self.cache_row(ix, entry, now, cx));
        }

        self.pane_container(children)
    }

    /// One database row in the IntelliSense Cache pane: name + "size · age" on
    /// the left, a Clear button on the right.
    pub(super) fn cache_row(
        &self,
        ix: usize,
        entry: &sqlly_services::datamodel_cache::CachedModelEntry,
        now_unix: i64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let meta = format!(
            "{} · {}",
            crate::project_file_picker_model::format_bytes(entry.size_bytes),
            cache_humanize_age(now_unix, entry.fetched_at_unix),
        );
        let path = entry.path.clone();
        let clear = self.quiet_button(
            ("clear-cache", ix),
            &crate::localization::text("settings.history_and_caches.quiet_button.clear"),
            cx,
            move |this, _w, cx| {
                // A single-row clear also cancels a half-armed "Clear all": the user
                // reached for one row, not the whole list.
                this.cache_clear_all_armed = false;
                if let Err(e) = sqlly_services::datamodel_cache::delete(&path) {
                    tracing::warn!("failed to delete data-model cache {path:?}: {e}");
                }
                this.intellisense_cache_entries = None;
                cx.emit(PreferencesWindowEvent::IntelliSenseCacheChanged);
                cx.notify();
            },
        );
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::scaled(12.)))
            .py(px(Theme::scaled(2.)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .child(
                        div()
                            .text_color(Theme::text())
                            .text_size(px(Theme::font_size()))
                            .child(self.match_text(&entry.database)),
                    )
                    .child(
                        div()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .child(meta),
                    ),
            )
            .child(clear)
            .into_any_element()
    }

    /// Clear every cached data model, behind a two-click arm-then-confirm — the
    /// same guard the keybindings "Reset All" uses, for a comparably rebuildable
    /// but not-free-to-rebuild action. The first click arms (the button relabels
    /// and a warning appears); the second commits.
    pub(super) fn clear_all_caches(&mut self, cx: &mut Context<Self>) {
        if !self.cache_clear_all_armed {
            self.cache_clear_all_armed = true;
            cx.notify();
            return;
        }
        self.cache_clear_all_armed = false;
        for entry in sqlly_services::datamodel_cache::list_entries() {
            if let Err(e) = sqlly_services::datamodel_cache::delete(&entry.path) {
                tracing::warn!("failed to delete data-model cache {:?}: {e}", entry.path);
            }
        }
        self.intellisense_cache_entries = None;
        cx.emit(PreferencesWindowEvent::IntelliSenseCacheChanged);
        cx.notify();
    }

    pub(super) fn render_performance(&self, cx: &mut Context<Self>) -> AnyElement {
        let perf = self.store.performance();
        self.pane_container(vec![
            self.heading(&crate::localization::text(
                "settings.history_and_caches.heading.performance",
            )),
            self.intro(&crate::localization::text(
                "settings.history_and_caches.intro.performance_intro",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.query_timing",
            )),
            self.checkbox(
                "perf_show_timing",
                &crate::localization::text(
                    "settings.history_and_caches.checkbox.detailed_query_timing",
                ),
                perf.show_detailed_query_timing,
                cx,
                |this, cx| {
                    this.set_performance_field(cx, |p| {
                        p.show_detailed_query_timing = !p.show_detailed_query_timing;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.timing_breakdown_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.query_result_optimizations",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.spill_row_threshold",
                ),
                "perf_spill_threshold",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.spill_threshold_hint",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.result_memory_budget_mib",
                ),
                "perf_memory_budget",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.memory_budget_hint",
            )),
            self.input_row(
                &crate::localization::text(
                    "settings.history_and_caches.input_row.resident_rows_around_view",
                ),
                "perf_window_rows",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.resident_rows",
            )),
            self.input_row(
                &crate::localization::text("settings.history_and_caches.input_row.page_margin"),
                "perf_page_margin",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.page_margin_hint",
            )),
            self.input_row(
                &crate::localization::text("settings.history_and_caches.input_row.page_chunk_size"),
                "perf_page_chunk",
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.page_chunk_hint",
            )),
            self.section_header(&crate::localization::text(
                "settings.history_and_caches.section_header.scrolling",
            )),
            self.checkbox(
                "perf_limit_scroll_repaints",
                &crate::localization::text(
                    "settings.history_and_caches.checkbox.limit_scroll_repaints",
                ),
                perf.limit_scroll_repaints,
                cx,
                |this, cx| {
                    this.set_performance_field(cx, |p| {
                        p.limit_scroll_repaints = !p.limit_scroll_repaints;
                    });
                },
            ),
            self.hint(&crate::localization::text(
                "settings.history_and_caches.hint.limit_scroll_repaints_hint",
            )),
        ])
    }
}
