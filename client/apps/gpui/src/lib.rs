//! SQLLY application library: every view, service binding, and the shared
//! UI bootstrap. The native `SQLLY` binary (`main.rs`) and the browser build
//! (`apps/browser`, wasm) both drive this crate, so platform differences stay
//! at the entry points and behind `cfg(target_arch = "wasm32")` seams —
//! the application logic itself is identical on every platform.

mod about;
mod activity_monitor;
mod admin_triage;
mod agent_grant;
mod ai_context;
mod ai_conversations;
mod ai_history_prompt;
mod ai_inline_run;
mod ai_panel;
mod ai_playground;
mod ai_review;
mod ai_templates;
mod app;
#[cfg(not(target_arch = "wasm32"))]
mod app_data_reset;
#[cfg(target_arch = "wasm32")]
#[path = "app_data_reset_web.rs"]
mod app_data_reset;
mod app_store;
mod app_store_edition_dialog;
mod app_update;
mod auto_refresh;
mod azure_discovery_cache;
mod azure_discovery_service;
mod azure_firewall;
mod azure_firewall_flow;
#[cfg(not(target_arch = "wasm32"))]
mod azure_http;
#[cfg(target_arch = "wasm32")]
#[path = "azure_http_web.rs"]
mod azure_http;
#[cfg(not(target_arch = "wasm32"))]
mod azure_keyring;
#[cfg(target_arch = "wasm32")]
#[path = "azure_keyring_web.rs"]
mod azure_keyring;
mod azure_registry;
#[cfg(not(target_arch = "wasm32"))]
mod azure_signin;
#[cfg(target_arch = "wasm32")]
#[path = "azure_signin_web.rs"]
mod azure_signin;
mod azure_sql_token;
mod backup_restore_dialog;
#[cfg(not(target_arch = "wasm32"))]
mod batch_insert_dialog;
#[cfg(target_arch = "wasm32")]
mod browser_download;
mod browser_open;
mod browser_preview;
#[cfg(target_arch = "wasm32")]
mod browser_tab;
mod build_info;
mod capabilities;
mod cell_actions;
mod cell_formatters;
mod cell_hydration;
mod cell_syntax;
mod cell_transform;
mod chrome_tabs;
mod cli_command;
pub mod cloud_ai;
#[cfg(not(target_arch = "wasm32"))]
mod cloud_iam_token;
mod codegen_types;
mod command_palette;
mod computed_columns;
mod conditional_directive;
mod conditional_formatting;
mod config_version;
mod connect_picker;
mod connection;
mod connection_editor;
#[cfg(not(target_arch = "wasm32"))]
mod credential_export;
#[cfg(not(target_arch = "wasm32"))]
mod credential_export_dialog;
#[cfg(not(target_arch = "wasm32"))]
mod credential_import;
#[cfg(not(target_arch = "wasm32"))]
mod database_dialog;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(not(target_arch = "wasm32"))]
mod database_dump_dialog;
#[cfg(not(target_arch = "wasm32"))]
mod database_search_dialog;
pub mod decision_ai;
#[cfg(not(target_arch = "wasm32"))]
mod deep_link_sheet;
mod distribution;
#[cfg(not(target_arch = "wasm32"))]
mod dump_schedule;
#[cfg(target_arch = "wasm32")]
#[path = "dump_schedule_web.rs"]
mod dump_schedule;
#[cfg(not(target_arch = "wasm32"))]
mod kube_discovery;
#[cfg(not(target_arch = "wasm32"))]
mod native_restore_dialog;
mod quick_open;
#[cfg(not(target_arch = "wasm32"))]
mod run_sql_file_dialog;
#[cfg(target_arch = "wasm32")]
#[path = "run_sql_file_dialog_web.rs"]
mod run_sql_file_dialog;
mod sandbox_access;
#[cfg(not(target_arch = "wasm32"))]
mod schema_compare_deploy;
mod self_update;
mod server_dashboard;
#[cfg(not(target_arch = "wasm32"))]
mod tunnel_profiles_dialog;
// `pub` until the app layer hosts the dashboard: the crate's views are only
// reachable from `app.rs`, so a private unreferenced module would be
// dead-code under `-D warnings`. Flip to a private `mod` when wired.
mod background_tasks;
mod binary_ids;
mod bulk_actions;
mod chart_suggest;
mod column_layout;
mod column_profile;
pub mod connection_health;
mod connection_management;
mod count_query;
#[cfg(not(target_arch = "wasm32"))]
mod data_transfer;
mod database_disk_usage;
mod ddl_navigation;
mod ddl_viewer;
mod default_picks;
#[cfg(not(target_arch = "wasm32"))]
mod deploy_cart;
mod designer;
mod redis_key_filters;
mod redis_memory_analyzer;
// `pub` while the app migrates its dialogs onto the unified chrome: the full
// builder/detach surface is typed against before every option has an in-tree
// caller. Flip to a private `mod` once migration no longer leaves gaps.
pub mod dialog;
pub mod editor;
mod editor_groups;
mod editor_keybindings;
mod editor_zoom;
mod engine_form;
mod entity_editing;
mod entity_management;
mod entra_accounts_dialog;
mod entra_grouping;
mod er_diagram;
mod er_export;
mod er_layout;
mod execution_error;
mod execution_plan;
mod execution_plan_diagram;
mod execution_plan_view;
mod explain_options_popover;
mod export_options;
#[cfg(not(target_arch = "wasm32"))]
mod export_stream;
mod external_editor;
#[cfg(not(target_arch = "wasm32"))]
mod feedback_annotate;
mod feedback_tree;
mod feedback_uploader;
mod field_input;
mod filter_lang;
mod fim_completion;
mod find_replace;
mod firewall_rule_dialog;
mod folding;
mod form_controls;
mod format_directives;
mod formatter_picker_dialog;
mod fs_safe;
#[cfg(not(target_arch = "wasm32"))]
mod general_feedback;
#[cfg(not(target_arch = "wasm32"))]
mod general_feedback_dialog;
mod gesture_interpreter;
mod gesture_prefs;
#[cfg(not(target_arch = "wasm32"))]
mod git_panel;
mod grid_bridge;
mod grid_filter_builder;
mod grid_goto_column;
mod grid_sort;
mod grid_transpose;
mod help_content;
mod history_panel;
#[cfg(not(target_arch = "wasm32"))]
mod host_key_dialog;
mod host_storage;
mod host_validator;
#[cfg(not(target_arch = "wasm32"))]
// The worker and the window refresh run on macOS only.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod icloud_sync;
mod icon_catalog;
mod icons;
mod index_analyzer;
#[cfg(not(target_arch = "wasm32"))]
mod library_git;
mod list_tools;
#[cfg(not(target_arch = "wasm32"))]
mod local_cli_agent;
#[cfg(target_arch = "wasm32")]
#[path = "local_cli_agent_web.rs"]
mod local_cli_agent;
mod local_cli_agent_config;
#[cfg(not(target_arch = "wasm32"))]
mod mcp_activity;
#[cfg(not(target_arch = "wasm32"))]
mod mcp_preflight;
#[cfg(not(target_arch = "wasm32"))]
mod mcp_settings;
mod msgpack;
mod notification_center;
mod param_history;
mod query_builder;
mod query_builder_model;
#[cfg(not(target_arch = "wasm32"))]
mod ssh_challenge_dialog;
// The pure half of SQLly Embedded's launch context; only the browser build
// opens launch tabs, native builds compile it for its tests.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod launch_tabs;
mod query_params;
mod query_params_strip;
#[cfg(not(target_arch = "wasm32"))]
mod redis_acl;
#[cfg(not(target_arch = "wasm32"))]
mod redis_bulk;
#[cfg(not(target_arch = "wasm32"))]
mod redis_cluster_topology;
#[cfg(not(target_arch = "wasm32"))]
mod redis_key_actions;
#[cfg(not(target_arch = "wasm32"))]
mod redis_key_dialog;
#[cfg(not(target_arch = "wasm32"))]
mod redis_key_view;
#[cfg(not(target_arch = "wasm32"))]
mod redis_streams;
mod schema_documentation;
mod structure_editor;
mod tenant_scoping;
pub mod test_data_generator;
pub use icon_catalog::AppIcon;
/// The SVG bytes behind one of SQLLY's own `app/*` icon paths, for hosts that
/// assemble their own asset source (the browser build embeds the icon set
/// itself — see `apps/browser`). Named `glyph` for historical parity with the
/// hosts that consult it.
pub use icon_catalog::asset as glyph;
mod check_constraint;
mod fk_autocomplete;
mod fk_labels;
mod formula;
mod grid_directive;
mod import_clipboard;
#[cfg(not(target_arch = "wasm32"))]
mod import_encoding;
mod import_export;
#[cfg(not(target_arch = "wasm32"))]
mod import_export_sheets;
#[cfg(not(target_arch = "wasm32"))]
mod import_profile;
mod intellisense_feedback;
mod intellisense_feedback_dialog;
mod keybindings;
mod license;
mod localization;
pub use localization::{
    apply_host_language, apply_host_overrides, clear_host_overrides, format as translation_format,
    text as translation_text,
};
mod log_console;
mod lsp_debug;
mod maintenance_dialog;
mod map_view;
mod masking_rules;
mod menu_bar;
mod model_codegen;
mod multi_target_dialog;
mod mutation_review;
mod notebook_model;
mod notebook_view;
mod object_browser;
mod object_browser_model;
mod object_dependencies;
mod object_editor;
mod object_editor_model;
mod open_in_spreadsheet;
mod open_source;
mod order_by_actions;
mod pane_divider;
#[cfg(not(target_arch = "wasm32"))]
mod platform_registration;
mod preferences;
mod preferences_ui;
mod procedure_mode;
mod project_file_picker;
mod project_file_picker_model;
mod query_ai_summary;
mod query_format;
mod tab_map;
mod unlock_dialog;
mod window_menu_bar;
mod xlsx_workbook;
// Via-Relay session establishment (native only — depends on the TLS/tokio relay
// client stack). The browser build routes every connection Direct.
mod cell_detail_model;
mod cell_detail_panel;
#[cfg(not(target_arch = "wasm32"))]
mod chart_dashboard;
#[cfg(all(test, not(target_arch = "wasm32")))]
mod clickhouse_live;
mod data_compare;
mod detached_results;
mod directive_completion;
mod paste_rows;
mod pending_changes;
mod query_history;
mod query_history_browser;
#[cfg(not(target_arch = "wasm32"))]
mod query_history_store;
#[cfg(target_arch = "wasm32")]
#[path = "query_history_store_web.rs"]
mod query_history_store;
mod query_store;
mod query_version_history;
#[cfg(not(target_arch = "wasm32"))]
mod remote_session;
#[cfg(not(target_arch = "wasm32"))]
mod result_cache;
mod result_cell_format;
mod result_chart;
mod result_export;
mod result_export_code;
mod result_export_dialog;
mod result_export_xlsx;
mod result_grid_menu;
mod result_header_actions;
mod result_layout;
mod result_lock;
mod result_pivot;
mod result_profiler;
mod result_set_difference;
mod result_set_names;
mod result_snapshots;
mod results;
mod results_pane_state;
mod reveal_path;
mod row_edit_gate;
mod row_editor;
mod row_editor_model;
mod run_picker;
#[cfg(not(target_arch = "wasm32"))]
mod saved_charts;
#[cfg(not(target_arch = "wasm32"))]
mod saved_charts_panel;
mod saved_filters;
mod schema_compare;
pub mod schema_compare_risk;
#[cfg(not(target_arch = "wasm32"))]
mod schema_history;
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
mod schema_snapshots;
mod schema_tree;
#[cfg(not(target_arch = "wasm32"))]
mod screenshot;
mod scrollbars;
mod security_dialog;
mod security_model;
mod selection_stats;
mod service_router;
mod session_stats;
mod settings_snapshot;
#[cfg(not(target_arch = "wasm32"))]
mod sql_credentials;
#[cfg(target_arch = "wasm32")]
#[path = "sql_credentials_web.rs"]
mod sql_credentials;
#[cfg(not(target_arch = "wasm32"))]
mod table_export;
#[cfg(not(target_arch = "wasm32"))]
mod table_export_dialog;
// `servers_explorer` was deleted here. It held a `matches`/`filter_nodes` filter
// and 12 tests for it, commented out of the crate — so the tests never compiled,
// never ran, and reported nothing while looking like coverage. It could not have
// been "re-enabled" to give the explorer a filter either: it filtered
// `sqlly_core::ServersExplorerTree`, a parallel model the live tree never
// builds. The real filter lives on the real tree, in `schema_tree.rs`
// (`SchemaTree::set_filter` / `flatten_tree_filtered`), with tests that run.
mod explorer_search;
mod file_watch;
mod sidebar;
mod snippets;
#[cfg(not(target_arch = "wasm32"))]
mod spill_pager;
#[cfg(target_arch = "wasm32")]
#[path = "spill_pager_web.rs"]
mod spill_pager;
mod sql_agent;
#[cfg(not(target_arch = "wasm32"))]
mod sql_library;
#[cfg(not(target_arch = "wasm32"))]
mod sql_library_model;
pub use sqlly_lang::sqlformat as sql_formatter;
mod protobuf_schema;
mod spatial_canvas;
mod sql_literal;
mod sql_syntax;
mod sql_validation_formatting;
mod sqlly_directive;
mod status_bar;
mod store;
mod store_signin;
mod structured_data;
mod structured_data_viewer;
mod structured_text;
mod tab_history;
mod tab_switcher;
mod table_actions;
mod tabs;
mod template_explorer;
mod terminal_launch;
mod theme;
mod tip_of_the_day;
mod tool_dialog;
mod tool_output;
mod tooltip;
mod top_values;
mod update_status;
mod value_recompress;
#[cfg(target_arch = "wasm32")]
pub(crate) mod wasm_tokio;
mod welcome;
mod where_actions;
mod workspace_dialog;
mod workspace_session;
mod xevents;

