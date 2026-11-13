//! Headless, UI-agnostic service layer for SQLLY.
//!
//! This crate is the single source of truth for the orchestration logic every
//! frontend shares: connection management, intellisense, query execution, the
//! schema catalog, and metadata/exploration. Frontends consume it through trait
//! **ports** ([`ConnectionManager`], [`IntellisenseService`], [`QueryExecutor`],
//! [`CatalogService`], [`MetadataService`]) and receive **native Rust types** —
//! there is intentionally no serialization on this path.
//!
//! Known adapters over this core:
//! - the GPUI app (`apps/gpui`) — calls [`Services`] directly in-process;
//! - the CLI engine (`apps/cli/src/engine`) — serializes to the binary protocol
//!   through [`DefaultCatalogService`] and the other service ports;
//! - the LSP server (`sqlly-lsp`) — serializes to JSON-RPC only at the wire edge;
//! - a future `sqlly-api` — will serialize to the binary protocol.
//!
//! The default implementations (`Default*`) are the real ones backed by
//! `sqlly-engines`, `sqlly-intel`, and `sqlly-ai`. Do not add a third
//! catalog/validation path — route new work through these ports.
//!
//! The domain modules that used to be crates of their own live here too:
//! `admin` (server administration plans), `alter` (schema-change scripting),
//! `security`, `scripting`, `datacompare`,
//! `cloud_auth`, `fakedb` (the browser build's in-memory backend), and
//! `remote` (the client half of the remote-engine path). `plans` (execution
//! plans) is re-exported from `sqlly-intel`, where it moved so the online
//! plan viewer's wasm build can use it without this crate's native deps.

#![forbid(unsafe_code)]

pub mod admin;
pub mod admin_tools;
#[cfg(not(target_arch = "wasm32"))]
pub mod agent_tools;
pub mod alter;
pub mod catalog;
pub mod cloud_auth;
pub mod connection;
pub mod database_search;
pub mod datacompare;
pub mod datamodel_cache;
pub mod ddl;
pub mod deploy_cart;
#[cfg(not(target_arch = "wasm32"))]
pub mod dump;
pub mod dump_render;
pub mod fakedb;
pub mod identifier_quoting;
pub mod intellisense;
pub mod maintenance;
#[cfg(not(target_arch = "wasm32"))]
pub mod mcp;
pub mod metadata;
pub mod mysql_definer;
#[cfg(not(target_arch = "wasm32"))]
pub mod native_dump;
#[cfg(not(target_arch = "wasm32"))]
pub mod open_requests;
pub mod pg_privileges;
pub use sqlly_intel::plans;
#[cfg(not(target_arch = "wasm32"))]
pub mod providers;
pub mod query;
#[cfg(not(target_arch = "wasm32"))]
pub mod redis_live;
pub mod remote;
pub mod row_editing;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_duckdb;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_engines;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_libsql;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_mysql;
#[cfg(not(target_arch = "wasm32"))]
pub mod row_editing_oracle;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_pg;
pub mod row_editing_sql;
#[cfg(not(target_arch = "wasm32"))]
mod row_editing_sqlite;
pub mod script_runner;
pub mod script_set;
pub mod scripting;
pub mod security;
pub mod server_info;
pub mod services;
pub mod table_snapshot;
#[cfg(not(target_arch = "wasm32"))]
pub mod tds_pool;
pub mod tool_dialects;
#[cfg(not(target_arch = "wasm32"))]
pub mod tool_discovery;
pub mod transfer;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserCapability {
    DataCompare,
    DemoObjectMetadata,
    RowEditing,
    ForeignKeySearch,
    ForeignKeyLabels,
    RelatedRowBrowsing,
}

