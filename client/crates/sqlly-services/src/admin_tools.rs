//! Admin tools service — direct in-process SQL generation for backup, restore,
//! activity monitoring, agent jobs, security, designers, import/export, schema
//! compare, query store, extended events, templates, and execution plans.
//!
//! All methods are synchronous (pure SQL generation, no I/O). The generated SQL
//! is returned as a `String` for the UI to display; the user can then execute it
//! via the query executor.

use std::collections::HashMap;

use crate::admin::activity::ActivityMonitor;
use crate::admin::advanced_tools::{
    CsvExportPlan, CsvImportPlan, ExtendedEvents, QueryStore, SchemaObject, StandardReport,
    TableTarget, compare_schema, script_diff, standard_report_query,
};
use crate::admin::backup::{BackupManager, BackupOptions, BackupType};
use crate::admin::dashboard::Dashboard;
use crate::admin::disk_usage::DiskUsage;
use crate::admin::editing::{TableKey, TableRef, select_top_sql};
use crate::admin::index_usage::IndexUsage;
use crate::admin::logs::ErrorLogReader;
use crate::admin::long_tail::{
    DesignerColumn, IndexDesignerModel, RelationshipDesignerModel, TableDesignerModel,
};
use crate::admin::restore::{RestoreManager, RestoreOptions};
use crate::scripting::template::TemplateExplorer;
use crate::security::permissions::{PermissionAction, SecurityCatalog, SecurityWriter};
use sqlly_ai::agent::history::JobHistoryReader;
use sqlly_ai::agent::jobs::{JobBrowser, JobDefinition, JobStepDefinition, JobWriter};
use sqlly_ai::agent::operator::{
    AlertDefinition, AlertInfo, AlertReader, AlertWriter, OperatorDefinition, OperatorReader,
    OperatorWriter,
};
use sqlly_ai::agent::schedule::{ScheduleDefinition, ScheduleReader, ScheduleWriter};

// ---------------------------------------------------------------------------
// DTO for schema compare input (SchemaObject is not Deserialize)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SchemaObjectDto {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub definition: String,
}

impl From<SchemaObjectDto> for SchemaObject {
    fn from(dto: SchemaObjectDto) -> Self {
        Self {
            schema: dto.schema,
            name: dto.name,
            kind: dto.kind,
            definition: dto.definition,
        }
    }
}

// ---------------------------------------------------------------------------
// Trait
// ---------------------------------------------------------------------------

/// SQL generation for admin/ops tools. Every method is synchronous and returns
/// a SQL string (or structured text for templates / connection preview). No I/O
/// — the caller executes the generated SQL via [`crate::query::QueryExecutor`].
pub trait AdminToolsService: Send + Sync {
    // ── Templates ──────────────────────────────────────────────────────────
    fn templates_list(&self) -> String;
    fn templates_search(&self, query: &str) -> String;
    fn templates_render(&self, name: &str, values: &HashMap<String, String>) -> String;

    // ── Backup / Restore ──────────────────────────────────────────────────
    fn backup_sql(
        &self,
        database: &str,
        destination: &str,
        backup_type: BackupType,
        compress: bool,
        checksum: bool,
        copy_only: bool,
    ) -> String;
    fn restore_sql(&self, database: &str, source: &str, replace: bool, recovery: bool) -> String;
    fn restore_filelistonly_sql(&self, source: &str) -> String;
    fn restore_headeronly_sql(&self, source: &str) -> String;
    fn restore_verifyonly_sql(&self, source: &str) -> String;
    fn validate_backup(&self, database: &str, destination: &str) -> Result<(), String>;
    fn validate_restore(&self, database: &str, source: &str) -> Result<(), String>;