use std::borrow::Cow;
use std::sync::OnceLock;

use gpui::{App, KeyBinding};
use sqlly_services::Services;

use crate::app::{AppView, Quit};

#[cfg(not(target_arch = "wasm32"))]
static TOKIO_RT: OnceLock<tokio::runtime::Handle> = OnceLock::new();

#[cfg(not(target_arch = "wasm32"))]
pub fn tokio_handle() -> tokio::runtime::Handle {
    TOKIO_RT
        .get()
        .expect("tokio runtime initialized in main")
        .clone()
}

/// Unit tests have no `main` to install the runtime; give them one. The
/// runtime is intentionally leaked — dropping it would kill the handle the
/// `OnceLock` just published for the rest of the test process.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub(crate) fn ensure_test_tokio_runtime() {
    if TOKIO_RT.get().is_some() {
        return;
    }
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("test tokio runtime");
    let _ = TOKIO_RT.set(rt.handle().clone());
    std::mem::forget(rt);
}

/// Browser build: there is no tokio runtime and none is needed — the browser
/// services are reactor-free futures. The shim handle schedules them on the
/// browser's single-threaded executor with the same call-site shape.
#[cfg(target_arch = "wasm32")]
pub fn tokio_handle() -> wasm_tokio::runtime::Handle {
    wasm_tokio::runtime::Handle
}