impl BrowserCapability {
    pub fn unavailable_message(self, served: bool) -> sqlly_core::DisplayMessage {
        let stem = match self {
            Self::DataCompare => "data_compare",
            Self::DemoObjectMetadata => "demo_object_metadata",
            Self::RowEditing => "row_editing",
            Self::ForeignKeySearch => "foreign_key_search",
            Self::ForeignKeyLabels => "foreign_key_labels",
            Self::RelatedRowBrowsing => "related_row_browsing",
        };
        let key = match (stem, served) {
            ("data_compare", false) => "services.browser.unavailable.data_compare_preview",
            ("data_compare", true) => "services.browser.unavailable.data_compare_served",
            ("demo_object_metadata", false) => {
                "services.browser.unavailable.demo_object_metadata_preview"
            }
            ("demo_object_metadata", true) => {
                "services.browser.unavailable.demo_object_metadata_served"
            }
            ("row_editing", false) => "services.browser.unavailable.row_editing_preview",
            ("row_editing", true) => "services.browser.unavailable.row_editing_served",
            ("foreign_key_search", false) => {
                "services.browser.unavailable.foreign_key_search_preview"
            }
            ("foreign_key_search", true) => {
                "services.browser.unavailable.foreign_key_search_served"
            }
            ("foreign_key_labels", false) => {
                "services.browser.unavailable.foreign_key_labels_preview"
            }
            ("foreign_key_labels", true) => {
                "services.browser.unavailable.foreign_key_labels_served"
            }
            ("related_row_browsing", false) => {
                "services.browser.unavailable.related_row_browsing_preview"
            }
            ("related_row_browsing", true) => {
                "services.browser.unavailable.related_row_browsing_served"
            }
            _ => unreachable!(),
        };
        let feature = match self {
            Self::DataCompare => "Data compare",
            Self::DemoObjectMetadata => "Object metadata for the demo database",
            Self::RowEditing => "Row editing",
            Self::ForeignKeySearch => "Foreign-key search",
            Self::ForeignKeyLabels => "Foreign-key labels",
            Self::RelatedRowBrowsing => "Related-row browsing",
        };
        let fallback = if served {
            format!("{feature} is not available when SQLLY runs in a browser")
        } else {
            format!("{feature} is not available in the browser preview — use the desktop app")
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }
}

/// Legacy English wrapper for non-localized frontends. New UI code should
/// resolve [`BrowserCapability::unavailable_message`] at the render boundary.
pub fn browser_unavailable(what: &str, served: bool) -> String {
    if served {
        format!("{what} when SQLLY runs in a browser")
    } else {
        format!("{what} in the browser preview — use the desktop app")
    }
}

#[cfg(test)]
mod browser_unavailable_tests {
    use super::browser_unavailable;

    #[test]
    fn the_preview_points_at_the_desktop_app() {
        assert_eq!(
            browser_unavailable("Data compare is not available", false),
            "Data compare is not available in the browser preview — use the desktop app"
        );
    }