    // ── Activity ───────────────────────────────────────────────────────────
    fn activity_query(&self, kind: &str) -> String;
    fn activity_kill_sql(&self, session_id: i32) -> String;
    /// [`Self::activity_query`] in the engine's own catalog language.
    /// `None` when the engine has no live-activity concept (SQLite).
    fn activity_query_for(&self, db: sqlly_core::DatabaseType, kind: &str) -> Option<String>;
    /// [`Self::activity_kill_sql`] per engine (`KILL`, `pg_terminate_backend`,
    /// MySQL `KILL`); `None` where sessions cannot be killed.
    fn activity_kill_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        session_id: i32,
    ) -> Option<String>;
    /// Read-only, engine-native diagnostics for one session. `None` where
    /// live sessions are unsupported.
    fn activity_diagnostic_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        session_id: i32,
    ) -> Option<String>;
    /// Read-only server-health metric groups, each `(name, sql)`. They are run
    /// independently so one unavailable catalog costs only its own tiles.
    /// `None` when the engine has no dashboard support.
    fn dashboard_metric_groups_for(
        &self,
        db: sqlly_core::DatabaseType,
    ) -> Option<Vec<(String, String)>>;
    /// Per-database sizes shown under the dashboard's tiles.
    fn dashboard_sizes_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String>;
    /// Top-queries insight in the engine's own statistics store: Query Store
    /// on SQL Server, `pg_stat_statements` on PostgreSQL, performance-schema
    /// statement digests on MySQL/MariaDB, the shared pool (`V$SQLAREA`) on
    /// Oracle. `None` on engines without one.
    fn query_insights_top(
        &self,
        db: sqlly_core::DatabaseType,
        database: &str,
        limit: u32,
        rank: crate::admin::advanced_tools::QueryStoreRank,
        hours: Option<u32>,
    ) -> Option<String>;
    /// Recent failed queries with the engine's exception text, newest first
    /// (ClickHouse `system.query_log`). `None` where the statistics store
    /// keeps no failure history.
    fn query_insights_failures(
        &self,
        db: sqlly_core::DatabaseType,
        limit: u32,
        hours: Option<u32>,
    ) -> Option<String> {
        let _ = (db, limit, hours);
        None
    }
    /// Script enabling/disabling an agent job (script-first, like the other
    /// agent actions).
    fn agent_set_job_enabled_sql(&self, job_name: &str, enabled: bool) -> String;

    // ── Index / Disk ───────────────────────────────────────────────────────
    fn index_usage_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String>;
    fn index_drop_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        schema: &str,
        table: &str,
        index: &str,
    ) -> Option<String>;
    fn disk_databases_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String>;
    fn disk_files_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String>;

    // ── Error Logs ─────────────────────────────────────────────────────────
    fn logs_query(&self, kind: &str) -> String;

    // ── Table editing ──────────────────────────────────────────────────────
    fn table_select_top(&self, schema: &str, table: &str, rows: u32) -> String;
    fn table_edit_preview(
        &self,
        schema: &str,
        table: &str,
        rows: u32,
        key_columns: &[String],
    ) -> String;

    // ── Query Store ────────────────────────────────────────────────────────
    fn query_store_top(&self, database: &str, limit: u32) -> String;
    fn query_store_plans(&self, database: &str, query_id: i64) -> String;

    // ── Extended Events ────────────────────────────────────────────────────
    fn xevents_sessions_query(&self) -> String;
    fn xevents_session_events_query(&self, session_name: &str) -> String;
    fn xevents_ring_buffer_query(&self, session_name: &str) -> String;

    // ── Standard Reports ──────────────────────────────────────────────────
    fn report_query(&self, report: &str) -> String;

    // ── Import / Export ─────────────────────────────────────────────────────
    fn import_csv_sql(&self, schema: &str, table: &str, file: &str, has_header: bool) -> String;
    fn export_csv_hint(&self, source_sql: &str, file: &str, include_header: bool) -> String;

    // ── SQL Agent ──────────────────────────────────────────────────────────
    fn agent_list_jobs_query(&self) -> String;
    fn agent_start_job_sql(&self, name: &str) -> String;
    fn agent_stop_job_sql(&self, name: &str) -> String;
    fn agent_create_job_sql(&self, name: &str, step_name: &str, command: &str) -> String;
    fn agent_create_job_full_sql(&self, job: &JobDefinition, steps: &[JobStepDefinition])
    -> String;
    fn agent_capability_query(&self) -> String;
    fn agent_history_query(&self, job_id: Option<&str>) -> String;
    fn agent_list_schedules_query(&self) -> String;
    fn agent_list_operators_query(&self) -> String;
    fn agent_list_alerts_query(&self) -> String;
    fn agent_update_job_sql(&self, original_name: &str, job: &JobDefinition) -> String;
    fn agent_upsert_job_step_sql(
        &self,
        job_name: &str,
        step_id: Option<i32>,
        step: &JobStepDefinition,
    ) -> String;
    fn agent_delete_job_sql(&self, job_name: &str) -> String;
    fn agent_create_schedule_definition_sql(&self, schedule: &ScheduleDefinition) -> String;
    fn agent_update_schedule_sql(&self, schedule_id: i32, schedule: &ScheduleDefinition) -> String;
    fn agent_attach_schedule_sql(&self, job_name: &str, schedule_name: &str) -> String;
    fn agent_delete_schedule_sql(&self, schedule_id: i32) -> String;
    fn agent_create_alert_sql(&self, alert: &AlertDefinition) -> String;
    fn agent_update_alert_sql(&self, original_name: &str, alert: &AlertDefinition) -> String;
    fn agent_add_notification_sql(&self, alert_name: &str, operator_name: &str) -> String;
    fn agent_delete_alert_sql(&self, name: &str) -> String;
    fn agent_parse_alert_row(
        &self,
        id: Option<i64>,
        name: Option<&str>,
        enabled: Option<i64>,
        message_id: Option<i64>,
        severity: Option<i64>,
        delay_between_responses: Option<i64>,
    ) -> Option<AlertInfo>;
    fn agent_create_operator_sql(&self, operator: &OperatorDefinition) -> String;
    fn agent_update_operator_sql(
        &self,
        original_name: &str,
        operator: &OperatorDefinition,
    ) -> String;
    fn agent_delete_operator_sql(&self, name: &str) -> String;

    // ── Security ───────────────────────────────────────────────────────────
    fn security_query(&self, kind: &str) -> String;
    fn security_create_login_sql(&self, name: &str, password: &str) -> String;
    fn security_create_user_sql(
        &self,
        user: &str,
        login: Option<&str>,
        schema: Option<&str>,
    ) -> String;
    fn security_alter_role_sql(&self, role: &str, member: &str, add: bool) -> String;
    fn security_permission_sql(
        &self,
        action: &str,
        permission: &str,
        object: &str,
        principal: &str,
    ) -> String;
    fn security_user_databases_query(&self) -> String;
    fn security_server_roles_query(&self) -> String;
    fn security_server_role_memberships_query(&self, login: &str) -> String;
    fn security_login_db_user_mapping_query(&self, login: &str, databases: &[String]) -> String;
    fn security_database_users_with_login_query(&self) -> String;
    fn security_use_database_sql(&self, database: &str, inner_sql: &str) -> String;
    fn security_create_windows_login_sql(&self, name: &str) -> String;
    fn security_alter_login_password_sql(&self, name: &str, new_password: &str) -> String;
    fn security_alter_login_enabled_sql(&self, name: &str, enabled: bool) -> String;
    fn security_alter_server_role_sql(&self, role: &str, member: &str, add: bool) -> String;
    fn security_alter_user_default_schema_sql(&self, user: &str, schema: &str) -> String;
    fn security_drop_user_sql(&self, user: &str) -> String;
    fn security_create_contained_user_sql(
        &self,
        user: &str,
        password: &str,
        schema: Option<&str>,
    ) -> String;
    fn security_alter_user_password_sql(&self, user: &str, new_password: &str) -> String;
    fn security_drop_login_sql(&self, name: &str) -> String;

    /// Per-engine security catalog read. `kind`: "roles" | "role-members" |
    /// "db-connect" | "schema-usage" | "users" | "users-compat" | "databases" |
    /// "tables".
    fn security_query_for(&self, db: sqlly_core::DatabaseType, kind: &str) -> Option<String>;
    fn security_table_grants_query_for(
        &self,
        db: sqlly_core::DatabaseType,
        grantee: &str,
    ) -> Option<String>;
    fn security_show_grants_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        user: &str,
        host: &str,
    ) -> Option<String>;
    /// Batch prefix for database-scoped statements; PG has none (statements are
    /// database-qualified), MySQL uses `USE \`db\`; SQL Server uses `USE [db]`.
    fn security_use_database_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        database: &str,
        inner_sql: &str,
    ) -> String;

    // ── Designers ──────────────────────────────────────────────────────────
    fn designer_create_table_sql(&self, schema: &str, table: &str, columns: &[String]) -> String;
    fn designer_create_index_sql(
        &self,
        schema: &str,
        table: &str,
        name: &str,
        columns: &[String],
        include_columns: &[String],
        unique: bool,
    ) -> String;
    #[allow(clippy::too_many_arguments)]
    fn designer_create_relationship_sql(
        &self,
        name: &str,
        child_schema: &str,
        child_table: &str,
        child_columns: &[String],
        parent_schema: &str,
        parent_table: &str,
        parent_columns: &[String],
    ) -> String;

    // ── Schema Compare ────────────────────────────────────────────────────
    fn schema_compare_script(&self, source_json: &str, target_json: &str) -> String;

    // ── Execution Plans ────────────────────────────────────────────────────
    fn plan_estimated_sql(&self, sql: &str) -> String;
    fn plan_actual_sql(&self, sql: &str) -> String;
    fn plan_summarize(&self, showplan_xml: &str) -> String;
    fn plan_render(&self, showplan_xml: &str) -> String;
}

// ---------------------------------------------------------------------------
// Default implementation — calls the underlying library crates directly
// ---------------------------------------------------------------------------

