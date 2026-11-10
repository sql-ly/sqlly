//! Constructor: builds every input/select/slider entity and its echo-guarded subscription.
//!
//! Split from `preferences_ui.rs`; code moved verbatim.

use super::*;

impl PreferencesWindow {
    pub fn new(store: PreferencesStore, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let tree = build_preferences_tree();
        // Reopen exactly as last closed: restore the saved pane / expansion /
        // scroll state when there is one; otherwise first-run defaults (all
        // top-level groups open, matching macOS System Settings behavior).
        let ui_state = PreferencesUiState::load();
        let mut expanded = HashSet::new();
        match &ui_state {
            Some(state) => {
                expanded.extend(state.expanded.iter().cloned());
            }
            None => {
                for node in &tree {
                    if node.is_folder() {
                        expanded.insert(node.id.clone());
                    }
                }
            }
        }
        let selected_pane = ui_state
            .as_ref()
            .and_then(|s| s.selected_pane.clone())
            .unwrap_or(PreferencesPane::EditorGeneral);
        // The restored pane must be visible: expand every folder above it,
        // even if the saved expansion state had its branch collapsed.
        if let Some(path) = preferences::tree_id_path_to(&tree, &selected_pane) {
            expanded.extend(path);
        }
        let sidebar_scroll = ScrollHandle::new();
        let pane_scroll = ScrollHandle::new();
        if let Some(state) = &ui_state {
            sidebar_scroll.set_offset(point(px(0.), px(state.sidebar_scroll_y)));
            pane_scroll.set_offset(point(px(0.), px(state.pane_scroll_y)));
        }

        // Seed the free-text inputs from the persisted stores so they show the
        // current values the moment the dialog opens. Each input is a stateful
        // `InputState` created once here; its `InputEvent::Change` subscription
        // routes edits into the matching store field (the same logic the old
        // per-render `on_text_change` callbacks carried).
        let rf = store.result_formatting();
        let ext = ExternalEditorPreferencesStore::load(&external_editor_prefs_path());

        let ddl = store.ddl_navigation();
        let sql = store.sql_formatting();
        let fim = store.fim();
        let perf = store.performance();
        let qh = store.query_history();
        let lib = store.sql_library();
        let tx = store.transaction();
        #[cfg(not(target_arch = "wasm32"))]
        let rc = store.result_cache();
        let kubectl_prefs = preferences::KubectlPreferences::load();
        #[cfg(not(target_arch = "wasm32"))]
        let native_tool_prefs = preferences::NativeToolPreferences::load();
        let ai_summary = store.ai_summary();
        let coding_agent = crate::local_cli_agent_config::load_local_cli_agent_config();
        let coding_agent_env = coding_agent
            .kind
            .and_then(|kind| {
                crate::local_cli_agent_config::load_cli_agent_env(kind)
                    .ok()
                    .flatten()
            })
            .unwrap_or_default();

        let mut specs: Vec<(&'static str, String, String, InputApply)> = vec![
            (
                "prefs_search",
                String::new(),
                crate::localization::text("settings.init.placeholder.search"),
                Box::new(|this, v, cx| this.apply_search(v, cx)),
            ),
            (
                "kb-search",
                String::new(),
                crate::localization::text("settings.init.placeholder.search_commands"),
                Box::new(|this, v, cx| this.handle_keybindings_search(v, cx)),
            ),
            (
                "snippet-prefix",
                String::new(),
                "sel".to_string(),
                Box::new(|this, v, cx| this.apply_snippet_prefix(v, cx)),
            ),
            (
                "snippet-description",
                String::new(),
                crate::localization::text("settings.init.placeholder.description"),
                Box::new(|this, v, cx| this.apply_snippet_description(v, cx)),
            ),
            (
                "qa-title",
                String::new(),
                crate::localization::text("settings.init.placeholder.describe_table"),
                Box::new(|this, v, cx| this.apply_quick_title(v, cx)),
            ),
            (
                "ddl_popup_width",
                ddl.popup.width.to_string(),
                "900".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(width) = v.trim().parse::<u32>() {
                        this.set_ddl_field(cx, move |d| d.popup.width = width);
                    }
                }),
            ),
            (
                "ddl_popup_height",
                ddl.popup.height.to_string(),
                "700".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(height) = v.trim().parse::<u32>() {
                        this.set_ddl_field(cx, move |d| d.popup.height = height);
                    }
                }),
            ),
            (
                "perf_spill_threshold",
                perf.spill_row_threshold.to_string(),
                "50000".to_string(),
                Box::new(|this, v, cx| {
                    this.set_performance_usize("perf_spill_threshold", &v, cx, |p, n| {
                        p.spill_row_threshold = n
                    });
                }),
            ),
            (
                "perf_memory_budget",
                perf.result_memory_budget_mib().to_string(),
                "64".to_string(),
                Box::new(|this, v, cx| {
                    this.set_performance_usize("perf_memory_budget", &v, cx, |p, n| {
                        p.set_result_memory_budget_mib(n);
                    });
                }),
            ),
            (
                "perf_window_rows",
                perf.window_rows.to_string(),
                "3000".to_string(),
                Box::new(|this, v, cx| {
                    this.set_performance_usize("perf_window_rows", &v, cx, |p, n| {
                        p.window_rows = n
                    });
                }),
            ),
            (
                "qh_idle_secs",
                qh.idle_seal_secs.to_string(),
                "5".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_idle_secs", &v, cx, |p, n| {
                        p.idle_seal_secs = n as u64
                    });
                }),
            ),
            (
                "tx_idle_secs",
                tx.idle_rollback_secs.to_string(),
                "300".to_string(),
                Box::new(|this, v, cx| {
                    this.set_transaction_usize("tx_idle_secs", &v, cx, |p, n| {
                        p.idle_rollback_secs = n as u64
                    });
                }),
            ),
            (
                "qh_max_versions",
                qh.max_versions.to_string(),
                "100".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_max_versions", &v, cx, |p, n| {
                        p.max_versions = n
                    });
                }),
            ),
            (
                "qh_execution_days",
                qh.execution_retention_days.to_string(),
                "90".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_execution_days", &v, cx, |p, n| {
                        p.execution_retention_days = u32::try_from(n).unwrap_or(u32::MAX)
                    });
                }),
            ),
            (
                "qh_execution_max",
                qh.execution_max_entries.to_string(),
                "10000".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_execution_max", &v, cx, |p, n| {
                        p.execution_max_entries = n
                    });
                }),
            ),
            (
                "qh_version_mib",
                (qh.max_results_bytes_per_version / (1024 * 1024)).to_string(),
                "1".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_version_mib", &v, cx, |p, n| {
                        p.max_results_bytes_per_version = n as u64 * 1024 * 1024
                    });
                }),
            ),
            (
                "qh_tab_mib",
                (qh.max_results_bytes_per_tab / (1024 * 1024)).to_string(),
                "25".to_string(),
                Box::new(|this, v, cx| {
                    this.set_query_history_usize("qh_tab_mib", &v, cx, |p, n| {
                        p.max_results_bytes_per_tab = n as u64 * 1024 * 1024
                    });
                }),
            ),
            (
                "lib_root",
                lib.root.clone().unwrap_or_default(),
                crate::localization::text("settings.init.placeholder.library_folder"),
                Box::new(|this, v, cx| {
                    this.set_sql_library_field(cx, |p| {
                        let trimmed = v.trim();
                        p.root = if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed.to_string())
                        };
                    });
                }),
            ),
            #[cfg(not(target_arch = "wasm32"))]
            (
                "rc_max_rows",
                rc.max_rows.to_string(),
                "10000".to_string(),
                Box::new(|this, v, cx| {
                    this.set_result_cache_usize("rc_max_rows", &v, cx, |p, n| p.max_rows = n);
                }),
            ),
            #[cfg(not(target_arch = "wasm32"))]
            (
                "rc_max_mib",
                (rc.max_bytes / (1024 * 1024)).to_string(),
                "32".to_string(),
                Box::new(|this, v, cx| {
                    this.set_result_cache_usize("rc_max_mib", &v, cx, |p, n| {
                        p.max_bytes = n as u64 * 1024 * 1024
                    });
                }),
            ),
            (
                "perf_page_margin",
                perf.page_margin_rows.to_string(),
                "500".to_string(),
                Box::new(|this, v, cx| {
                    this.set_performance_usize("perf_page_margin", &v, cx, |p, n| {
                        p.page_margin_rows = n
                    });
                }),
            ),
            (
                "perf_page_chunk",
                perf.page_chunk_rows.to_string(),
                "3000".to_string(),
                Box::new(|this, v, cx| {
                    this.set_performance_usize("perf_page_chunk", &v, cx, |p, n| {
                        p.page_chunk_rows = n
                    });
                }),
            ),
            (
                "prefs_ext_name",
                ext.custom_display_name.clone(),
                crate::localization::text("settings.init.placeholder.external_editor_name"),
                Box::new(|this, v, cx| this.update_ext_prefs(cx, |p| p.custom_display_name = v)),
            ),
            (
                "prefs_ext_path",
                ext.selected_resolved_binary_path(|p| std::path::Path::new(p).exists()),
                "/path/to/editor".to_string(),
                Box::new(|this, v, cx| {
                    this.update_ext_prefs(cx, move |p| {
                        let id = p.selected_editor;
                        if id == ExternalEditorId::Other {
                            p.custom_binary_path = v;
                        } else if v.trim().is_empty() {
                            p.path_overrides.remove(id.as_str());
                        } else {
                            p.path_overrides.insert(id.as_str().to_string(), v);
                        }
                    });
                }),
            ),
            (
                "ai_summary_model",
                ai_summary.model.clone(),
                crate::localization::text("settings.init.placeholder.default_model"),
                Box::new(|this, v, cx| {
                    this.set_ai_summary_field(cx, |p| p.model = v);
                }),
            ),
            (
                "agent_max_turns",
                crate::preferences::AgentPreferences::load()
                    .max_turns
                    .to_string(),
                "30".to_string(),
                Box::new(|_this, v, _cx| {
                    if let Ok(max_turns) = v.trim().parse::<usize>() {
                        let mut prefs = crate::preferences::AgentPreferences::load();
                        prefs.max_turns = max_turns;
                        prefs.save();
                    }
                }),
            ),
            (
                "cli_agent_exe",
                coding_agent.exe_path.clone(),
                crate::localization::text("settings.init.placeholder.path_to_claude_or_codex"),
                Box::new(|this, v, cx| {
                    this.coding_agent_cli.config.exe_path = v;
                    cx.notify();
                }),
            ),
            (
                "cli_agent_env",
                coding_agent_env,
                "ANTHROPIC_MODEL=model-name".to_string(),
                Box::new(|this, v, cx| {
                    if let Some(kind) = this.coding_agent_cli.config.kind {
                        let _ = crate::local_cli_agent_config::store_cli_agent_env(kind, &v);
                    }
                    cx.notify();
                }),
            ),
            (
                "cli_agent_extra_args",
                coding_agent.extra_args.clone(),
                "".to_string(),
                Box::new(|this, v, cx| {
                    this.coding_agent_cli.config.extra_args = v;
                    cx.notify();
                }),
            ),
            (
                "cli_agent_max_turns",
                if coding_agent.max_turns == 0 {
                    "30".to_string()
                } else {
                    coding_agent.max_turns.to_string()
                },
                "30".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(max_turns) = v.trim().parse::<usize>() {
                        this.coding_agent_cli.config.max_turns = max_turns;
                        cx.notify();
                    }
                }),
            ),
            (
                "cli_agent_timeout",
                if coding_agent.timeout_minutes == 0 {
                    "10".to_string()
                } else {
                    coding_agent.timeout_minutes.to_string()
                },
                "10".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(minutes) = v.trim().parse::<u64>() {
                        this.coding_agent_cli.config.timeout_minutes = minutes;
                        cx.notify();
                    }
                }),
            ),
            (
                "fim_delay",
                fim.generation_delay_secs.to_string(),
                "0.3".to_string(),
                Box::new(|this, v, cx| {
                    this.set_fim_f64("fim_delay", &v, cx, |p, n| p.generation_delay_secs = n);
                }),
            ),
            (
                "fim_max_ctx",
                fim.max_context_tokens.to_string(),
                "4096".to_string(),
                Box::new(|this, v, cx| {
                    this.set_fim_usize("fim_max_ctx", &v, cx, |p, n| p.max_context_tokens = n);
                }),
            ),
            (
                "fim_suffix",
                fim.suffix_percent.to_string(),
                "0.25".to_string(),
                Box::new(|this, v, cx| {
                    this.set_fim_f64("fim_suffix", &v, cx, |p, n| p.suffix_percent = n);
                }),
            ),
            (
                "fim_temperature",
                fim.temperature.to_string(),
                "0.1".to_string(),
                Box::new(|this, v, cx| {
                    this.set_fim_f64("fim_temperature", &v, cx, |p, n| p.temperature = n);
                }),
            ),
            (
                "fim_max_new",
                fim.max_new_tokens.to_string(),
                "256".to_string(),
                Box::new(|this, v, cx| {
                    this.set_fim_usize("fim_max_new", &v, cx, |p, n| p.max_new_tokens = n);
                }),
            ),
            (
                "sql_line_len",
                sql.max_line_length_raw.to_string(),
                "120".to_string(),
                Box::new(|this, v, cx| {
                    this.set_sql_i32("sql_line_len", &v, cx, |s, n| s.max_line_length_raw = n);
                }),
            ),
            (
                "sql_short_len",
                sql.short_query_max_length_raw.to_string(),
                "80".to_string(),
                Box::new(|this, v, cx| {
                    this.set_sql_i32("sql_short_len", &v, cx, |s, n| {
                        s.short_query_max_length_raw = n
                    });
                }),
            ),
            (
                "prefs_date_format",
                rf.defaults.date_format.format.clone(),
                "yyyy-MM-dd HH:mm:ss".to_string(),
                Box::new(|this, v, cx| {
                    this.set_rf_field(cx, |r| r.defaults.date_format.format = v);
                }),
            ),
            (
                "prefs_date_tz",
                rf.defaults.date_format.time_zone_identifier.clone(),
                "UTC".to_string(),
                Box::new(|this, v, cx| {
                    this.set_rf_field(cx, |r| r.defaults.date_format.time_zone_identifier = v);
                }),
            ),
            (
                "prefs_bool_true",
                rf.defaults.bool_format.custom_true.clone(),
                "true".to_string(),
                Box::new(|this, v, cx| {
                    this.set_rf_field(cx, |r| r.defaults.bool_format.custom_true = v);
                }),
            ),
            (
                "prefs_bool_false",
                rf.defaults.bool_format.custom_false.clone(),
                "false".to_string(),
                Box::new(|this, v, cx| {
                    this.set_rf_field(cx, |r| r.defaults.bool_format.custom_false = v);
                }),
            ),
            (
                "prefs_int_threshold",
                rf.defaults.integer_format.metric_threshold.to_string(),
                "1000".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(n) = v.trim().parse::<i64>() {
                        this.set_rf_field(cx, |r| r.defaults.integer_format.metric_threshold = n);
                    }
                }),
            ),
            (
                "prefs_dec_threshold",
                rf.defaults.decimal_format.metric_threshold.to_string(),
                "1000".to_string(),
                Box::new(|this, v, cx| {
                    if let Ok(n) = v.trim().parse::<i64>() {
                        this.set_rf_field(cx, |r| r.defaults.decimal_format.metric_threshold = n);
                    }
                }),
            ),
            (
                "prefs_override_id",
                String::new(),
                "table.column".to_string(),
                // The identifier is read straight from the input when "Add
                // override" is clicked; nothing to route on each keystroke.
                Box::new(|_this, _v, _cx| {}),
            ),
            (
                "masking_new_pattern",
                String::new(),
                "*phone*".to_string(),
                Box::new(|_this, _v, _cx| {}),
            ),
        ];
        let ai_profiles = crate::cloud_ai::load_ai_profiles();
        let selected_profile = ai_profiles
            .profiles
            .first()
            .map(|profile| profile.id.clone());
        let key_saved: std::collections::HashMap<String, bool> = ai_profiles
            .profiles
            .iter()
            .map(|profile| {
                (
                    profile.id.clone(),
                    crate::cloud_ai::keychain_key_present_ref(&profile.api_key_ref),
                )
            })
            .collect();
        let cloud_specs: Vec<(&'static str, String, String, InputApply)> = vec![
            (
                "ai_profile_name",
                ai_profiles
                    .profiles
                    .first()
                    .map(|p| p.name.clone())
                    .unwrap_or_default(),
                crate::localization::text("settings.init.placeholder.profile_name"),
                Box::new(|this, text, cx| {
                    if let Some(id) = this.cloud_ai.selected.clone() {
                        if let Some(profile) = this
                            .cloud_ai
                            .profiles
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == id)
                        {
                            profile.name = text;
                        }
                        let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                        cx.notify();
                    }
                }),
            ),
            (
                "ai_profile_base_url",
                ai_profiles
                    .profiles
                    .first()
                    .map(|p| p.base_url.clone())
                    .unwrap_or_default(),
                "https://…".to_string(),
                Box::new(|this, text, cx| {
                    if let Some(id) = this.cloud_ai.selected.clone() {
                        if let Some(profile) = this
                            .cloud_ai
                            .profiles
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == id)
                        {
                            profile.base_url = text;
                        }
                        let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                        cx.notify();
                    }
                }),
            ),
            (
                "ai_profile_model",
                ai_profiles
                    .profiles
                    .first()
                    .map(|p| p.model.clone())
                    .unwrap_or_default(),
                crate::localization::text("settings.init.placeholder.model_id"),
                Box::new(|this, text, cx| {
                    if let Some(id) = this.cloud_ai.selected.clone() {
                        if let Some(profile) = this
                            .cloud_ai
                            .profiles
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == id)
                        {
                            profile.model = text;
                        }
                        let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                        cx.notify();
                    }
                }),
            ),
            (
                "ai_profile_key",
                String::new(),
                "sk-…".to_string(),
                Box::new(|this, text, cx| {
                    if let Some(id) = this.cloud_ai.selected.clone() {
                        this.cloud_ai.key_input.insert(id, text);
                        cx.notify();
                    }
                }),
            ),
            (
                "ai_profile_thinking_budget",
                ai_profiles
                    .profiles
                    .first()
                    .and_then(|p| p.thinking_budget.map(|b| b.to_string()))
                    .unwrap_or_default(),
                crate::localization::text("settings.init.placeholder.token_budget"),
                Box::new(|this, text, cx| {
                    if let Some(id) = this.cloud_ai.selected.clone() {
                        if let Some(profile) = this
                            .cloud_ai
                            .profiles
                            .profiles
                            .iter_mut()
                            .find(|p| p.id == id)
                        {
                            profile.thinking_budget = text.trim().parse().ok();
                        }
                        let _ = crate::cloud_ai::save_ai_profiles(&this.cloud_ai.profiles);
                        cx.notify();
                    }
                }),
            ),
        ];
        #[cfg(not(target_arch = "wasm32"))]
        specs.push((
            "kubectl_path",
            kubectl_prefs.kubectl_path.clone().unwrap_or_default(),
            "/opt/homebrew/bin/kubectl".to_string(),
            Box::new(|this, v, cx| this.apply_kubectl_path(v, cx)),
        ));
        #[cfg(not(target_arch = "wasm32"))]
        {
            use sqlly_services::NativeTool;
            let native_rows = [
                (NativeTool::PgDump, "native_tool_pg_dump"),
                (NativeTool::PgRestore, "native_tool_pg_restore"),
                (NativeTool::Psql, "native_tool_psql"),
                (NativeTool::MysqlDump, "native_tool_mysqldump"),
                (NativeTool::Mysql, "native_tool_mysql"),
            ];
            for (tool, id) in native_rows {
                specs.push((
                    id,
                    native_tool_prefs
                        .overrides
                        .get(tool.snake_case())
                        .cloned()
                        .unwrap_or_default(),
                    crate::localization::text("settings.init.placeholder.auto_detect"),
                    Box::new(move |this, v, cx| this.apply_native_tool_path(tool, v, cx)),
                ));
            }
        }
        let decision_config = crate::decision_ai::load_decision_ai_config();
        let decision_key_saved =
            crate::cloud_ai::keychain_key_present_ref(&decision_config.api_key_ref);
        let decision_specs: Vec<(&'static str, String, String, InputApply)> = vec![
            (
                "decision_ai_base_url",
                decision_config.base_url.clone(),
                "https://api.typesafe.ai".to_string(),
                Box::new(|this, text, cx| {
                    this.decision_ai.config.base_url = text;
                    let _ = crate::decision_ai::save_decision_ai_config(&this.decision_ai.config);
                    cx.notify();
                }),
            ),
            (
                "decision_ai_model",
                decision_config.model.clone(),
                "jev-1.13.0".to_string(),
                Box::new(|this, text, cx| {
                    this.decision_ai.config.model = text;
                    let _ = crate::decision_ai::save_decision_ai_config(&this.decision_ai.config);
                    cx.notify();
                }),
            ),
            (
                "decision_ai_key",
                String::new(),
                "sk-…".to_string(),
                Box::new(|this, text, cx| {
                    this.decision_ai.key_input = text;
                    cx.notify();
                }),
            ),
        ];
        specs.extend(cloud_specs);
        specs.extend(decision_specs);

        let mut inputs: std::collections::HashMap<&'static str, FieldInput> =
            std::collections::HashMap::new();
        let mut subscriptions: Vec<Subscription> = Vec::new();
        for (id, initial, placeholder, apply) in specs {
            let field = FieldInput::Single(cx.new(|cx| {
                let key = match id {
                    "prefs_search" => Some("settings.init.placeholder.search"),
                    "kb-search" => Some("settings.init.placeholder.search_commands"),
                    "snippet-description" => Some("settings.init.placeholder.description"),
                    "qa-title" => Some("settings.init.placeholder.describe_table"),
                    "lib_root" => Some("settings.init.placeholder.library_folder"),
                    "prefs_ext_name" => Some("settings.init.placeholder.external_editor_name"),
                    "ai_summary_model" => Some("settings.init.placeholder.default_model"),
                    "cli_agent_exe" => Some("settings.init.placeholder.path_to_claude_or_codex"),
                    "ai_profile_name" => Some("settings.init.placeholder.profile_name"),
                    "ai_profile_model" => Some("settings.init.placeholder.model_id"),
                    "ai_profile_thinking_budget" => Some("settings.init.placeholder.token_budget"),
                    id if id.starts_with("native_tool_") => {
                        Some("settings.init.placeholder.auto_detect")
                    }
                    _ => None,
                };
                let placeholder = key
                    .map(|key| crate::localization::input_placeholder(window, cx, key))
                    .unwrap_or(placeholder);
                InputState::new(window, cx)
                    .placeholder(placeholder)
                    .default_value(initial)
            }));
            subscriptions.push(field.subscribe(
                window,
                cx,
                move |this: &mut Self, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        // Programmatic re-seeds echo a Change too; consume the
                        // reserved echoes instead of re-running `apply`.
                        if let Some(n) = this.suppressed_input_changes.get_mut(id)
                            && *n > 0
                        {
                            *n -= 1;
                            return;
                        }
                        let text = this
                            .inputs
                            .get(id)
                            .map(|field| field.value(cx))
                            .unwrap_or_default();
                        apply(this, text, cx);
                    }
                },
            ));
            inputs.insert(id, field);
        }

        for (id, placeholder, apply) in [
            (
                "snippet-body",
                "SELECT TOP ${1:100} * FROM ${2:table}$0".to_string(),
                Box::new(|this: &mut Self, v: String, cx: &mut Context<Self>| {
                    this.apply_snippet_body(v, cx)
                }) as InputApply,
            ),
            (
                "qa-body",
                "EXEC sp_help '${table}'".to_string(),
                Box::new(|this: &mut Self, v: String, cx: &mut Context<Self>| {
                    this.apply_quick_body(v, cx)
                }) as InputApply,
            ),
        ] {
            let field = FieldInput::Multi(cx.new(|cx| {
                gpui_component::input::TextareaState::new(window, cx)
                    .placeholder(placeholder)
                    .soft_wrap(true)
                    .auto_grow(4, 12)
            }));
            subscriptions.push(field.subscribe(
                window,
                cx,
                move |this: &mut Self, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        if let Some(n) = this.suppressed_input_changes.get_mut(id)
                            && *n > 0
                        {
                            *n -= 1;
                            return;
                        }
                        let text = this
                            .inputs
                            .get(id)
                            .map(|field| field.value(cx))
                            .unwrap_or_default();
                        apply(this, text, cx);
                    }
                },
            ));
            inputs.insert(id, field);
        }

        // DDL popup position sliders: shown as 0–100 percent, stored 0.0–1.0.
        let ddl_popup_x_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value((ddl.popup.x_percent * 100.0).clamp(0.0, 100.0))
        });
        let ddl_popup_y_slider = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(100.)
                .step(1.)
                .default_value((ddl.popup.y_percent * 100.0).clamp(0.0, 100.0))
        });
        subscriptions.push(cx.subscribe_in(
            &ddl_popup_x_slider,
            window,
            |this: &mut Self, _state, event: &SliderEvent, _window, cx| {
                let SliderEvent::Change(value) = event else {
                    return;
                };
                let percent = value.start();
                this.set_ddl_field(cx, move |d| d.popup.x_percent = percent / 100.0);
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &ddl_popup_y_slider,
            window,
            |this: &mut Self, _state, event: &SliderEvent, _window, cx| {
                let SliderEvent::Change(value) = event else {
                    return;
                };
                let percent = value.start();
                this.set_ddl_field(cx, move |d| d.popup.y_percent = percent / 100.0);
            },
        ));

        let _cloud_ai_config = crate::cloud_ai::load_cloud_ai_config();

        // Enumerate all system font names for the font-picker dropdowns.
        let system_fonts = cx.text_system().all_font_names();
        let ai_playground = cx.new(|cx| crate::ai_playground::AiPlayground::new(window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        let mcp_settings = cx.new(|cx| crate::mcp_settings::McpSettingsView::new(window, cx));
        #[cfg(not(target_arch = "wasm32"))]
        cx.subscribe(&mcp_settings, |_prefs, _mcp, event, cx| {
            if matches!(
                event,
                crate::mcp_settings::McpSettingsEvent::OpenMcpActivity
            ) {
                cx.emit(PreferencesWindowEvent::OpenMcpActivity);
            }
        })
        .detach();
        let appearance = store.appearance();
        subscriptions.push(
            cx.observe_global::<crate::localization::LocalizationChanged>(|this, cx| {
                this.tree = build_preferences_tree();
                this.filtered_tree = this.tree.clone();
                this.expand_to_selected();
                this.mark_search_index_stale();
                this.search_dirty = true;
                cx.notify();
            }),
        );

        Self {
            store,
            filtered_tree: tree.clone(),
            tree,
            expanded,
            selected_pane,
            search_query: String::new(),
            search_index: std::collections::HashMap::new(),
            index_stale: true,
            index_warmup_frame_done: false,
            search_dirty: false,
            text_collector: std::cell::RefCell::new(None),
            input_values: Default::default(),
            pane_reveal_pending: std::cell::Cell::new(false),
            pane_match_claimed: std::cell::Cell::new(false),
            sidebar_reveal_pending: std::cell::Cell::new(false),
            select_slots: std::cell::RefCell::new(std::collections::HashMap::new()),
            scheduled_dumps_cache: std::cell::RefCell::new(Default::default()),
            override_text_slots: std::cell::RefCell::new(std::collections::HashMap::new()),
            color_slots: std::cell::RefCell::new(std::collections::HashMap::new()),
            override_kind: ResultKind::Date,
            masking_new_scope: sqlly_core::RuleScope::ColumnName,
            inputs,
            suppressed_input_changes: std::collections::HashMap::new(),
            ddl_popup_x_slider,
            ddl_popup_y_slider,
            _subscriptions: subscriptions,
            cloud_ai: CloudAIState {
                profiles: ai_profiles,
                selected: selected_profile,
                key_input: std::collections::HashMap::new(),
                key_saved,
                status: None,
                fetching: false,
                testing: false,
                fetch_generation: 0,
                test_generation: 0,
                latency: std::collections::HashMap::new(),
                adding_kind: false,
            },
            decision_ai: DecisionAiState {
                config: decision_config,
                key_input: String::new(),
                key_saved: decision_key_saved,
                status: None,
                testing: false,
                test_generation: 0,
            },
            coding_agent_cli: CodingAgentCliState {
                config: crate::local_cli_agent_config::load_local_cli_agent_config(),
                status: None,
                health_detail: None,
                health_ok: None,
                checking: false,
                check_generation: 0,
            },
            ai_runtime_probe: AiRuntimeProbe::Idle,
            ai_runtime_probe_generation: 0,
            ai_playground,
            #[cfg(not(target_arch = "wasm32"))]
            mcp_settings,
            keybindings_model: None,
            keybindings_search: String::new(),
            keybinding_capture: None,
            editor_keybindings_overrides: None,
            keybindings_restart_required: false,
            keybinding_conflict: None,
            system_fonts,
            monitors: crate::ddl_navigation::detect_monitors(cx),
            sidebar_scroll,
            pane_scroll,
            field_errors: std::collections::HashMap::new(),
            did_initial_focus: false,
            previous_focus: None,
            reset_all_armed: false,
            focus_handle: cx.focus_handle(),
            // Fixed size: the two-column split (tree + detail) manages its
            // own scrolling, so the chrome must not auto-size to content.
            frame: crate::dialog::DialogFrame::new(Theme::scaled(900.), Theme::scaled(744.)),
            sql_preview_input: {
                let editor = cx.new(|cx| {
                    let mut editor = crate::editor::SqlEditor::new(cx);
                    editor.set_text(SQL_PREVIEW_SAMPLES[0].1, cx);
                    // A new editor grabs window focus on its first render;
                    // this one is a preview inside a pane that can appear
                    // mid-search-keystroke, where stealing focus kills typing
                    // in the search field.
                    editor.abandon_focus_request();
                    editor
                });
                // Re-render (and thus re-format the preview) on every edit.
                cx.subscribe(&editor, |_this, _editor, event, cx| {
                    if matches!(event, crate::editor::SqlEditorEvent::Change) {
                        cx.notify();
                    }
                })
                .detach();
                editor
            },
            sql_preview_sample: 0,
            intellisense_cache_entries: None,
            cache_clear_all_armed: false,
            #[cfg(not(target_arch = "wasm32"))]
            result_cache_clear_armed: false,
            snippet_prefs: crate::snippets::SnippetPreferences::load(),
            selected_snippet: None,
            selected_quick: None,
            snippet_save_error: None,
            snippet_transfer_status: None,
            // The language actually showing: on the browser build the host
            // page may have switched it away from the saved preference.
            pending_language: crate::localization::current_language(),
            pending_translation_file: appearance.custom_translation_file,
            language_status: None,
            language_error: None,
        }
    }
}