    #[test]
    fn a_served_page_never_mentions_the_preview_or_the_desktop_app() {
        let message = browser_unavailable("Data compare is not available", true);
        assert_eq!(
            message,
            "Data compare is not available when SQLLY runs in a browser"
        );
        assert!(!message.contains("preview"));
        assert!(!message.contains("desktop"));
    }
}

/// Browser (wasm) engine: SQLite/fakedb-backed implementations of the ports
/// plus the open/close entry points the wasm shell calls.
#[cfg(target_arch = "wasm32")]
pub mod browser;

pub use crate::admin::activity::ActivityDialect;
pub use crate::admin::advanced_tools::{
    CsvImportPlan, QueryStoreRank, SchemaObject, TableTarget as AdminTableTarget,
};
pub use crate::admin::backup::BackupType;
pub use crate::admin::dashboard::{Dashboard, DashboardDialect, MetricGroup};
pub use crate::admin::disk_usage::DiskDialect;
pub use crate::admin::index_usage::IndexDialect;
pub use crate::admin::long_tail::{
    DesignerColumn, IndexDesignerModel, RelationshipDesignerModel, TableDesignerModel,
};
pub use crate::admin::schema_plan::{
    ComparedDifference, DeployOptions, DeployPlan, DeployStep, DifferenceStatus,
    RENAME_SIMILARITY_THRESHOLD, RenameCandidate, apply_renames, compare_objects, deploy_script,
    detect_renames, plan_deploy, rollback_script, script_differences,
};
pub use crate::alter::{
    Change, ChangeSet, ColumnDef, DraftColumn, ForeignKeyDef, IndexDef, MysqlAlgorithm, MysqlLock,
    RenderOptions, TableDraft, TableSnapshot, diff, render,
};
pub use crate::scripting::template::{
    ExplorerActionMode, ExplorerActionSpec, ExplorerObjectKind, ScriptTemplate, TemplateExplorer,
    TemplateObjectContext, TemplateOrigin, UserTemplatesFile, explorer_actions_for,
    parameters_from_body,
};
pub use admin_tools::{AdminToolsService, DefaultAdminToolsService};
#[cfg(not(target_arch = "wasm32"))]
pub use agent_tools::ServicesToolHost;
#[cfg(not(target_arch = "wasm32"))]
pub use catalog::{
    CatalogService, DefaultCatalogService, TenantScopePolicy, TenantScopeResolver,
    catalog_objects_from_snapshot, catalog_root, parse_dialect,
};
pub use connection::ConnectionManager;
#[cfg(not(target_arch = "wasm32"))]
pub use connection::DefaultConnectionManager;
pub use database_search::{
    MatchMode, SearchHit, SearchKind, SearchOptions, SearchProgress, SearchSummary, SearchTarget,
    build_table_sql, run_search,
};
pub use ddl::{
    DdlDocument, DdlOptions, DdlSource, DdlSourceKind, fetch_ddl_document, fetch_ddl_document_with,
};
pub use deploy_cart::{
    AddOutcome, DEPLOY_CARTS_VERSION, DeployCart, DeployCartGroup, DeployCartItem, DeployCartsFile,
    DeployObjectKind,
};
#[cfg(not(target_arch = "wasm32"))]
pub use dump::{DumpPhase, DumpProgress, DumpReport, run_database_dump};
pub use dump_render::{DumpContent, DumpLayout, DumpOptions};
pub use intellisense::{
    CompletionContext, CompletionRequest, CompletionResponse, CompletionState, DefaultIntellisense,
    FkRelation, IntelliSenseClause, IntelliSenseIntent, IntelliSenseRule,
    IntelliSenseRuleCondition, IntelliSenseRuleConfig, IntelliSenseRuleKeyboard,
    IntelliSenseTriggerKind, IntellisenseService, PolicyDecision, ProcedureInfo,
    ProcedureParameter, RankingHints, RuleContext, TableRef, ValueContext, ValueLookupEntry,
    ValueOperator, apply_rules, base_type, bracket, build_catalog_from_metadata,
    build_completion_state, build_model_snapshot, complete_hints, completion_state_from_snapshot,
    default_config_text, distinct_values_sql, distinct_values_sql_matching,
    distinct_word_lookup_sql, evaluate_policy, filter_by_policy, fk_values_sql,
    fk_values_sql_matching, format_sql_literal, is_numeric_literal, join_candidates, kind_name,
    matches_partial, merge_columns, merge_columns_into_snapshot, merge_procedure_parameters,
    merge_procedure_params_into_snapshot, parse as parse_rules, placeholder_literal,
    procedures_from_snapshot, should_auto_trigger, strip_jsonc, value_lookup_cache_key,
};
pub use metadata::{
    BulkColumnRow, DefaultMetadataService, ForeignKeyRow, MetadataNode, MetadataService,
};
#[cfg(not(target_arch = "wasm32"))]
pub use native_dump::{
    CommandSpec, Endpoint, NativeDumpRequest, PgFormat, ProgressLine, RestoreKind, RestoreRequest,
    mysql_restore_args, mysqldump_args, parse_mysqldump_extra_args, parse_progress_line,
    pg_dump_args, pg_restore_args, run_native_dump, run_native_restore, sniff_restore_kind,
    validate_mysqldump_extra_args,
};
#[cfg(not(target_arch = "wasm32"))]
pub use query::DefaultQueryExecutor;
pub use query::multi_target::{
    INDEX_STRIDE, MultiTargetEvent, MultiTargetHandle, MultiTargetSpec, TargetOutcome,
    run_multi_target,
};
#[cfg(not(target_arch = "wasm32"))]
pub use query::{
    CancelHook, CancelSlot, register_mysql_cancel, register_oracle_cancel, register_pg_cancel,
    register_sqlite_cancel, register_tds_cancel, run_query_producer,
};
pub use query::{
    CellLimits, QueryEvent, QueryExecutor, QueryHandle, TruncatedCell, TruncatedKind,
    result_memory_budget_bytes, set_result_memory_budget_bytes, set_spill_row_threshold,
    spill_row_threshold, take_spilled_rows, truncate_rows,
};
#[cfg(not(target_arch = "wasm32"))]
pub use row_editing::DefaultRowEditingService;
pub use row_editing::{
    BatchOutcome, BatchRequest, RowEditColumn, RowEditOpenOutcome, RowEditOpenRequest,
    RowEditSession, RowEditingService, StagedBatch, StagedOp, TableTarget, UpdateRowOutcome,
    UpdateRowRequest, build_batch_statements, render_preview_script, render_rollback_script,
};
pub use script_runner::reader::{CHUNK_BYTES, ChunkReader};
#[cfg(not(target_arch = "wasm32"))]
pub use script_runner::run::open_script_file;
pub use script_runner::run::{
    ErrorDecision, OnError, ProgressThrottle, ScriptCancel, ScriptOutcome, ScriptProgress,
    ScriptRunOptions, ScriptUnitFailure, TransactionMode, begin_sql, coalesce_inserts, commit_sql,
    decide_after_error, rollback_sql, run_script,
};
pub use script_set::{
    ScriptSetObject, ScriptSetReport, comment_safe, definition_dependencies, order_for_create,
    order_for_create_live, order_for_create_with_deps, order_for_drop, order_for_drop_with_deps,
    render_create_script, render_create_script_with, render_drop_script, render_drop_script_live,
};
pub use server_info::{IdentifierCase, ServerInfo, collect_server_info};
pub use services::Services;
pub use sqlly_ai::agent::jobs::{JobDefinition, JobStepDefinition};
pub use sqlly_ai::agent::operator::{AlertDefinition, AlertInfo, OperatorDefinition};
pub use sqlly_ai::agent::schedule::ScheduleDefinition;
pub use sqlly_ai::agent::schedule::{describe_schedule, next_run_after};
pub use sqlly_ai::catalog::SyncCoverage;
pub use sqlly_intel::metadata::{ObjectPropertiesSection, ObjectPropertiesSpec};
pub use table_snapshot::{TableShape, load_table_shape, load_table_snapshot};
#[cfg(not(target_arch = "wasm32"))]
pub use tds_pool::{MySqlPool, PgPool, ROW_EDIT_LANE, TdsPool, WarmPool};
pub use tool_dialects::{
    ProcToolDialect, ToolDialects, activity_dialect, dashboard_dialect, disk_dialect,
    index_dialect, plan_dialect, proc_dialect, tool_dialects,
};
#[cfg(not(target_arch = "wasm32"))]
pub use tool_discovery::{
    NativeTool, SqlcmdFlavor, ToolLocation, ToolSource, classify_sqlcmd_output, find_tool,
    locate_all, parse_version_line, probe_sqlcmd_flavor, resolve_paths, well_known_dirs,
};
pub use transfer::plan::{
    BATCH_ROWS, NameCasing, TransferColumn, TransferMode, TransferPlan, TransferStatements,
    TransferTable, apply_casing, create_table_sql, insert_sql, ordered_tables, placeholder,
    qualified_target, rows_per_statement, select_sql, statements_for,
};
#[cfg(not(target_arch = "wasm32"))]
pub use transfer::run::{TransferCancel, TransferOutcome, TransferProgress, run_transfer};
#[cfg(not(target_arch = "wasm32"))]
pub use transfer::sink::{NativeSink, TransferSink, open_sink};

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::intellisense::CompletionCatalog;