/// Default [`AdminToolsService`] that delegates to `sqlly-admin`, `sqlly-agent`,
/// `sqlly-security`, `sqlly-scripting`, and `sqlly-plans` library functions.
/// All methods are pure SQL generation — no I/O, no subprocess.
#[derive(Debug, Clone, Default)]
pub struct DefaultAdminToolsService;

impl AdminToolsService for DefaultAdminToolsService {
    // ── Templates ──────────────────────────────────────────────────────────

    fn templates_list(&self) -> String {
        let explorer = TemplateExplorer::with_built_in();
        let mut out = String::new();
        for category in explorer.categories() {
            out.push_str(&category.name);
            out.push('\n');
            for template in &category.templates {
                out.push_str(&format!(
                    "  - {} — {}\n",
                    template.name, template.description
                ));
            }
            out.push('\n');
        }
        out.trim_end().to_string()
    }

    fn templates_search(&self, query: &str) -> String {
        let explorer = TemplateExplorer::with_built_in();
        let results = explorer.search(query);
        if results.is_empty() {
            return format!("No templates matching \"{query}\".");
        }
        results
            .iter()
            .map(|t| format!("{} [{}] — {}", t.name, t.category, t.description))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn templates_render(&self, name: &str, values: &HashMap<String, String>) -> String {
        let explorer = TemplateExplorer::with_built_in();
        match explorer.find(name) {
            Some(template) => template.render(values),
            None => format!("Template \"{name}\" not found."),
        }
    }

    // ── Backup / Restore ──────────────────────────────────────────────────

    fn backup_sql(
        &self,
        database: &str,
        destination: &str,
        backup_type: BackupType,
        compress: bool,
        checksum: bool,
        copy_only: bool,
    ) -> String {
        let options = BackupOptions {
            database: database.to_string(),
            backup_type,
            destination: destination.to_string(),
            compress,
            checksum,
            copy_only,
            description: None,
        };
        BackupManager::generate_backup_sql(&options)
    }

    fn restore_sql(&self, database: &str, source: &str, replace: bool, recovery: bool) -> String {
        let options = RestoreOptions {
            database: database.to_string(),
            source: source.to_string(),
            replace,
            recovery,
            file_relocation: Vec::new(),
            point_in_time: None,
        };
        RestoreManager::generate_restore_sql(&options)
    }

    fn restore_filelistonly_sql(&self, source: &str) -> String {
        RestoreManager::filelistonly_query(source)
    }

    fn restore_headeronly_sql(&self, source: &str) -> String {
        RestoreManager::headeronly_query(source)
    }

    fn restore_verifyonly_sql(&self, source: &str) -> String {
        RestoreManager::restore_verifyonly_query(source)
    }

    fn validate_backup(&self, database: &str, destination: &str) -> Result<(), String> {
        BackupManager::validate(&BackupOptions {
            database: database.to_string(),
            backup_type: BackupType::Full,
            destination: destination.to_string(),
            compress: false,
            checksum: false,
            copy_only: false,
            description: None,
        })
    }

    fn validate_restore(&self, database: &str, source: &str) -> Result<(), String> {
        RestoreManager::validate(&RestoreOptions {
            database: database.to_string(),
            source: source.to_string(),
            replace: false,
            recovery: true,
            file_relocation: Vec::new(),
            point_in_time: None,
        })
    }

    // ── Activity ───────────────────────────────────────────────────────────

    fn activity_query(&self, kind: &str) -> String {
        match kind {
            "sessions" => ActivityMonitor::sessions_query().to_string(),
            "blocking" => ActivityMonitor::blocking_query().to_string(),
            "expensive" => ActivityMonitor::expensive_queries_query().to_string(),
            "waits" => ActivityMonitor::waits_query().to_string(),
            other => format!(
                "-- Unknown activity kind \"{other}\". Valid: sessions, blocking, expensive, waits."
            ),
        }
    }

    fn activity_kill_sql(&self, session_id: i32) -> String {
        ActivityMonitor::kill_session_sql(session_id)
    }

    fn activity_query_for(&self, db: sqlly_core::DatabaseType, kind: &str) -> Option<String> {
        let dialect = crate::tool_dialects::activity_dialect(db)?;
        Some(match kind {
            "blocking" => ActivityMonitor::blocking_query_for(dialect).to_string(),
            "expensive" => ActivityMonitor::expensive_queries_query_for(dialect).to_string(),
            "waits" => ActivityMonitor::waits_query_for(dialect).to_string(),
            _ => ActivityMonitor::sessions_query_for(dialect).to_string(),
        })
    }

    fn activity_kill_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        session_id: i32,
    ) -> Option<String> {
        Some(ActivityMonitor::kill_session_sql_for(
            crate::tool_dialects::activity_dialect(db)?,
            session_id,
        ))
    }