/// The application's asset source: SQLLY's own glyphs first, everything else
/// from gpui-component's bundled Lucide set.
///
/// Both live behind one source because gpui resolves every `svg().path(..)`
/// through the single source the `Application` was built with — a second one
/// cannot be registered later.
#[cfg(not(target_arch = "wasm32"))]
struct AppAssets;

#[cfg(not(target_arch = "wasm32"))]
impl gpui::AssetSource for AppAssets {
    fn load(&self, path: &str) -> gpui::Result<Option<std::borrow::Cow<'static, [u8]>>> {
        if let Some(bytes) = icon_catalog::asset(path) {
            return Ok(Some(std::borrow::Cow::Borrowed(bytes)));
        }
        gpui_kit_assets::Assets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<gpui::SharedString>> {
        gpui_kit_assets::Assets.list(path)
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn run_native() {
    // Install the global panic hook first — before any work that could panic.
    // Every panic in any thread or async task is logged to the crash file
    // ($SQLLY_CRASH_LOG or $TMPDIR/sqlly-crash.log) with a backtrace, then the
    // default stderr handler runs as usual.
    sqlly_core::install_panic_hook();

    // Logging goes to stderr and is controlled by RUST_LOG (defaults to `info`).
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(filter)
        .init();

    // gpui uses its own smol-based async executor — it has no tokio runtime.
    // Our services (sqlly-tds, sqlly-services) use tokio I/O directly
    // (tokio::net::TcpStream, tokio::time::timeout, tokio::spawn). Install a
    // multi-threaded tokio runtime and enter it on the main thread so
    // cx.spawn (foreground) tasks have a tokio reactor. Background tasks
    // retrieve the handle via tokio_handle() and call handle.enter().
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");
    let _main_guard = rt.enter();
    TOKIO_RT.set(rt.handle().clone()).ok();

    // A reset requested by the previous process is completed before services
    // or views can reopen and rewrite any persisted state.
    if let Err(error) = app_data_reset::finish_pending_reset() {
        tracing::error!(%error, "could not complete pending fresh-install reset");
        return;
    }
    // Bring older settings files up to the current config version before
    // anything loads them.
    config_version::migrate_settings_files();

    // Mac App Store edition: reopen the files/folders the user granted in
    // earlier runs before anything reads them, then re-verify the in-app
    // purchase. Both are no-ops in the direct-download edition.
    sandbox_access::restore_all();
    app_store::start();
    // App Store apps may not download code: no DuckDB extension downloads.
    if distribution::is_app_store() {
        sqlly_engines::duckdb::set_extension_downloads_allowed(false);
    }

    let (catalog, provider_diagnostics) =
        rt.block_on(sqlly_services::catalog::DefaultCatalogService::from_env());
    if let Some(note) = provider_diagnostics.llm_fallback_note() {
        tracing::warn!(%note);
    }
    if let Some(note) = provider_diagnostics.embedding_fallback_note() {
        tracing::warn!(%note);
    }
    // Explicit opt-in cloud routing for catalog LLM calls (Ask, Generate,
    // schema-card cache): when Settings ▸ AI ▸ Cloud selects a provider as
    // the app AI backend (and its key and model are configured), those
    // calls go to that provider instead of the local runtime. Query
    // summaries, AI review, and FIM keep their own Ollama paths. Any
    // problem falls back to the environment-selected provider with a
    // logged reason — never a silent half-configured cloud path.
    let catalog = match cloud_app_ai_provider(&catalog) {
        Some(cloud) => catalog.with_llm(cloud),
        None => catalog,
    };
    // Per-database tenant-scoping levels (Off by default, Allowed, Warn,
    // Required) flow into every validation, review, and generation call
    // through this resolver.
    let catalog = catalog.with_tenant_scope_resolver(std::sync::Arc::new(|profile| {
        tenant_scoping::policy_for(profile)
    }));
    let services = Services::from_catalog(std::sync::Arc::new(catalog));

    crate::app::install_ssh_host_key_decider();
    preferences::KubectlPreferences::load().apply_override();
    #[cfg(not(target_arch = "wasm32"))]
    {
        rt.spawn(async {
            preferences::refresh_native_tool_locations().await;
        });
        // Explorer custom actions read the user template catalog on every
        // context-menu open; load it and watch it off the UI thread.
        template_explorer::start_user_template_cache();
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        use crate::app::deep_links::{
            STARTUP_DEEP_LINKS, deep_link_candidate, prepare_startup_arg,
        };
        use sqlly_services::open_requests;

        let startup_links: Vec<String> = std::env::args()
            .skip(1)
            .filter(|arg| deep_link_candidate(arg))
            .map(|arg| prepare_startup_arg(&arg))
            .collect();
        if !startup_links.is_empty()
            && open_requests::instance_is_alive(
                std::time::SystemTime::now(),
                std::time::Duration::from_secs(9),
            )
        {
            for raw in &startup_links {
                let _ = open_requests::write_request(&crate::app::deep_links::hand_off_form(raw));
            }
            return;
        }
        if let Ok(mut pending) = STARTUP_DEEP_LINKS.lock() {
            *pending = Some(startup_links);
        }

        if let Err(error) = crate::platform_registration::ensure_url_scheme_registered() {
            tracing::warn!(%error, "could not register sqlly:// URL handler");
        }
    }

    // gpui-component icons (chevrons, check marks, clear buttons, eye toggles)
    // resolve through the application's AssetSource — the companion assets
    // crate embeds the Lucide SVG set the component library references, and
    // [`AppAssets`] layers SQLLY's own glyphs on top of it.
    // `gpui_platform::application()` selects the OS windowing backend; the
    // registry gpui bundled the backends behind `Application::new()`, but the
    // zed git tree splits them into per-platform crates behind this facade.
    let app = gpui_platform::application().with_assets(AppAssets);

    // macOS keeps the app alive with no windows open, so clicking the Dock icon
    // (or picking the app in ⌘-Tab) has to be able to bring one back. Without
    // this the app is running but unreachable: no window, and no way to ask for
    // one except the menu bar.
    {
        let services = services.clone();
        app.on_reopen(move |cx| {
            if AppView::active_app_window(cx).is_none() {
                let _ = AppView::open_window(services.clone(), cx);
            }
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        app.on_open_urls(|urls| {
            for url in urls {
                if url.starts_with("sqlly://")
                    || url.starts_with("file://")
                    || sqlly_core::connect_url::is_raw_connection_uri(&url)
                {
                    // A Finder open is a user grant; keep it past this run.
                    if let Some(path) = url::Url::parse(&url)
                        .ok()
                        .filter(|parsed| parsed.scheme() == "file")
                        .and_then(|parsed| parsed.to_file_path().ok())
                    {
                        crate::sandbox_access::remember(&path);
                    }
                    crate::app::deep_links::queue_open_url(url);
                } else {
                    tracing::debug!(scheme = %url.split(':').next().unwrap_or(""), "ignored open URL");
                }
            }
        });
    }

    app.run(move |cx: &mut App| init_ui(services, cx));
}

/// The cloud LLM provider the user selected as the app AI backend, fully
/// configured (provider + model + Keychain key) — or `None` (log the reason)
/// so the caller keeps the environment-selected local provider.
#[cfg(not(target_arch = "wasm32"))]
fn cloud_app_ai_provider(
    _catalog: &sqlly_services::catalog::DefaultCatalogService,
) -> Option<sqlly_ai::catalog::DynLlmProvider> {
    use sqlly_services::providers::CloudLlmProvider;
    let profiles = cloud_ai::load_ai_profiles();
    let profile_id = profiles.active_profile_id?;
    let profile = profiles
        .profiles
        .iter()
        .find(|profile| profile.id == profile_id)?;
    if profile.model.trim().is_empty() {
        tracing::warn!(
            profile = %profile.name,
            "cloud app-AI backend selected but no model is chosen; staying local"
        );
        return None;
    }
    let key = match cloud_ai::load_api_key_ref(&profile.api_key_ref) {
        Ok(Some(key)) => key,
        Ok(None) if profile.kind == cloud_ai::ProviderKind::OpenAiCompatible => String::new(),
        Ok(None) => {
            tracing::warn!(
                profile = %profile.name,
                "cloud app-AI backend selected but no API key is stored; staying local"
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                profile = %profile.name,
                %error,
                "could not read the cloud API key; staying local"
            );
            return None;
        }
    };
    match cloud_ai::cloud_provider_spec(profile, key).and_then(CloudLlmProvider::new) {
        Ok(cloud) => {
            tracing::info!(
                egress = %cloud_ai::egress_label(profile),
                model = %profile.model,
                "routing app AI through the selected cloud profile"
            );
            Some(std::sync::Arc::new(cloud))
        }
        Err(error) => {
            tracing::warn!(%error, "could not build the cloud AI provider; staying local");
            None
        }
    }
}

/// Everything the application does inside `Application::run`, shared verbatim
/// by the native binary and the browser (wasm) entry point so the two builds
/// cannot drift: fonts, gpui-component/theme initialization, keybindings,
/// action handlers, and the first window.
/// Browser build: point the app at a `sqlly-relay` browser proxy for this
/// page load. Call before [`init_ui`]. `None` keeps the standalone preview.
#[cfg(target_arch = "wasm32")]
pub fn configure_browser_proxy(url: Option<String>, token: Option<String>) {
    app::browser_proxy::configure(url, token);
}

/// Browser build: lets service futures spawned from gpui background workers
/// run on the page's thread. Call on the page's thread before [`init_ui`].
#[cfg(target_arch = "wasm32")]
pub fn install_browser_task_mailbox() {
    wasm_tokio::install_main_thread_mailbox();
}

pub fn init_ui(services: Services, cx: &mut App) {
    localization::activate_saved();
    // The browser build has no `run_native`; this is its first chance to
    // migrate settings (a no-op when native already ran it).
    config_version::migrate_settings_files();
    {
        register_fonts(cx);

        // Initialize gpui-component (theme global, input/menu/select/dialog
        // keybindings and layers) before any window opens, then push the app
        // palette into its global theme so the very first paint is themed.
        gpui_component::init(cx);
        crate::theme::sync_component_theme(cx);

        // The results grid draws its own hover chips on its own schedule.
        // Hand it the app's dwell time so a tooltip over the grid behaves
        // exactly like one over the explorer or the toolbar.
        sqlly_datatable::set_tooltip_show_delay(crate::tooltip::TOOLTIP_SHOW_DELAY);

        editor::register_keybindings(cx);

        // App-level action handlers (Quit, About)
        cx.on_action(|_: &Quit, cx| {
            cx.quit();
        });
        #[cfg(not(target_arch = "wasm32"))]
        {
            cx.on_action(|_: &crate::app::About, cx| {
                about::open_about_window(cx);
            });
            cx.on_action(|_: &crate::app::OpenSourceLicenses, cx| {
                open_source::open_open_source_window(cx);
            });
        }

        // Push the persisted spill threshold and preview memory budget into
        // the query executor before any query can run; kept in sync
        // afterwards by `PreferencesStore::set_performance`.
        let perf = preferences::PerformancePreferences::load();
        sqlly_services::set_spill_row_threshold(perf.spill_row_threshold());
        sqlly_services::set_result_memory_budget_bytes(perf.result_memory_budget_bytes());

        // Native menu bar. Starts in the disconnected layout (Tools ▸ Connect);
        // the workbench reinstalls it with Disconnect once a connection is live.
        cx.set_menus(menu_bar::build_menus(false));

        // Profile and per-command overrides are resolved from the persisted
        // shortcut settings before the first window can receive a key event.
        keybindings::register_app_keybindings(cx);

        #[cfg(not(target_arch = "wasm32"))]
        cx.on_app_quit(|_cx| {
            sqlly_engines::ssh::TunnelManager::global().close_all();
            async {}
        })
        .detach();

        cx.bind_keys([
            KeyBinding::new("cmd-n", crate::app::NewQueryTab, None),
            KeyBinding::new(
                "cmd-shift-enter",
                crate::app::RunAllNotebookCells,
                Some("Notebook"),
            ),
            KeyBinding::new("cmd-shift-n", crate::app::NewWindow, None),
            KeyBinding::new("cmd-o", crate::app::OpenProjectFile, None),
            KeyBinding::new("cmd-s", crate::app::SaveSql, None),
            KeyBinding::new("cmd-shift-r", crate::app::ToggleResultsPane, None),
            KeyBinding::new("cmd-j", crate::app::ToggleResultsPane, None),
            KeyBinding::new("cmd-shift-j", crate::app::ToggleResultsMaximized, None),
            KeyBinding::new("cmd-alt-j", crate::app::DetachResults, None),
            KeyBinding::new("cmd-1", crate::app::SelectOutputResults, None),
            KeyBinding::new("cmd-2", crate::app::SelectOutputMessages, None),
            KeyBinding::new("cmd-3", crate::app::SelectOutputPlan, None),
            KeyBinding::new("cmd-4", crate::app::SelectOutputHistory, None),
            KeyBinding::new("cmd-5", crate::app::SelectOutputDdl, None),
            KeyBinding::new("cmd-6", crate::app::SelectOutputDiff, None),
            KeyBinding::new("cmd-alt-left", crate::app::HistoryPrevVersion, None),
            KeyBinding::new("cmd-alt-right", crate::app::HistoryNextVersion, None),
            KeyBinding::new("cmd-shift-b", crate::app::FocusExplorer, None),
            KeyBinding::new("cmd-shift-f", crate::sidebar::FocusExplorerFilter, None),
            KeyBinding::new("cmd-shift-e", crate::app::OpenExternalEditor, None),
            KeyBinding::new("cmd-k cmd-s", crate::app::OpenKeybindings, None),
            KeyBinding::new("cmd-l", crate::app::ExecutionPlanViewer, None),
            KeyBinding::new("cmd-shift-i", crate::app::ToggleAiPanel, None),
            KeyBinding::new("cmd-shift-l", crate::app::ToggleLogConsole, None),
            KeyBinding::new("cmd-,", crate::app::OpenPreferences, None),
            KeyBinding::new("cmd-m", crate::app::MinimizeWindow, None),
            KeyBinding::new("cmd-=", crate::app::ZoomEditorIn, None),
            KeyBinding::new("cmd-+", crate::app::ZoomEditorIn, None),
            KeyBinding::new("cmd--", crate::app::ZoomEditorOut, None),
            KeyBinding::new("cmd-0", crate::app::ZoomEditorReset, None),
            KeyBinding::new("cmd-q", crate::app::Quit, None),
            KeyBinding::new("escape", crate::app::CancelTabSwitcher, None),
            // Scoped to the Preferences dialog, so it beats the context-less
            // `escape` above. Without a binding, Esc in Preferences is swallowed
            // by `CancelTabSwitcher` and the dialog cannot be dismissed from the
            // keyboard at all.
            KeyBinding::new(
                "escape",
                crate::preferences_ui::ClosePreferences,
                Some(crate::preferences_ui::PREFS_KEY_CONTEXT),
            ),
        ]);

        #[cfg(not(target_os = "macos"))]
        cx.bind_keys([
            KeyBinding::new("ctrl-n", crate::app::NewQueryTab, None),
            KeyBinding::new(
                "ctrl-shift-enter",
                crate::app::RunAllNotebookCells,
                Some("Notebook"),
            ),
            KeyBinding::new("ctrl-shift-n", crate::app::NewWindow, None),
            KeyBinding::new("ctrl-o", crate::app::OpenSql, None),
            KeyBinding::new("ctrl-s", crate::app::SaveSql, None),
            KeyBinding::new("ctrl-shift-r", crate::app::ToggleResultsPane, None),
            KeyBinding::new("ctrl-j", crate::app::ToggleResultsPane, None),
            KeyBinding::new("ctrl-shift-j", crate::app::ToggleResultsMaximized, None),
            KeyBinding::new("ctrl-alt-j", crate::app::DetachResults, None),
            KeyBinding::new("ctrl-1", crate::app::SelectOutputResults, None),
            KeyBinding::new("ctrl-2", crate::app::SelectOutputMessages, None),
            KeyBinding::new("ctrl-3", crate::app::SelectOutputPlan, None),
            KeyBinding::new("ctrl-4", crate::app::SelectOutputHistory, None),
            KeyBinding::new("ctrl-5", crate::app::SelectOutputDdl, None),
            KeyBinding::new("ctrl-6", crate::app::SelectOutputDiff, None),
            KeyBinding::new("ctrl-alt-left", crate::app::HistoryPrevVersion, None),
            KeyBinding::new("ctrl-alt-right", crate::app::HistoryNextVersion, None),
            KeyBinding::new("ctrl-shift-f", crate::sidebar::FocusExplorerFilter, None),
            KeyBinding::new("ctrl-shift-e", crate::app::OpenExternalEditor, None),
            KeyBinding::new("ctrl-k ctrl-s", crate::app::OpenKeybindings, None),
            KeyBinding::new("ctrl-l", crate::app::ExecutionPlanViewer, None),
            KeyBinding::new("ctrl-shift-i", crate::app::ToggleAiPanel, None),
            KeyBinding::new("ctrl-shift-l", crate::app::ToggleLogConsole, None),
            // NOT ctrl-escape: that chord opens the Start menu on Windows, so
            // the binding would be unreachable exactly when a runaway query
            // needs it. (gpui has no Pause/Break key, or SSMS's alt-break
            // would be the natural alias.)
            KeyBinding::new("ctrl-,", crate::app::OpenPreferences, None),
            KeyBinding::new("ctrl-=", crate::app::ZoomEditorIn, None),
            KeyBinding::new("ctrl-+", crate::app::ZoomEditorIn, None),
            KeyBinding::new("ctrl--", crate::app::ZoomEditorOut, None),
            KeyBinding::new("ctrl-0", crate::app::ZoomEditorReset, None),
            KeyBinding::new("ctrl-q", crate::app::Quit, None),
            KeyBinding::new("escape", crate::app::CancelTabSwitcher, None),
            // Same reason as the macOS map above: without this, the global
            // `escape` swallows the key and Preferences can't be dismissed.
            KeyBinding::new(
                "escape",
                crate::preferences_ui::ClosePreferences,
                Some(crate::preferences_ui::PREFS_KEY_CONTEXT),
            ),
        ]);

        let _ = AppView::open_window(services.clone(), cx);

        // Global fallbacks for menu items and app-wide shortcuts. Routed to
        // the active AppView window at dispatch time (see
        // `register_global_actions`) so they keep working after the original
        // window is closed and a secondary window remains — and, for
        // `NewWindow`, when none does.
        AppView::register_global_actions(cx, services.clone());

        // App-level action handlers for the tab switcher. These fire as
        // global fallbacks so ctrl+tab / ctrl+shift+tab / Escape work whenever
        // the application window is focused, not just when a query editor has
        // keyboard focus. Mirrors the Swift `NSEvent.addLocalMonitorForEvents`
        // approach in `QueryWorkspaceController.swift:2675`.
        cx.on_action(move |_: &crate::app::NextEditorTab, cx| {
            AppView::update_active_app_view(cx, |view, _window, cx| {
                view.advance_tab_switcher(crate::tab_switcher::SwitchDirection::Forward, cx);
            });
        });
        cx.on_action(move |_: &crate::app::PreviousEditorTab, cx| {
            AppView::update_active_app_view(cx, |view, _window, cx| {
                view.advance_tab_switcher(crate::tab_switcher::SwitchDirection::Backward, cx);
            });
        });
        cx.on_action(move |_: &crate::app::CancelTabSwitcher, cx| {
            AppView::update_active_app_view(cx, |view, _window, cx| {
                view.cancel_tab_switcher(cx);
            });
        });
        cx.on_action(|_: &crate::sidebar::FocusExplorerFilter, cx| {
            AppView::update_active_app_view(cx, |_view, window, cx| {
                crate::sidebar::focus_active_filter(window, cx);
            });
        });
    }
}

/// Family name of the bundled default editor font (see [`register_fonts`]).
pub const DEFAULT_EDITOR_FONT: &str = "JetBrains Mono";

/// Registers the embedded monospace faces, so the default editor font ships
/// in the binary and never depends on what the machine has installed.
/// JetBrains Mono is the default; Fira Code (the previous default) stays
/// bundled so anyone who picks it again still gets it.
fn register_fonts(cx: &mut App) {
    let fonts: Vec<Cow<'static, [u8]>> = vec![
        Cow::Borrowed(include_bytes!(
            "../resources/fonts/JetBrainsMono-Regular.ttf"
        )),
        Cow::Borrowed(include_bytes!("../resources/fonts/JetBrainsMono-Bold.ttf")),
        Cow::Borrowed(include_bytes!(
            "../resources/fonts/JetBrainsMono-Medium.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../resources/fonts/JetBrainsMono-SemiBold.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../resources/fonts/JetBrainsMono-Italic.ttf"
        )),
        Cow::Borrowed(include_bytes!(
            "../resources/fonts/JetBrainsMono-BoldItalic.ttf"
        )),
        Cow::Borrowed(include_bytes!("../resources/fonts/FiraCode-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../resources/fonts/FiraCode-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../resources/fonts/FiraCode-Medium.ttf")),
        Cow::Borrowed(include_bytes!("../resources/fonts/FiraCode-SemiBold.ttf")),
    ];
    if let Err(e) = cx.text_system().add_fonts(fonts) {
        tracing::warn!("Failed to register bundled editor fonts: {e}");
    }
}