    #[test]
    fn intellisense_port_direct_wires_to_native_complete() {
        let svc = Services::new();
        let catalog = CompletionCatalog::default();
        let items = svc.intellisense.complete("SELECT * FROM ", 14, &catalog);
        let _ = items;
    }

    #[test]
    fn default_snippets_port_returns_builtin_snippets() {
        let svc = Services::new();
        let snippets = svc.intellisense.default_snippets();
        assert!(!snippets.is_empty(), "built-in snippets must be populated");
    }
}

#[cfg(test)]
mod presentation_catalog_tests {
    use std::collections::{BTreeMap, BTreeSet};

    fn flatten(
        value: &serde_json::Value,
        prefix: &str,
        entries: &mut BTreeMap<String, (String, String)>,
    ) {
        let object = value.as_object().expect("catalog object");
        if object.len() == 2 && object.contains_key("value") && object.contains_key("location") {
            let text = object["value"].as_str().expect("message value");
            let location = object["location"].as_str().expect("message location");
            assert!(!text.trim().is_empty(), "empty value at {prefix}");
            assert!(!location.trim().is_empty(), "empty location at {prefix}");
            entries.insert(prefix.to_string(), (text.to_string(), location.to_string()));
            return;
        }
        for (name, child) in object {
            assert!(!name.is_empty());
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            flatten(child, &key, entries);
        }
    }