    fn activity_diagnostic_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        session_id: i32,
    ) -> Option<String> {
        Some(ActivityMonitor::diagnostic_sql_for(
            crate::tool_dialects::activity_dialect(db)?,
            session_id,
        ))
    }

    fn dashboard_metric_groups_for(
        &self,
        db: sqlly_core::DatabaseType,
    ) -> Option<Vec<(String, String)>> {
        let dialect = crate::tool_dialects::dashboard_dialect(db)?;
        Some(
            Dashboard::metric_groups_for(dialect)
                .iter()
                .map(|group| (group.name.to_string(), group.sql.to_string()))
                .collect(),
        )
    }

    fn dashboard_sizes_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String> {
        let dialect = crate::tool_dialects::dashboard_dialect(db)?;
        Some(Dashboard::database_sizes_query_for(dialect).to_string())
    }

    fn query_insights_top(
        &self,
        db: sqlly_core::DatabaseType,
        database: &str,
        limit: u32,
        rank: crate::admin::advanced_tools::QueryStoreRank,
        hours: Option<u32>,
    ) -> Option<String> {
        use sqlly_core::ConnectionKind;
        match db.connection_kind() {
            ConnectionKind::Tds => Some(QueryStore::top_queries_query_ranked(
                database, limit, rank, hours,
            )),
            ConnectionKind::PostgresWire => {
                Some(QueryStore::pg_stat_statements_top_query(limit, rank))
            }
            ConnectionKind::MySqlWire => {
                Some(QueryStore::mysql_statement_digest_top_query(limit, rank))
            }
            ConnectionKind::OracleNet => Some(QueryStore::oracle_top_query(limit, rank)),
            ConnectionKind::ClickHouseHttp => Some(QueryStore::clickhouse_query_log_top_query(
                limit, rank, hours,
            )),
            // Redis: the slow log is the only per-command history; the
            // driver shapes it as query_id / query_text / duration columns.
            ConnectionKind::Resp => Some(format!("SLOWLOG GET {}", limit.clamp(1, 128))),
            _ => None,
        }
    }

    fn query_insights_failures(
        &self,
        db: sqlly_core::DatabaseType,
        limit: u32,
        hours: Option<u32>,
    ) -> Option<String> {
        use sqlly_core::ConnectionKind;
        match db.connection_kind() {
            ConnectionKind::ClickHouseHttp => Some(
                QueryStore::clickhouse_query_log_failures_query(limit, hours),
            ),
            _ => None,
        }
    }

    fn agent_set_job_enabled_sql(&self, job_name: &str, enabled: bool) -> String {
        sqlly_ai::agent::jobs::JobWriter::set_job_enabled_sql(job_name, enabled)
    }

    // ── Index / Disk ───────────────────────────────────────────────────────

    fn index_usage_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String> {
        Some(IndexUsage::usage_query_for(crate::tool_dialects::index_dialect(db)?).to_string())
    }

    fn index_drop_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        schema: &str,
        table: &str,
        index: &str,
    ) -> Option<String> {
        Some(IndexUsage::drop_index_sql_for(
            crate::tool_dialects::index_dialect(db)?,
            schema,
            table,
            index,
        ))
    }

    fn disk_databases_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String> {
        Some(DiskUsage::databases_query_for(crate::tool_dialects::disk_dialect(db)?).to_string())
    }

    fn disk_files_query_for(&self, db: sqlly_core::DatabaseType) -> Option<String> {
        DiskUsage::files_query_for(crate::tool_dialects::disk_dialect(db)?).map(str::to_string)
    }

    // ── Error Logs ─────────────────────────────────────────────────────────

    fn logs_query(&self, kind: &str) -> String {
        match kind {
            "current" => ErrorLogReader::current_log_query().to_string(),
            "agent" => ErrorLogReader::agent_log_query().to_string(),
            other => format!("-- Unknown log kind \"{other}\". Valid: current, agent."),
        }
    }

    // ── Table editing ──────────────────────────────────────────────────────

    fn table_select_top(&self, schema: &str, table: &str, rows: u32) -> String {
        select_top_sql(
            &TableRef {
                schema: schema.to_string(),
                name: table.to_string(),
            },
            rows,
        )
    }

    fn table_edit_preview(
        &self,
        schema: &str,
        table: &str,
        rows: u32,
        key_columns: &[String],
    ) -> String {
        let table_ref = TableRef {
            schema: schema.to_string(),
            name: table.to_string(),
        };
        let key = if key_columns.is_empty() {
            None
        } else {
            Some(TableKey {
                columns: key_columns.to_vec(),
            })
        };
        crate::admin::editing::edit_top_sql(&table_ref, rows, key.as_ref()).sql
    }

    // ── Query Store ────────────────────────────────────────────────────────

    fn query_store_top(&self, database: &str, limit: u32) -> String {
        QueryStore::top_queries_query(database, limit)
    }

    fn query_store_plans(&self, database: &str, query_id: i64) -> String {
        QueryStore::plan_history_query(database, query_id)
    }

    // ── Extended Events ────────────────────────────────────────────────────

    fn xevents_sessions_query(&self) -> String {
        ExtendedEvents::sessions_query()
    }

    fn xevents_session_events_query(&self, session_name: &str) -> String {
        ExtendedEvents::session_events_query(session_name)
    }

    fn xevents_ring_buffer_query(&self, session_name: &str) -> String {
        ExtendedEvents::live_ring_buffer_query(session_name)
    }

    // ── Standard Reports ────────────────────────────────────────────────────

    fn report_query(&self, report: &str) -> String {
        let report = match report {
            "disk-usage" => StandardReport::DiskUsage,
            "backup-history" => StandardReport::BackupHistory,
            "blocking" => StandardReport::BlockingSessions,
            other => {
                return format!(
                    "-- Unknown report \"{other}\". Valid: disk-usage, backup-history, blocking."
                );
            }
        };
        standard_report_query(report).to_string()
    }

    // ── Import / Export ─────────────────────────────────────────────────────

    fn import_csv_sql(&self, schema: &str, table: &str, file: &str, has_header: bool) -> String {
        let plan = CsvImportPlan {
            target: TableTarget::new(schema, table),
            file_path: file.to_string(),
            has_header,
            delimiter: ',',
            batch_size: 1000,
        };
        plan.bulk_insert_sql()
    }

    fn export_csv_hint(&self, source_sql: &str, file: &str, include_header: bool) -> String {
        let plan = CsvExportPlan {
            source_sql: source_sql.to_string(),
            file_path: file.to_string(),
            include_header,
            delimiter: ',',
        };
        plan.command_line_hint()
    }

    // ── SQL Agent ──────────────────────────────────────────────────────────

    fn agent_list_jobs_query(&self) -> String {
        JobBrowser::list_jobs_query().to_string()
    }

    fn agent_start_job_sql(&self, name: &str) -> String {
        JobWriter::start_job_sql(name)
    }

    fn agent_stop_job_sql(&self, name: &str) -> String {
        JobWriter::stop_job_sql(name)
    }

    fn agent_create_job_sql(&self, name: &str, step_name: &str, command: &str) -> String {
        let job = JobDefinition {
            name: name.to_string(),
            enabled: true,
            description: String::new(),
            owner_login: None,
        };
        let step = JobStepDefinition {
            step_name: step_name.to_string(),
            subsystem: "TSQL".to_string(),
            command: command.to_string(),
            database_name: None,
        };
        JobWriter::create_job_sql(&job, &[step])
    }

    fn agent_create_job_full_sql(
        &self,
        job: &JobDefinition,
        steps: &[JobStepDefinition],
    ) -> String {
        JobWriter::create_job_sql(job, steps)
    }

    fn agent_capability_query(&self) -> String {
        JobBrowser::capability_query().to_string()
    }

    fn agent_history_query(&self, job_id: Option<&str>) -> String {
        JobHistoryReader::history_query(job_id)
    }

    fn agent_list_schedules_query(&self) -> String {
        ScheduleReader::list_schedules_query().to_string()
    }

    fn agent_list_operators_query(&self) -> String {
        OperatorReader::list_operators_query().to_string()
    }

    fn agent_list_alerts_query(&self) -> String {
        AlertReader::list_alerts_query().to_string()
    }

    fn agent_update_job_sql(&self, original_name: &str, job: &JobDefinition) -> String {
        JobWriter::update_job_sql(original_name, job)
    }

    fn agent_upsert_job_step_sql(
        &self,
        job_name: &str,
        step_id: Option<i32>,
        step: &JobStepDefinition,
    ) -> String {
        JobWriter::upsert_job_step_sql(job_name, step_id, step)
    }

    fn agent_delete_job_sql(&self, job_name: &str) -> String {
        JobWriter::delete_job_sql(job_name)
    }

    fn agent_create_schedule_definition_sql(&self, schedule: &ScheduleDefinition) -> String {
        ScheduleWriter::create_schedule_definition_sql(schedule)
    }

    fn agent_update_schedule_sql(&self, schedule_id: i32, schedule: &ScheduleDefinition) -> String {
        ScheduleWriter::update_schedule_sql(schedule_id, schedule)
    }

    fn agent_attach_schedule_sql(&self, job_name: &str, schedule_name: &str) -> String {
        ScheduleWriter::attach_schedule_sql(job_name, schedule_name)
    }

    fn agent_delete_schedule_sql(&self, schedule_id: i32) -> String {
        ScheduleWriter::delete_schedule_sql(schedule_id)
    }

    fn agent_create_alert_sql(&self, alert: &AlertDefinition) -> String {
        AlertWriter::create_alert_sql(alert)
    }

    fn agent_update_alert_sql(&self, original_name: &str, alert: &AlertDefinition) -> String {
        AlertWriter::update_alert_sql(original_name, alert)
    }

    fn agent_add_notification_sql(&self, alert_name: &str, operator_name: &str) -> String {
        AlertWriter::add_notification_sql(alert_name, operator_name)
    }

    fn agent_delete_alert_sql(&self, name: &str) -> String {
        AlertWriter::delete_alert_sql(name)
    }

    fn agent_parse_alert_row(
        &self,
        id: Option<i64>,
        name: Option<&str>,
        enabled: Option<i64>,
        message_id: Option<i64>,
        severity: Option<i64>,
        delay_between_responses: Option<i64>,
    ) -> Option<AlertInfo> {
        AlertReader::parse_alert_row(
            id,
            name,
            enabled,
            message_id,
            severity,
            delay_between_responses,
        )
    }

    fn agent_create_operator_sql(&self, operator: &OperatorDefinition) -> String {
        OperatorWriter::create_operator_sql(operator)
    }

    fn agent_update_operator_sql(
        &self,
        original_name: &str,
        operator: &OperatorDefinition,
    ) -> String {
        OperatorWriter::update_operator_sql(original_name, operator)
    }

    fn agent_delete_operator_sql(&self, name: &str) -> String {
        OperatorWriter::delete_operator_sql(name)
    }

    // ── Security ───────────────────────────────────────────────────────────

    fn security_query(&self, kind: &str) -> String {
        match kind {
            "logins" => SecurityCatalog::server_logins_query().to_string(),
            "users" => SecurityCatalog::database_users_query().to_string(),
            "roles" => SecurityCatalog::roles_query().to_string(),
            "role-members" => SecurityCatalog::role_membership_query().to_string(),
            "permissions" => SecurityCatalog::permissions_query(None),
            other => {
                format!(
                    "-- Unknown security kind \"{other}\". Valid: logins, users, roles, role-members, permissions."
                )
            }
        }
    }

    fn security_create_login_sql(&self, name: &str, password: &str) -> String {
        SecurityWriter::create_login_sql(name, password)
    }

    fn security_create_user_sql(
        &self,
        user: &str,
        login: Option<&str>,
        schema: Option<&str>,
    ) -> String {
        SecurityWriter::create_user_sql(user, login, schema)
    }

    fn security_alter_role_sql(&self, role: &str, member: &str, add: bool) -> String {
        SecurityWriter::alter_role_sql(role, member, add)
    }

    fn security_permission_sql(
        &self,
        action: &str,
        permission: &str,
        object: &str,
        principal: &str,
    ) -> String {
        let action = match action {
            "grant" => PermissionAction::Grant,
            "deny" => PermissionAction::Deny,
            "revoke" => PermissionAction::Revoke,
            other => {
                return format!(
                    "-- Unknown permission action \"{other}\". Valid: grant, deny, revoke."
                );
            }
        };
        SecurityWriter::permission_sql(action, permission, object, principal)
    }

    fn security_user_databases_query(&self) -> String {
        SecurityCatalog::user_databases_query().to_string()
    }

    fn security_server_roles_query(&self) -> String {
        SecurityCatalog::server_roles_query().to_string()
    }

    fn security_server_role_memberships_query(&self, login: &str) -> String {
        SecurityCatalog::server_role_memberships_query(login)
    }

    fn security_login_db_user_mapping_query(&self, login: &str, databases: &[String]) -> String {
        SecurityCatalog::login_db_user_mapping_query(login, databases)
    }

    fn security_database_users_with_login_query(&self) -> String {
        SecurityCatalog::database_users_with_login_query().to_string()
    }

    fn security_use_database_sql(&self, database: &str, inner_sql: &str) -> String {
        SecurityCatalog::use_database_sql(database, inner_sql)
    }

    fn security_create_windows_login_sql(&self, name: &str) -> String {
        SecurityWriter::create_windows_login_sql(name)
    }

    fn security_alter_login_password_sql(&self, name: &str, new_password: &str) -> String {
        SecurityWriter::alter_login_password_sql(name, new_password)
    }

    fn security_alter_login_enabled_sql(&self, name: &str, enabled: bool) -> String {
        SecurityWriter::alter_login_enabled_sql(name, enabled)
    }

    fn security_alter_server_role_sql(&self, role: &str, member: &str, add: bool) -> String {
        SecurityWriter::alter_server_role_sql(role, member, add)
    }

    fn security_alter_user_default_schema_sql(&self, user: &str, schema: &str) -> String {
        SecurityWriter::alter_user_default_schema_sql(user, schema)
    }

    fn security_drop_user_sql(&self, user: &str) -> String {
        SecurityWriter::drop_user_sql(user)
    }

    fn security_create_contained_user_sql(
        &self,
        user: &str,
        password: &str,
        schema: Option<&str>,
    ) -> String {
        SecurityWriter::create_contained_user_sql(user, password, schema)
    }

    fn security_alter_user_password_sql(&self, user: &str, new_password: &str) -> String {
        SecurityWriter::alter_user_password_sql(user, new_password)
    }

    fn security_drop_login_sql(&self, name: &str) -> String {
        SecurityWriter::drop_login_sql(name)
    }

    fn security_query_for(&self, db: sqlly_core::DatabaseType, kind: &str) -> Option<String> {
        use crate::tool_dialects::SecurityDialect;
        match crate::tool_dialects::security_dialect(db)? {
            SecurityDialect::Postgres => Some(match kind {
                "roles" => crate::admin::security_sql::postgres::roles_query().to_string(),
                "role-members" => {
                    crate::admin::security_sql::postgres::role_members_query().to_string()
                }
                "db-connect" => {
                    crate::admin::security_sql::postgres::database_connect_grants_query()
                        .to_string()
                }
                "schema-usage" => {
                    crate::admin::security_sql::postgres::schema_usage_query().to_string()
                }
                "tables" => crate::admin::security_sql::postgres::tables_query().to_string(),
                _ => return None,
            }),
            SecurityDialect::MySql | SecurityDialect::MariaDb => Some(match kind {
                "users" => crate::admin::security_sql::mysql::users_query().to_string(),
                "users-compat" => {
                    crate::admin::security_sql::mysql::users_query_compat().to_string()
                }
                "roles" => crate::admin::security_sql::mysql::roles_query().to_string(),
                "databases" => crate::admin::security_sql::mysql::databases_query().to_string(),
                _ => return None,
            }),
            SecurityDialect::ClickHouse => Some(match kind {
                "users" => crate::admin::security_sql::clickhouse::users_query().to_string(),
                "roles" => crate::admin::security_sql::clickhouse::roles_query().to_string(),
                "grants" => crate::admin::security_sql::clickhouse::grants_query().to_string(),
                "role-grants" => {
                    crate::admin::security_sql::clickhouse::role_grants_query().to_string()
                }
                "profiles" => {
                    crate::admin::security_sql::clickhouse::settings_profiles_query().to_string()
                }
                _ => return None,
            }),
            // The PostgreSQL-shaped sets drive the shared Roles tab; the
            // `-compat` kinds answer from USER_* / ALL_* for logins without
            // DBA_* access, and the rest are Oracle-only detail sets.
            SecurityDialect::Oracle => {
                use crate::admin::security_sql::oracle;
                Some(match kind {
                    "roles" => oracle::roles_query().to_string(),
                    "roles-compat" => oracle::roles_query_compat().to_string(),
                    "role-members" => oracle::role_members_query(),
                    "role-members-compat" => oracle::role_members_query_compat().to_string(),
                    "db-connect" => oracle::database_connect_grants_query(),
                    "db-connect-compat" => {
                        oracle::database_connect_grants_query_compat().to_string()
                    }
                    "schema-usage" | "schema-usage-compat" => {
                        oracle::schema_usage_query().to_string()
                    }
                    "tables" | "tables-compat" => oracle::tables_query().to_string(),
                    "users" => oracle::users_query().to_string(),
                    "users-compat" => oracle::users_query_compat().to_string(),
                    "dba-roles" => oracle::dba_roles_query().to_string(),
                    "profiles" => oracle::profiles_query().to_string(),
                    _ => return None,
                })
            }
            SecurityDialect::SqlServer => None,
        }
    }

    fn security_table_grants_query_for(
        &self,
        db: sqlly_core::DatabaseType,
        grantee: &str,
    ) -> Option<String> {
        use crate::tool_dialects::SecurityDialect;
        match crate::tool_dialects::security_dialect(db)? {
            SecurityDialect::Postgres => Some(
                crate::admin::security_sql::postgres::table_grants_query(grantee),
            ),
            SecurityDialect::Oracle => Some(
                crate::admin::security_sql::oracle::table_grants_query(grantee),
            ),
            _ => None,
        }
    }

    fn security_show_grants_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        user: &str,
        host: &str,
    ) -> Option<String> {
        use crate::tool_dialects::SecurityDialect;
        match crate::tool_dialects::security_dialect(db)? {
            SecurityDialect::MySql | SecurityDialect::MariaDb => Some(
                crate::admin::security_sql::mysql::show_grants_sql(user, host),
            ),
            // ClickHouse principals have no host part; `SHOW GRANTS FOR`
            // works for users and roles alike.
            SecurityDialect::ClickHouse => Some(
                crate::admin::security_sql::clickhouse::show_grants_sql(user),
            ),
            _ => None,
        }
    }

    fn security_use_database_sql_for(
        &self,
        db: sqlly_core::DatabaseType,
        database: &str,
        inner_sql: &str,
    ) -> String {
        use crate::tool_dialects::SecurityDialect;
        match crate::tool_dialects::security_dialect(db) {
            Some(SecurityDialect::MySql | SecurityDialect::MariaDb) => {
                crate::admin::security_sql::mysql::use_database_sql(database, inner_sql)
            }
            Some(SecurityDialect::SqlServer) | None => {
                SecurityCatalog::use_database_sql(database, inner_sql)
            }
            // Neither PostgreSQL, ClickHouse nor Oracle switches databases inside a session.
            Some(
                SecurityDialect::Postgres | SecurityDialect::ClickHouse | SecurityDialect::Oracle,
            ) => inner_sql.to_string(),
        }
    }

    // ── Designers ──────────────────────────────────────────────────────────

    fn designer_create_table_sql(&self, schema: &str, table: &str, columns: &[String]) -> String {
        let mut designer_columns = Vec::new();
        let mut primary_key = Vec::new();
        for spec in columns {
            let parts: Vec<&str> = spec.split(':').collect();
            if parts.len() < 2 || parts[0].is_empty() {
                return format!(
                    "-- Invalid column spec \"{spec}\" (expected name:type[:pk][:identity][:null])"
                );
            }
            let name = parts[0].to_string();
            let flags: Vec<&str> = parts[2..].to_vec();
            let is_pk = flags.contains(&"pk");
            if is_pk {
                primary_key.push(name.clone());
            }
            designer_columns.push(DesignerColumn {
                name,
                data_type: parts[1].to_string(),
                nullable: flags.contains(&"null"),
                identity: flags.contains(&"identity"),
            });
        }
        let model = TableDesignerModel {
            schema: schema.to_string(),
            table: table.to_string(),
            columns: designer_columns,
            primary_key,
        };
        model.create_table_sql()
    }

    fn designer_create_index_sql(
        &self,
        schema: &str,
        table: &str,
        name: &str,
        columns: &[String],
        include_columns: &[String],
        unique: bool,
    ) -> String {
        if schema.trim().is_empty()
            || table.trim().is_empty()
            || name.trim().is_empty()
            || columns.is_empty()
        {
            return "-- schema, table, index name, and at least one column are required."
                .to_string();
        }
        let model = IndexDesignerModel {
            schema: schema.to_string(),
            table: table.to_string(),
            name: name.to_string(),
            columns: columns.to_vec(),
            include_columns: include_columns.to_vec(),
            unique,
            filter: None,
            fill_factor: None,
            clustered: false,
        };
        model.create_index_sql()
    }

    fn designer_create_relationship_sql(
        &self,
        name: &str,
        child_schema: &str,
        child_table: &str,
        child_columns: &[String],
        parent_schema: &str,
        parent_table: &str,
        parent_columns: &[String],
    ) -> String {
        if name.trim().is_empty()
            || child_schema.trim().is_empty()
            || child_table.trim().is_empty()
            || parent_schema.trim().is_empty()
            || parent_table.trim().is_empty()
        {
            return "-- relationship name, child table, and parent table are required.".to_string();
        }
        if child_columns.is_empty()
            || parent_columns.is_empty()
            || child_columns.len() != parent_columns.len()
        {
            return "-- child and parent columns must be non-empty and have matching counts."
                .to_string();
        }
        let model = RelationshipDesignerModel {
            name: name.to_string(),
            child_schema: child_schema.to_string(),
            child_table: child_table.to_string(),
            child_columns: child_columns.to_vec(),
            parent_schema: parent_schema.to_string(),
            parent_table: parent_table.to_string(),
            parent_columns: parent_columns.to_vec(),
        };
        model.add_foreign_key_sql()
    }

    // ── Schema Compare ────────────────────────────────────────────────────

    fn schema_compare_script(&self, source_json: &str, target_json: &str) -> String {
        let source: Vec<SchemaObjectDto> = match serde_json::from_str(source_json) {
            Ok(v) => v,
            Err(e) => return format!("-- Failed to parse source JSON: {e}"),
        };
        let target: Vec<SchemaObjectDto> = match serde_json::from_str(target_json) {
            Ok(v) => v,
            Err(e) => return format!("-- Failed to parse target JSON: {e}"),
        };
        let source: Vec<SchemaObject> = source.into_iter().map(Into::into).collect();
        let target: Vec<SchemaObject> = target.into_iter().map(Into::into).collect();
        let differences = compare_schema(&source, &target);
        if differences.is_empty() {
            return "-- No schema differences found.".to_string();
        }
        script_diff(&differences)
    }

    // ── Execution Plans ────────────────────────────────────────────────────

    fn plan_estimated_sql(&self, sql: &str) -> String {
        crate::plans::estimated_plan_sql(sql)
    }

    fn plan_actual_sql(&self, sql: &str) -> String {
        crate::plans::include_actual_plan_sql(sql)
    }

    fn plan_summarize(&self, showplan_xml: &str) -> String {
        serde_json::to_string_pretty(&crate::plans::summarize_showplan_xml(showplan_xml))
            .unwrap_or_else(|e| format!("-- Failed to serialize plan summary: {e}"))
    }

    fn plan_render(&self, showplan_xml: &str) -> String {
        match crate::plans::render_showplan_xml(showplan_xml) {
            Ok(layout) => serde_json::to_string_pretty(&layout)
                .unwrap_or_else(|e| format!("-- Failed to serialize plan layout: {e}")),
            Err(e) => format!("-- Failed to render plan: {e}"),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_built_in_templates() {
        let svc = DefaultAdminToolsService;
        let out = svc.templates_list();
        assert!(out.contains("Create Table"));
        assert!(out.contains("—"));
    }

    #[test]
    fn searches_templates() {
        let svc = DefaultAdminToolsService;
        let out = svc.templates_search("table");
        assert!(out.contains("Create Table"));
    }

    #[test]
    fn validates_backup_and_restore_inputs() {
        let svc = DefaultAdminToolsService;
        assert!(svc.validate_backup("", "/tmp/x.bak").is_err());
        assert!(svc.validate_backup("db", "").is_err());
        assert!(svc.validate_backup("db", "/tmp/x.bak").is_ok());
        assert!(svc.validate_restore("", "/tmp/x.bak").is_err());
        assert!(svc.validate_restore("db", "").is_err());
        assert!(svc.validate_restore("db", "/tmp/x.bak").is_ok());
    }

    #[test]
    fn index_and_disk_queries_match_admin_crate() {
        let svc = DefaultAdminToolsService;
        use crate::admin::disk_usage::DiskUsage;
        use crate::admin::index_usage::IndexUsage;
        use crate::{DiskDialect, IndexDialect};
        let db = sqlly_core::DatabaseType::SqlServer;
        assert_eq!(
            svc.index_usage_query_for(db).as_deref(),
            Some(IndexUsage::usage_query_for(IndexDialect::SqlServer))
        );
        assert_eq!(
            svc.disk_databases_query_for(db).as_deref(),
            Some(DiskUsage::databases_query_for(DiskDialect::SqlServer))
        );
    }

    #[test]
    fn generates_backup_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.backup_sql(
            "MyDb",
            "/tmp/backup.bak",
            BackupType::Full,
            true,
            true,
            false,
        );
        assert!(sql.contains("BACKUP DATABASE"));
        assert!(sql.contains("COMPRESSION"));
        assert!(sql.contains("CHECKSUM"));
    }

    #[test]
    fn generates_restore_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.restore_sql("MyDb", "/tmp/backup.bak", true, true);
        assert!(sql.contains("RESTORE DATABASE"));
        assert!(sql.contains("REPLACE"));
        assert!(sql.contains("RECOVERY"));
    }

    #[test]
    fn generates_activity_queries() {
        let svc = DefaultAdminToolsService;
        assert!(svc.activity_query("sessions").contains("dm_exec_sessions"));
        assert!(
            svc.activity_query("blocking")
                .contains("blocking_session_id")
        );
        assert!(
            svc.activity_query("expensive")
                .contains("dm_exec_query_stats")
        );
        assert!(svc.activity_query("waits").contains("dm_os_wait_stats"));
        assert!(svc.activity_query("bogus").contains("Unknown"));
    }

    #[test]
    fn generates_kill_sql() {
        let svc = DefaultAdminToolsService;
        assert_eq!(svc.activity_kill_sql(42), "KILL 42");
        assert!(
            svc.activity_diagnostic_sql_for(sqlly_core::DatabaseType::Postgres, 42)
                .is_some_and(|sql| sql.contains("pg_stat_activity"))
        );
        assert!(
            svc.activity_diagnostic_sql_for(sqlly_core::DatabaseType::Sqlite, 42)
                .is_none()
        );
    }

    #[test]
    fn generates_security_queries() {
        let svc = DefaultAdminToolsService;
        assert!(svc.security_query("logins").contains("server_principals"));
        assert!(svc.security_query("users").contains("database_principals"));
        assert!(
            svc.security_query("permissions")
                .contains("database_permissions")
        );
        assert!(svc.security_query("bogus").contains("Unknown"));
    }

    #[test]
    fn generates_security_writes() {
        let svc = DefaultAdminToolsService;
        assert!(
            svc.security_create_login_sql("app", "pass")
                .contains("CREATE LOGIN")
        );
        assert!(
            svc.security_create_user_sql("app", Some("login"), Some("dbo"))
                .contains("CREATE USER")
        );
        assert!(
            svc.security_permission_sql("grant", "SELECT", "dbo.Orders", "app")
                .contains("GRANT SELECT")
        );
    }

    #[test]
    fn security_query_for_postgres_reads_pg_roles() {
        let svc = DefaultAdminToolsService;
        let sql = svc
            .security_query_for(sqlly_core::DatabaseType::Postgres, "roles")
            .unwrap();
        assert!(sql.contains("pg_roles"));
    }

    /// The Oracle catalog answers every PostgreSQL-shaped kind (plus its
    /// `-compat` twin) so the shared Roles tab can drive it, and never
    /// prefixes a USE statement.
    #[test]
    fn security_query_for_oracle_reads_dba_views_with_compat_fallbacks() {
        let svc = DefaultAdminToolsService;
        let db = sqlly_core::DatabaseType::Oracle;
        for kind in [
            "roles",
            "role-members",
            "db-connect",
            "schema-usage",
            "tables",
        ] {
            let sql = svc.security_query_for(db, kind).unwrap();
            assert!(!sql.is_empty(), "{kind}");
            let compat = svc
                .security_query_for(db, &format!("{kind}-compat"))
                .unwrap();
            assert!(!compat.contains("dba_"), "{kind}-compat: {compat}");
        }
        assert!(
            svc.security_query_for(db, "roles")
                .unwrap()
                .contains("dba_users")
        );
        assert!(
            svc.security_table_grants_query_for(db, "APP")
                .unwrap()
                .contains("dba_tab_privs")
        );
        assert!(svc.security_show_grants_sql_for(db, "APP", "%").is_none());
        assert_eq!(
            svc.security_use_database_sql_for(db, "FREEPDB1", "GRANT X TO Y;"),
            "GRANT X TO Y;"
        );
        assert!(svc.security_query_for(db, "nope").is_none());
    }

    #[test]
    fn security_query_for_mysql_reads_mysql_user() {
        let svc = DefaultAdminToolsService;
        let sql = svc
            .security_query_for(sqlly_core::DatabaseType::MySql, "users")
            .unwrap();
        assert!(sql.contains("mysql.user"));
    }

    #[test]
    fn security_query_for_unsupported_engine_is_none() {
        let svc = DefaultAdminToolsService;
        assert!(
            svc.security_query_for(sqlly_core::DatabaseType::PlanetScale, "users")
                .is_none()
        );
    }

    #[test]
    fn generates_agent_queries() {
        let svc = DefaultAdminToolsService;
        assert!(svc.agent_list_jobs_query().contains("sysjobs"));
        assert!(svc.agent_start_job_sql("MyJob").contains("sp_start_job"));
        assert!(svc.agent_stop_job_sql("MyJob").contains("sp_stop_job"));
        assert!(
            svc.agent_create_job_sql("Job", "Step", "EXEC sp")
                .contains("sp_add_job")
        );
    }

    #[test]
    fn generates_query_store_queries() {
        let svc = DefaultAdminToolsService;
        assert!(
            svc.query_store_top("MyDb", 25)
                .contains("query_store_query")
        );
        assert!(
            svc.query_store_plans("MyDb", 42)
                .contains("query_store_plan")
        );
        let postgres = svc
            .query_insights_top(
                sqlly_core::DatabaseType::Postgres,
                "ignored",
                37,
                crate::admin::advanced_tools::QueryStoreRank::ExecutionCount,
                Some(24),
            )
            .unwrap();
        assert!(postgres.contains("pg_stat_statements"));
        assert!(postgres.contains("LIMIT 37"));
        assert!(!postgres.contains("DATEADD"));
        let mysql = svc
            .query_insights_top(
                sqlly_core::DatabaseType::MySql,
                "ignored",
                19,
                crate::admin::advanced_tools::QueryStoreRank::AvgLogicalReads,
                None,
            )
            .unwrap();
        assert!(mysql.contains("performance_schema"));
        assert!(mysql.contains("LIMIT 19"));
        let oracle = svc
            .query_insights_top(
                sqlly_core::DatabaseType::Oracle,
                "ignored",
                7,
                crate::admin::advanced_tools::QueryStoreRank::AvgLogicalReads,
                Some(24),
            )
            .unwrap();
        assert!(oracle.contains("v$sqlarea"));
        assert!(oracle.contains("FETCH FIRST 7 ROWS ONLY"));
        assert!(oracle.contains("ORDER BY s.buffer_gets / GREATEST(s.executions, 1) DESC"));
        assert!(
            svc.query_insights_top(
                sqlly_core::DatabaseType::Sqlite,
                "ignored",
                50,
                crate::admin::advanced_tools::QueryStoreRank::AvgDuration,
                None,
            )
            .is_none()
        );
    }

    #[test]
    fn generates_xevents_queries() {
        let svc = DefaultAdminToolsService;
        assert!(
            svc.xevents_sessions_query()
                .contains("server_event_sessions")
        );
        assert!(
            svc.xevents_session_events_query("MySession")
                .contains("MySession")
        );
        assert!(
            svc.xevents_ring_buffer_query("MySession")
                .contains("ring_buffer")
        );
    }

    #[test]
    fn generates_import_export() {
        let svc = DefaultAdminToolsService;
        let sql = svc.import_csv_sql("dbo", "Orders", "/tmp/data.csv", true);
        assert!(sql.contains("BULK INSERT"));
        assert!(sql.contains("FIRSTROW = 2"));
        let hint = svc.export_csv_hint("SELECT * FROM Orders", "/tmp/out.csv", true);
        assert!(hint.contains("bcp"));
    }

    #[test]
    fn generates_designer_table_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.designer_create_table_sql(
            "dbo",
            "Orders",
            &[
                "id:int:pk:identity".to_string(),
                "name:nvarchar:null".to_string(),
            ],
        );
        assert!(sql.contains("CREATE TABLE"));
        assert!(sql.contains("PRIMARY KEY"));
        assert!(sql.contains("IDENTITY"));
    }

    #[test]
    fn generates_designer_index_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.designer_create_index_sql(
            "dbo",
            "Orders",
            "IX_Orders",
            &["CustomerID".to_string()],
            &["OrderDate".to_string()],
            false,
        );
        assert!(sql.contains("CREATE INDEX"));
        assert!(sql.contains("INCLUDE"));
    }

    #[test]
    fn generates_designer_relationship_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.designer_create_relationship_sql(
            "FK_Orders_Customers",
            "dbo",
            "Orders",
            &["CustomerID".to_string()],
            "dbo",
            "Customers",
            &["CustomerID".to_string()],
        );
        assert!(sql.contains("ALTER TABLE"));
        assert!(sql.contains("FOREIGN KEY"));
    }

    #[test]
    fn generates_plan_sql() {
        let svc = DefaultAdminToolsService;
        let sql = svc.plan_estimated_sql("SELECT 1");
        assert!(sql.contains("SHOWPLAN_XML"));
        let actual = svc.plan_actual_sql("SELECT 1");
        assert!(actual.contains("STATISTICS XML"));
    }

    #[test]
    fn generates_table_select_top() {
        let svc = DefaultAdminToolsService;
        let sql = svc.table_select_top("dbo", "Orders", 100);
        assert!(sql.contains("SELECT TOP (100)"));
        assert!(sql.contains("Orders"));
    }

    #[test]
    fn generates_report_queries() {
        let svc = DefaultAdminToolsService;
        assert!(svc.report_query("disk-usage").contains("database_files"));
        assert!(svc.report_query("backup-history").contains("backupset"));
        assert!(svc.report_query("blocking").contains("blocking_session_id"));
        assert!(svc.report_query("bogus").contains("Unknown"));
    }

    #[test]
    fn schema_compare_handles_empty_json() {
        let svc = DefaultAdminToolsService;
        let result = svc.schema_compare_script("[]", "[]");
        assert!(result.contains("No schema differences"));
    }

    #[test]
    fn schema_compare_detects_differences() {
        let svc = DefaultAdminToolsService;
        let source = r#"[{"schema":"dbo","name":"Orders","kind":"TABLE","definition":"CREATE TABLE dbo.Orders (id INT)"}]"#;
        let target = r#"[]"#;
        let result = svc.schema_compare_script(source, target);
        assert!(result.contains("Missing in target"));
    }
}