    fn catalog(source: &str) -> BTreeMap<String, (String, String)> {
        let json: serde_json::Value = serde_json::from_str(source).expect("valid catalog JSON");
        let mut entries = BTreeMap::new();
        flatten(&json, "", &mut entries);
        entries
    }

    /// `{{` and `}}` are literal braces (the `format` grammar), so they are
    /// skipped rather than read as the start or end of a placeholder.
    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut found = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find(['{', '}']) {
            rest = &rest[start..];
            if rest.starts_with("{{") || rest.starts_with("}}") {
                rest = &rest[2..];
                continue;
            }
            if rest.starts_with('}') {
                rest = &rest[1..];
                continue;
            }
            let after = &rest[1..];
            let Some(end) = after.find('}') else {
                panic!("unclosed placeholder in {text}");
            };
            found.insert(after[..end].to_string());
            rest = &after[end + 1..];
        }
        found
    }

    #[test]
    fn service_catalogs_have_exact_key_and_placeholder_parity() {
        let english = catalog(include_str!("../../sqlly-i18n/translations/en.json"));
        let spanish = catalog(include_str!("../../sqlly-i18n/translations/es.json"));
        assert!(!english.is_empty());
        assert_eq!(
            english.keys().collect::<Vec<_>>(),
            spanish.keys().collect::<Vec<_>>()
        );
        for (key, (english_text, _)) in &english {
            assert_eq!(
                placeholders(english_text),
                placeholders(&spanish[key].0),
                "placeholder drift at {key}"
            );
        }
    }

    #[test]
    fn descriptors_keep_dynamic_details_out_of_catalog_keys() {
        let error = sqlly_core::StudioError::Connection("server supplied detail".into());
        let message = error.message();
        assert_eq!(message.key(), Some("services.errors.connection"));
        assert_eq!(
            message.arguments().collect::<Vec<_>>(),
            vec![("detail", "server supplied detail")]
        );
    }
}
