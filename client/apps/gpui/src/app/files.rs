use super::*;

#[cfg(not(target_arch = "wasm32"))]
use gpui::Styled;

#[cfg(not(target_arch = "wasm32"))]
pub(super) struct PendingToolImport {
    pub bundle: sqlly_core::ProfileExportBundle,
    pub skipped: Vec<sqlly_core::SkippedConnection>,
    pub has_ssh_handler: bool,
    pub kind: sqlly_core::ToolFileKind,
    pub had_password_in_source: bool,
}

/// What Import From Other Tools found: a secret-free preview to confirm, or
/// (Redis Insight) connections already imported through the credential
/// importer, with the status line to show.
#[cfg(not(target_arch = "wasm32"))]
enum ToolImportResult {
    Preview(PendingToolImport),
    Imported(String),
}

/// Import a Redis Insight export: write its certificate and key bodies to
/// PEM files under the app's data directory (keys owner-only), then run
/// the converted credential file through the credential importer, which
/// moves every password and passphrase into the keychain.
#[cfg(not(target_arch = "wasm32"))]
fn import_redis_insight_export(text: &str, source: &std::path::Path) -> Result<String, String> {
    let pem_dir = dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SQLLY")
        .join("certificates")
        .join("redis-insight");
    let import = sqlly_core::tool_import::redis_insight::convert(text, &pem_dir)?;
    if !import.pem_files.is_empty() {
        std::fs::create_dir_all(&pem_dir).map_err(|e| {
            crate::localization::format(
                "shell.files.redis_certificate_directory_failed",
                &[
                    ("path", &pem_dir.display().to_string()),
                    ("error", &e.to_string()),
                ],
            )
        })?;
    }
    for file in &import.pem_files {
        write_pem(file)?;
    }
    let credential_text = serde_json::to_string(&import.credential_file).map_err(|e| {
        crate::localization::format(
            "shell.files.redis_import_prepare_failed",
            &[("error", &e.to_string())],
        )
    })?;
    let summary = crate::credential_import::import_credential_text(
        &credential_text,
        crate::connection::ConnectionStore::global(),
    )
    .map_err(|error| {
        crate::localization::format("shell.files.redis_import_failed", &[("error", &error)])
    })?;
    // Every part is a complete catalog sentence in the UI language (this
    // runs on the UI thread); the password sentence names the Redis Insight
    // export as the file to delete.
    let summary = summary.message_for(
        source,
        crate::credential_import::CredentialImportSource::RedisInsightExport,
    );
    let skipped = import
        .skipped
        .iter()
        .map(|skip| {
            crate::localization::format(
                "shell.files.redis_import_skipped",
                &[
                    ("name", skip.name.clone()),
                    ("reason", crate::localization::message(&skip.reason)),
                ],
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let notes = import
        .notes
        .iter()
        .map(crate::localization::message)
        .collect::<Vec<_>>()
        .join(" ");
    if import.pem_files.is_empty() {
        Ok(crate::localization::format(
            "shell.files.redis_import_summary",
            &[
                ("summary", &summary),
                ("skipped", &skipped),
                ("notes", &notes),
            ],
        ))
    } else {
        Ok(crate::localization::format(
            "shell.files.redis_import_summary_certificates",
            &[
                ("summary", &summary),
                ("certificates", &pem_dir.display().to_string()),
                ("skipped", &skipped),
                ("notes", &notes),
            ],
        ))
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn write_pem(file: &sqlly_core::tool_import::redis_insight::PemFile) -> Result<(), String> {
    let fail = |e: std::io::Error| {
        crate::localization::format(
            "shell.files.certificate_write_failed",
            &[
                ("path", &file.path.display().to_string()),
                ("error", &e.to_string()),
            ],
        )
    };
    #[cfg(unix)]
    if file.private {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&file.path)
            .map_err(fail)?;
        return out.write_all(file.contents.as_bytes()).map_err(fail);
    }
    std::fs::write(&file.path, file.contents.as_bytes()).map_err(fail)
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DroppedKind {
    Sql,
    /// A SQLite or DuckDB database file, opened as a connection.
    #[cfg(not(target_arch = "wasm32"))]
    Database,
    Other,
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn classify_dropped_path(path: &std::path::Path) -> DroppedKind {
    if sqlly_core::file_database_engine(path).is_some() {
        DroppedKind::Database
    } else if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("sql") || e.eq_ignore_ascii_case("tsql"))
    {
        DroppedKind::Sql
    } else {
        DroppedKind::Other
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DiskPromptSpec {
    pub level: gpui::PromptLevel,
    pub message: String,
    pub detail: String,
    pub buttons: Vec<String>,
}

/// Copy and buttons for the Reload/Keep and Keep/Close disk prompts.
pub(crate) fn disk_prompt_spec(
    state: crate::file_watch::DiskState,
    dirty: bool,
    name: &str,
) -> DiskPromptSpec {
    use crate::file_watch::DiskState;
    match state {
        DiskState::ModifiedOnDisk => DiskPromptSpec {
            level: gpui::PromptLevel::Info,
            message: crate::localization::format(
                "shell.files.disk_modified_message",
                &[("name", name)],
            ),
            detail: if dirty {
                crate::localization::text("shell.files.disk_modified_detail_dirty")
            } else {
                crate::localization::text("shell.files.disk_modified_detail_clean")
            },
            buttons: vec![
                crate::localization::text("shell.files.disk_reload_button"),
                crate::localization::text("shell.files.disk_keep_button"),
            ],
        },
        DiskState::DeletedOnDisk => DiskPromptSpec {
            level: gpui::PromptLevel::Warning,
            message: crate::localization::format(
                "shell.files.disk_deleted_message",
                &[("name", name)],
            ),
            detail: crate::localization::text("shell.files.disk_deleted_detail"),
            buttons: vec![
                crate::localization::text("shell.files.disk_keep_button"),
                crate::localization::text("shell.files.disk_close_button"),
            ],
        },
        DiskState::Clean => DiskPromptSpec {
            level: gpui::PromptLevel::Info,
            message: String::new(),
            detail: String::new(),
            buttons: Vec::new(),
        },
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn file_paths_match(a: &std::path::Path, b: &std::path::Path) -> bool {
    if a == b {
        return true;
    }
    let ac = std::fs::canonicalize(a).unwrap_or_else(|_| a.to_path_buf());
    let bc = std::fs::canonicalize(b).unwrap_or_else(|_| b.to_path_buf());
    if ac == bc {
        return true;
    }
    a.file_name() == b.file_name()
        && match (a.parent(), b.parent()) {
            (Some(ap), Some(bp)) => {
                let apc = std::fs::canonicalize(ap).unwrap_or_else(|_| ap.to_path_buf());
                let bpc = std::fs::canonicalize(bp).unwrap_or_else(|_| bp.to_path_buf());
                apc == bpc
            }
            _ => false,
        }
}

/// The status line after Export for Other Tools saved its file.
#[cfg(not(target_arch = "wasm32"))]
fn tool_export_status(
    tool: sqlly_core::tool_export::ToolExportFormat,
    exported: usize,
    skipped: usize,
    path: &std::path::Path,
) -> String {
    let key = match (exported == 1, skipped > 0) {
        (true, false) => "shell.files.tool_export_done_singular",
        (false, false) => "shell.files.tool_export_done_plural",
        (true, true) => "shell.files.tool_export_done_singular_skipped",
        (false, true) => "shell.files.tool_export_done_plural_skipped",
    };
    let count = exported.to_string();
    let path = path.display().to_string();
    let skipped = skipped.to_string();
    let mut args = vec![
        ("count", count.as_str()),
        ("tool", tool.tool_name()),
        ("path", path.as_str()),
    ];
    if key.ends_with("_skipped") {
        args.push(("skipped", skipped.as_str()));
    }
    crate::localization::format(key, &args)
}

#[cfg(not(target_arch = "wasm32"))]
fn write_connection_export_file(path: &std::path::Path, json: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("connections-export");
    let tmp = path.with_file_name(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(|error| {
            crate::localization::format(
                "shell.files.connection_export_write_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        std::io::Write::write_all(&mut file, json.as_bytes()).map_err(|error| {
            crate::localization::format(
                "shell.files.connection_export_write_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        file.sync_all().map_err(|error| {
            crate::localization::format(
                "shell.files.connection_export_write_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        std::fs::rename(&tmp, path).map_err(|error| {
            crate::localization::format(
                "shell.files.connection_export_write_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

const RECENT_FILES_CAP: usize = 15;

#[cfg(not(target_arch = "wasm32"))]
fn recent_files_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push("recent-files.json");
    p
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load_recent_files() -> Vec<String> {
    std::fs::read_to_string(recent_files_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub(super) fn load_recent_files() -> Vec<String> {
    Vec::new()
}

#[cfg(not(target_arch = "wasm32"))]
fn save_recent_files(files: &[String]) {
    if let Ok(text) = serde_json::to_string_pretty(files) {
        let _ = std::fs::write(recent_files_path(), text);
    }
}

#[cfg(target_arch = "wasm32")]
fn save_recent_files(_files: &[String]) {}

/// Write a schema-documentation set relative to a chosen base path. The
/// empty-suffix file goes to `base` itself; every other file becomes a sibling
/// named `<stem><suffix>.<ext>`. Returns how many files were written, or the
/// first write error. Stops on the first failure so a partial set is reported.
#[cfg(not(target_arch = "wasm32"))]
fn write_schema_doc_files(
    base: &std::path::Path,
    files: &[crate::schema_documentation::DocFile],
) -> std::io::Result<usize> {
    let parent = base.parent().unwrap_or_else(|| std::path::Path::new("."));
    let stem = base
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("schema-docs");
    let ext = base.extension().and_then(|s| s.to_str()).unwrap_or("md");
    let mut written = 0;
    for file in files {
        let target = if file.suffix.is_empty() {
            base.to_path_buf()
        } else {
            parent.join(format!("{stem}{}.{ext}", file.suffix))
        };
        std::fs::write(&target, file.content.as_bytes())?;
        written += 1;
    }
    Ok(written)
}

/// Best-effort "show this file in the platform file manager".
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn reveal_path_in_file_manager(path: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open")
        .arg("-R")
        .arg(path)
        .spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{path}"))
        .spawn();
    #[cfg(target_os = "linux")]
    {
        let parent = std::path::Path::new(path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        let _ = std::process::Command::new("xdg-open").arg(parent).spawn();
    }
}

#[cfg(target_arch = "wasm32")]
pub(super) fn reveal_path_in_file_manager(_path: &str) {}

/// Collects all rows from the first result set of a `SELECT * FROM …`
/// query and generates an INSERT or MERGE script via `table_actions`.
/// Mirrors the Swift `TableStreamWriter` path for `.insertInto` /
/// `.updateOrInsert` save formats. Returns the generated SQL script or an
/// error message.
/// A table save or script failure produced on the tokio runtime, worded on
/// the UI thread with [`TableWorkFailure::text`]: translation state is per
/// thread, so worker threads would always build English.
enum TableWorkFailure {
    Message(sqlly_core::DisplayMessage),
    SaveQuery(sqlly_core::DisplayMessage),
    ScriptQuery(sqlly_core::DisplayMessage),
}

impl TableWorkFailure {
    fn text(&self) -> String {
        match self {
            Self::Message(message) => crate::localization::message(message),
            Self::SaveQuery(error) => crate::localization::format(
                "shell.files.save_data_query_failed",
                &[("error", crate::localization::message(error))],
            ),
            Self::ScriptQuery(error) => crate::localization::format(
                "shell.files.script_query_failed",
                &[("error", crate::localization::message(error))],
            ),
        }
    }
}

/// Drain a query handle into full in-memory columns + rows (spill files
/// included), for bounded exports that materialize the whole set.
#[cfg(not(target_arch = "wasm32"))]
async fn collect_table_rows(
    handle: sqlly_services::QueryHandle,
) -> Result<
    (
        Vec<sqlly_core::ResultColumn>,
        Vec<Vec<sqlly_core::CellValue>>,
    ),
    TableWorkFailure,
> {
    let mut events = handle.events;
    let mut columns: Vec<sqlly_core::ResultColumn> = Vec::new();
    let mut rows: Vec<Vec<sqlly_core::CellValue>> = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::ResultSet { columns: cols, .. } => columns = cols,
            sqlly_services::QueryEvent::Rows { rows: batch, .. } => rows.extend(batch),
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                // The spill file holds the COMPLETE result set — it replaces
                // any prefix forwarded before payload suppression.
                for set in &manifest.result_sets {
                    if let Some(all) = sqlly_services::take_spilled_rows(set) {
                        rows = all;
                    }
                }
            }
            sqlly_services::QueryEvent::Error { message } => {
                return Err(TableWorkFailure::Message(crate::localization::deferred(
                    "shell.files.collect_rows_query_failed",
                    &[("error", &message)],
                )));
            }
            sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok((columns, rows))
}

async fn collect_table_script(
    handle: sqlly_services::QueryHandle,
    db_type: sqlly_lang::sqlmodel::DatabaseType,
    table: &crate::table_actions::QualifiedTable,
    kind: &crate::schema_tree::TableScriptKind,
    key_columns: &[String],
) -> Result<String, TableWorkFailure> {
    use crate::schema_tree::TableScriptKind;
    use crate::table_actions::{ColumnDescriptor, insert_into_sql_for, upsert_sql_for};
    use sqlly_core::CellValue;

    let mut events = handle.events;
    let mut columns: Vec<ColumnDescriptor> = Vec::new();
    let mut raw_columns: Vec<sqlly_core::ResultColumn> = Vec::new();
    let mut rows: Vec<Vec<CellValue>> = Vec::new();
    let mut error: Option<String> = None;

    while let Some(event) = events.recv().await {
        match event {
            sqlly_services::QueryEvent::ResultSet { columns: cols, .. } => {
                raw_columns = cols.clone();
                columns = cols
                    .into_iter()
                    .map(|c| ColumnDescriptor {
                        name: c.name,
                        data_type: c.data_type,
                        nullable: c.nullable,
                    })
                    .collect();
            }
            sqlly_services::QueryEvent::Rows { rows: batch, .. } => {
                rows.extend(batch);
            }
            sqlly_services::QueryEvent::SpillManifest { manifest } => {
                // The spill file holds the COMPLETE result set — it replaces
                // the prefix forwarded before payload suppression (row
                // threshold or byte budget), so the script omits nothing.
                for set in &manifest.result_sets {
                    if let Some(all) = sqlly_services::take_spilled_rows(set) {
                        rows = all;
                    }
                }
            }
            sqlly_services::QueryEvent::Error { message } => {
                error = Some(message);
                break;
            }
            sqlly_services::QueryEvent::Completed { .. }
            | sqlly_services::QueryEvent::Cancelled => break,
            _ => {}
        }
    }

    if let Some(msg) = error {
        return Err(TableWorkFailure::Message(crate::localization::deferred(
            "shell.files.collect_script_query_failed",
            &[("error", &msg)],
        )));
    }

    let col_names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
    let script = match kind {
        TableScriptKind::Insert => insert_into_sql_for(db_type, table, &columns, &[], &rows),
        TableScriptKind::TempTable => crate::result_export::temp_table_sql_for(
            db_type,
            &raw_columns,
            &rows,
            &format!("#import_{}", table.table),
        ),
        TableScriptKind::Merge => {
            // Prefer the table's discovered primary key (composite keys
            // included, verified against the actual result columns); fall
            // back to the first column with an explicit note so the user
            // knows to review the match key.
            let discovered: Vec<String> = key_columns
                .iter()
                .filter(|key| col_names.iter().any(|c| c.eq_ignore_ascii_case(key)))
                .cloned()
                .collect();
            if discovered.is_empty() {
                let key = col_names.first().cloned().unwrap_or_default();
                let script =
                    upsert_sql_for(db_type, table, &columns, std::slice::from_ref(&key), &rows);
                format!(
                    "-- No primary key was discovered for this table; the merge matches on the first column ({key}).
-- Review the match key before running.
{script}"
                )
            } else {
                upsert_sql_for(db_type, table, &columns, &discovered, &rows)
            }
        }
    };
    Ok(script)
}

impl AppView {
    /// Record a completed export/save deliverable (result export, table data
    /// export, or schema documentation) in the notification center, and raise a
    /// system notification when the window is in the background — these finish
    /// asynchronously and the user has often looked away by the time they do.
    /// Every producer is native-only, so the browser build sees no callers.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub(super) fn record_export_saved(&mut self, message: impl Into<String>, cx: &gpui::App) {
        let message = message.into();
        self.notifications.push(
            crate::notification_center::NotificationKind::ExportSaved,
            message.clone(),
            chrono::Utc::now(),
        );
        if !self.window_is_active && self.os_notify.enabled {
            crate::notification_center::notify_system(
                &crate::localization::text("shell.files.export_saved_notification_title"),
                &message,
                cx,
            );
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn record_export_saved_with_path(
        &mut self,
        message: impl Into<String>,
        path: std::path::PathBuf,
        cx: &gpui::App,
    ) {
        let message = message.into();
        self.notifications.push_with_path(
            crate::notification_center::NotificationKind::ExportSaved,
            message.clone(),
            chrono::Utc::now(),
            Some(path),
        );
        if !self.window_is_active && self.os_notify.enabled {
            crate::notification_center::notify_system(
                &crate::localization::text("shell.files.export_saved_notification_title"),
                &message,
                cx,
            );
        }
    }

    fn no_results_to_export_message() -> String {
        crate::localization::text("shell.files.no_results_to_export")
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn prompt_and_export(
        &mut self,
        format: crate::results::ExportFormat,
        results_entity: Entity<crate::results::ResultsView>,
        cx: &mut Context<Self>,
    ) {
        if results_entity.read(cx).export_source(cx).is_none() {
            self.set_status(StatusLevel::Info, Self::no_results_to_export_message());
            cx.notify();
            return;
        }
        let filename = format!("results.{}", format.extension());
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&filename));
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(path))) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.start_export_task(path, format, results_entity, cx);
                    })
                    .ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_export_task(
        &mut self,
        path: std::path::PathBuf,
        format: crate::results::ExportFormat,
        results_entity: Entity<crate::results::ResultsView>,
        cx: &mut Context<Self>,
    ) {
        use crate::background_tasks::{TaskKind, TaskState};
        use crate::export_stream::{ExportSource, gz_path, run_export};
        use crate::preferences::ExportOptionsPreferences;

        let Some((columns, db, source)) = results_entity.read(cx).export_source(cx) else {
            self.set_status(StatusLevel::Info, Self::no_results_to_export_message());
            cx.notify();
            return;
        };
        let export_label = format.label();
        let opts = ExportOptionsPreferences::load().to_options(format);
        let display_path = gz_path(&path, opts.gzip);
        let file_name = display_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("export");
        let (id, progress) = self.background_tasks.start(
            TaskKind::Export,
            crate::localization::format(
                "shell.files.export_task_label",
                &[("format", export_label), ("name", file_name)],
            ),
            Some(display_path.clone()),
        );
        if let ExportSource::Spill { total, .. } = &source {
            progress.set_total_rows(*total as u64);
        }
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<u64, sqlly_core::DisplayMessage>>(1);
        cx.background_spawn(async move {
            let outcome = run_export(&path, format, db, &columns, source, &progress, &opts);
            let _ = tx.send(outcome).await;
        })
        .detach();
        let app = cx.entity();
        let async_cx = cx.to_async();
        let app_recv = app.clone();
        let async_cx_recv = async_cx.clone();
        cx.spawn(async move |_this, _cx| {
            if let Some(outcome) = rx.recv().await {
                let mut cx = async_cx_recv.clone();
                app_recv.update(&mut cx, |this, cx| {
                    // Worded here: the export ran off the UI thread.
                    let outcome = outcome.map_err(crate::localization::message);
                    this.finish_export_task(id, outcome, cx);
                });
            }
        })
        .detach();
        let app_tick = app.clone();
        let async_cx_tick = async_cx.clone();
        cx.spawn(async move |_this, _cx| {
            loop {
                async_cx_tick
                    .background_executor()
                    .timer(std::time::Duration::from_millis(250))
                    .await;
                let mut cx = async_cx_tick.clone();
                let mut still_running = false;
                app_tick.update(&mut cx, |this, _| {
                    still_running = this
                        .background_tasks
                        .get(id)
                        .is_some_and(|t| t.state == TaskState::Running);
                });
                if !still_running {
                    break;
                }
                app_tick.update(&mut cx, |_, cx| cx.notify());
            }
        })
        .detach();
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn finish_export_task(
        &mut self,
        id: crate::background_tasks::TaskId,
        outcome: Result<u64, String>,
        cx: &mut Context<Self>,
    ) {
        use crate::background_tasks::TaskState;
        use crate::export_stream::CANCELLED;

        match outcome {
            Ok(rows) => {
                let (label, path) = self
                    .background_tasks
                    .get(id)
                    .map(|t| (t.label.clone(), t.output_path.clone()))
                    .unwrap_or((String::new(), None));
                self.background_tasks.finish(id, TaskState::Finished);
                let message = crate::localization::format(
                    "shell.files.export_saved_status",
                    &[("label", &label), ("count", &rows.to_string())],
                );
                self.set_status(StatusLevel::Success, message.clone());
                if let Some(path) = path {
                    self.record_export_saved_with_path(message, path, cx);
                } else {
                    self.record_export_saved(message, cx);
                }
            }
            Err(e) if e == CANCELLED => {
                self.background_tasks.finish(id, TaskState::Cancelled);
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.files.export_cancelled"),
                );
            }
            Err(e) => {
                self.background_tasks
                    .finish(id, TaskState::Failed(e.clone()));
                self.set_status(StatusLevel::Error, e);
            }
        }
        cx.notify();
    }

    pub(super) fn handle_open_sql(
        &mut self,
        _action: &OpenSql,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Self::open_sql_file(
            self,
            &crate::localization::text("shell.files.open_sql_prompt"),
            cx,
        );
    }

    pub(super) fn handle_open_sql_file(
        &mut self,
        _action: &OpenSqlFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        Self::open_sql_file(
            self,
            &crate::localization::text("shell.files.open_sql_prompt"),
            cx,
        );
    }

    /// Shows the custom project-file picker (Folder / Project SQL modes,
    /// metadata chips, SQL preview) instead of the native open panel.
    pub(super) fn handle_open_project_file(
        &mut self,
        _action: &OpenProjectFile,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.overlays.project_file_picker.is_some() {
            self.restore_editor_focus(cx);
            self.overlays.project_file_picker = None;
            cx.notify();
            return;
        }
        self.open_project_file_picker(cx);
    }

    pub(super) fn handle_open_sqlly_export(
        &mut self,
        _action: &OpenSqllyExport,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.open_export_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(crate::localization::text("shell.files.open_export_prompt").into()),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let result = (|| {
                    if !path
                        .extension()
                        .and_then(|value| value.to_str())
                        .is_some_and(|value| value.eq_ignore_ascii_case("sqllyexport"))
                    {
                        return Err(crate::localization::text(
                            "shell.files.export_extension_required",
                        ));
                    }
                    let metadata = std::fs::metadata(&path).map_err(|error| {
                        crate::localization::format(
                            "shell.files.export_inspect_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &error.to_string()),
                            ],
                        )
                    })?;
                    if metadata.len() > crate::import_export::IMPORT_FILE_BYTE_CAP {
                        return Err(crate::localization::text("shell.files.export_size_limit"));
                    }
                    let text = std::fs::read_to_string(&path).map_err(|error| {
                        crate::localization::format(
                            "shell.files.export_read_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &error.to_string()),
                            ],
                        )
                    })?;
                    crate::import_export::parse_sqlly_export_snapshot(&text)
                        .map(|snapshot| (path, snapshot))
                })();
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| match result {
                        Ok((path, snapshot)) => this.install_sqlly_export(path, snapshot, cx),
                        Err(error) => {
                            this.set_status(StatusLevel::Error, error);
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .detach();
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_export_connections(
        &mut self,
        _action: &ExportConnections,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            StatusLevel::Error,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.files.export_connections_unavailable",
            )),
        );
        cx.notify();
    }

    /// Opens the server → database picker; the chosen connections are
    /// written to a connection file with no passwords on confirm.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_export_connections(
        &mut self,
        _action: &ExportConnections,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_connection_export(crate::credential_export::ExportSecrets::Omit, window, cx);
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_export_credentials(
        &mut self,
        _action: &ExportCredentials,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_status(
            StatusLevel::Error,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.files.export_credentials_unavailable",
            )),
        );
        cx.notify();
    }

    /// Opens the server → database picker; the chosen connections are
    /// written to a connection file with passwords in clear text on confirm.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_export_credentials(
        &mut self,
        _action: &ExportCredentials,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_connection_export(crate::credential_export::ExportSecrets::Include, window, cx);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_connection_export(
        &mut self,
        secrets: crate::credential_export::ExportSecrets,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = crate::connection::ConnectionStore::global();
        let profiles = store.list();
        if profiles.is_empty() {
            if crate::credential_export::store_has_exportable_content(store) {
                // Labels and saved logins but no connection to pick: there's
                // nothing for the picker to show, so export them straight away.
                self.handle_credential_export_event(
                    &crate::credential_export_dialog::CredentialExportDialogEvent::Export {
                        ids: Vec::new(),
                        secrets,
                        tool: None,
                    },
                    cx,
                );
            } else {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.files.no_connections_to_export"),
                );
                cx.notify();
            }
            return;
        }
        let dialog = cx.new(|cx| {
            crate::credential_export_dialog::CredentialExportDialog::new(&profiles, secrets, cx)
        });
        cx.subscribe_in(&dialog, window, |this, _, event, _window, cx| {
            this.handle_credential_export_event(event, cx);
        })
        .detach();
        self.overlays.credential_export_dialog = Some(dialog);
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_credential_export_event(
        &mut self,
        event: &crate::credential_export_dialog::CredentialExportDialogEvent,
        cx: &mut Context<Self>,
    ) {
        use crate::credential_export::ExportSecrets;
        use crate::credential_export_dialog::CredentialExportDialogEvent;
        self.overlays.credential_export_dialog = None;
        let (ids, secrets, tool) = match event {
            CredentialExportDialogEvent::Cancel => {
                cx.notify();
                return;
            }
            CredentialExportDialogEvent::Export { ids, secrets, tool } => {
                (ids.clone(), *secrets, *tool)
            }
        };
        let selected: std::collections::HashSet<uuid::Uuid> = ids.into_iter().collect();
        if let Some(tool) = tool {
            self.write_tool_export(tool, &selected, cx);
            return;
        }
        let (json, summary) = match crate::credential_export::export_connections_json(
            crate::connection::ConnectionStore::global(),
            &selected,
            secrets,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.set_status(StatusLevel::Error, error);
                cx.notify();
                return;
            }
        };
        let suggested = match secrets {
            ExportSecrets::Include => "sqlly-connections-with-passwords.json",
            ExportSecrets::Omit => "sqlly-connections.json",
        };
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(suggested));
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            // Owner-only permissions either way: even without passwords the
            // file maps out every server and login name.
            let result = write_connection_export_file(&path, &json);
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            let count = summary.connections;
                            let missing = summary.missing_passwords;
                            let key = match (secrets, count == 1, missing > 0) {
                                (ExportSecrets::Include, true, false) => {
                                    "shell.files.credentials_exported_singular"
                                }
                                (ExportSecrets::Include, false, false) => {
                                    "shell.files.credentials_exported_plural"
                                }
                                (ExportSecrets::Include, true, true) => {
                                    "shell.files.credentials_exported_singular_missing"
                                }
                                (ExportSecrets::Include, false, true) => {
                                    "shell.files.credentials_exported_plural_missing"
                                }
                                (ExportSecrets::Omit, true, false) => {
                                    "shell.files.connections_exported_singular"
                                }
                                (ExportSecrets::Omit, false, false) => {
                                    "shell.files.connections_exported_plural"
                                }
                                (ExportSecrets::Omit, true, true) => {
                                    "shell.files.connections_exported_singular_missing"
                                }
                                (ExportSecrets::Omit, false, true) => {
                                    "shell.files.connections_exported_plural_missing"
                                }
                            };
                            let mut arguments = vec![
                                ("count", count.to_string()),
                                ("path", path.display().to_string()),
                            ];
                            if missing > 0 {
                                arguments.push(("missing", missing.to_string()));
                            }
                            let arguments = arguments
                                .iter()
                                .map(|(name, value)| (*name, value.as_str()))
                                .collect::<Vec<_>>();
                            let message = crate::localization::format(key, &arguments);
                            this.set_status(StatusLevel::Success, message);
                        }
                        Err(error) => this.set_status(StatusLevel::Error, error),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
        cx.notify();
    }

    /// Export for Other Tools reads the connection store and writes a file,
    /// neither of which the browser build has.
    #[cfg(target_arch = "wasm32")]
    fn tool_export_unavailable(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Error,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.files.tool_export_unavailable",
            )),
        );
        cx.notify();
    }

    /// Export for Other Tools: the connection picker, narrowed to the
    /// engines `tool` can open; the file is written on confirm.
    #[cfg(not(target_arch = "wasm32"))]
    fn open_tool_export(
        &mut self,
        tool: sqlly_core::tool_export::ToolExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let profiles: Vec<_> = crate::connection::ConnectionStore::global()
            .list()
            .into_iter()
            .filter(|profile| tool.supports(profile.database_type))
            .collect();
        if profiles.is_empty() {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.files.tool_export_nothing",
                    &[("tool", tool.tool_name())],
                ),
            );
            cx.notify();
            return;
        }
        let dialog = cx.new(|cx| {
            crate::credential_export_dialog::CredentialExportDialog::for_tool(&profiles, tool, cx)
        });
        cx.subscribe_in(&dialog, window, |this, _, event, _window, cx| {
            this.handle_credential_export_event(event, cx);
        })
        .detach();
        self.overlays.credential_export_dialog = Some(dialog);
        cx.notify();
    }

    /// Writes the picked connections in `tool`'s own format. Never reads a
    /// secret: the profiles are mapped to their secret-free export shape.
    #[cfg(not(target_arch = "wasm32"))]
    fn write_tool_export(
        &mut self,
        tool: sqlly_core::tool_export::ToolExportFormat,
        selected: &std::collections::HashSet<uuid::Uuid>,
        cx: &mut Context<Self>,
    ) {
        let profiles: Vec<sqlly_core::profile_store::ExportedProfile> =
            crate::connection::ConnectionStore::global()
                .list()
                .iter()
                .filter(|profile| selected.contains(&profile.id.0))
                .map(sqlly_core::profile_store::export_profile)
                .collect();
        let export = sqlly_core::tool_export::export_for_tool(tool, &profiles);
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(tool.file_name()));
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            // Owner-only, like every connection export: the file maps out
            // servers and login names even without passwords.
            let result = write_connection_export_file(&path, &export.contents);
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| {
                    match result {
                        Ok(()) => {
                            let message = tool_export_status(
                                tool,
                                export.exported,
                                export.skipped.len(),
                                &path,
                            );
                            this.set_status(StatusLevel::Success, message);
                        }
                        Err(error) => this.set_status(StatusLevel::Error, error),
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
        cx.notify();
    }

    pub(super) fn handle_export_for_pg_admin(
        &mut self,
        _action: &ExportForPgAdmin,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::PgAdminServersJson,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    pub(super) fn handle_export_for_azure_data_studio(
        &mut self,
        _action: &ExportForAzureDataStudio,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::AzureDataStudioSettingsJson,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    pub(super) fn handle_export_for_pg_service(
        &mut self,
        _action: &ExportForPgService,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::PgServiceConf,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    pub(super) fn handle_export_for_dbeaver(
        &mut self,
        _action: &ExportForDbeaver,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::DbeaverDataSourcesJson,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    pub(super) fn handle_export_for_data_grip(
        &mut self,
        _action: &ExportForDataGrip,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::JetBrainsDataSourcesXml,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    pub(super) fn handle_export_for_db_gate(
        &mut self,
        _action: &ExportForDbGate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(not(target_arch = "wasm32"))]
        self.open_tool_export(
            sqlly_core::tool_export::ToolExportFormat::DbGateEnv,
            window,
            cx,
        );
        #[cfg(target_arch = "wasm32")]
        self.tool_export_unavailable(window, cx);
    }

    /// After any import wrote connections to the store: show them in the
    /// server explorer (and everything that reads the sidebar's list, like
    /// the query bar's server dropdown) at once, the same refresh the
    /// connection editor does after a save.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn connections_imported(&mut self, cx: &mut Context<Self>) {
        self.sidebar.update(cx, |s, cx| s.refresh_connections(cx));
        cx.notify();
    }

    /// An iCloud sync pass changed connections, snippets or queries on this
    /// Mac: show them. Synced settings apply at the next launch.
    #[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
    pub(super) fn icloud_sync_applied(&mut self, cx: &mut Context<Self>) {
        let Some(changes) = crate::icloud_sync::runtime::last_changes() else {
            return;
        };
        if changes.connections_changed {
            self.connections_imported(cx);
        }
        if changes.snippets_changed {
            self.apply_snippet_preferences(cx);
        }
        if changes.library_changed {
            self.sidebar.update(cx, |s, cx| s.rescan_library(cx));
        }
        if changes.connections_changed || changes.snippets_changed || changes.library_changed {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("icloud_sync.shell.updated"),
            );
        }
        cx.notify();
    }

    pub(super) fn handle_import_connections(
        &mut self,
        _action: &ImportConnections,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_connections_proxied(cx) {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.import_connections_unavailable",
                )),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(
                    crate::localization::text("shell.files.import_connections_prompt").into(),
                ),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let Ok(Ok(Some(paths))) = rx.await else {
                    return;
                };
                let Some(path) = paths.into_iter().next() else {
                    return;
                };
                let result = std::fs::read_to_string(&path)
                    .map_err(|error| {
                        crate::localization::format(
                            "shell.files.import_connections_read_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &error.to_string()),
                            ],
                        )
                    })
                    .and_then(|text| {
                        crate::credential_import::import_connection_file(
                            &text,
                            &path,
                            crate::connection::ConnectionStore::global(),
                        )
                    });
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| match result {
                        Ok(message) => {
                            match message {
                                Some(message) => {
                                    this.connections_imported(cx);
                                    this.set_status(StatusLevel::Success, message)
                                }
                                None => this.set_status(
                                    StatusLevel::Info,
                                    crate::localization::text(
                                        "shell.files.import_connections_empty",
                                    ),
                                ),
                            }
                            cx.notify();
                        }
                        Err(error) => {
                            this.set_status(StatusLevel::Error, error);
                            cx.notify();
                        }
                    })
                    .ok();
            })
            .detach();
        }
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn handle_import_connections_from_tools(
        &mut self,
        _action: &ImportConnectionsFromTools,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_connections_proxied(cx) {
            return;
        }
        self.set_status(
            StatusLevel::Error,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.files.import_tools_unavailable",
            )),
        );
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_import_connections_from_tools(
        &mut self,
        _action: &ImportConnectionsFromTools,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.refuse_if_connections_proxied(cx) {
            return;
        }
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(crate::localization::text("shell.files.import_tools_prompt").into()),
        });
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            let result: Result<ToolImportResult, String> = (|| {
                let bytes = std::fs::read(&path).map_err(|error| {
                    crate::localization::format(
                        "shell.files.import_tools_read_failed",
                        &[
                            ("path", &path.display().to_string()),
                            ("error", &error.to_string()),
                        ],
                    )
                })?;
                let text = String::from_utf8_lossy(&bytes);
                let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let kind = sqlly_core::detect_kind(file_name, &text).ok_or_else(|| {
                    crate::localization::text("shell.files.import_tools_unrecognized")
                })?;
                // Redis Insight exports carry passwords and certificates:
                // they go through the credential importer, which moves every
                // secret into the keychain, instead of the secret-free preview.
                if kind == sqlly_core::ToolFileKind::RedisInsightConnectionsJson {
                    return import_redis_insight_export(&text, &path)
                        .map(ToolImportResult::Imported);
                }
                let preview = if kind == sqlly_core::ToolFileKind::TablePlusConnectionsPlist {
                    let groups = if let Some(groups_path) = path
                        .parent()
                        .map(|parent| parent.join("ConnectionGroups.plist"))
                        .filter(|groups_path| groups_path.is_file())
                    {
                        let group_bytes = std::fs::read(&groups_path).map_err(|error| {
                            crate::localization::format(
                                "shell.files.import_groups_read_failed",
                                &[
                                    ("path", &groups_path.display().to_string()),
                                    ("error", &error.to_string()),
                                ],
                            )
                        })?;
                        sqlly_core::parse_tableplus_groups_plist(&group_bytes)?
                    } else {
                        std::collections::HashMap::new()
                    };
                    sqlly_core::parse_tableplus_plist_with_groups(&bytes, &groups)?
                } else {
                    sqlly_core::parse_tool_file_bytes(kind, &bytes)?
                };
                let existing: std::collections::HashSet<String> =
                    crate::connection::ConnectionStore::global()
                        .list()
                        .iter()
                        .map(|p| {
                            sqlly_core::dedupe_key(
                                p.database_type,
                                &p.server,
                                p.port,
                                p.database.as_deref(),
                            )
                        })
                        .collect();
                let (bundle, skipped) = sqlly_core::preview_to_bundle(&preview, &existing);
                let has_ssh_handler = preview.connections.iter().any(|c| c.has_ssh_handler);
                let had_password_in_source = preview.connections.iter().any(|c| c.had_password);
                Ok(ToolImportResult::Preview(PendingToolImport {
                    bundle,
                    skipped,
                    has_ssh_handler,
                    kind,
                    had_password_in_source,
                }))
            })();
            let mut cx = async_cx.clone();
            weak_self
                .update(&mut cx, |this, cx| match result {
                    Ok(ToolImportResult::Preview(pending)) => {
                        this.pending_tool_import = Some(pending);
                        cx.notify();
                    }
                    Ok(ToolImportResult::Imported(message)) => {
                        this.connections_imported(cx);
                        this.set_status(StatusLevel::Success, message);
                        cx.notify();
                    }
                    Err(error) => {
                        this.set_status(StatusLevel::Error, error);
                        cx.notify();
                    }
                })
                .ok();
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn confirm_tool_import(&mut self, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_tool_import.take() else {
            return;
        };
        let imported = crate::connection::ConnectionStore::global().import_bundle(&pending.bundle);
        if imported > 0 {
            self.connections_imported(cx);
        }
        let skipped = pending.skipped.len();
        self.set_status(
            StatusLevel::Success,
            crate::localization::format(
                if imported == 1 {
                    "shell.files.import_tools_singular"
                } else {
                    "shell.files.import_tools_plural"
                },
                &[
                    ("imported", &imported.to_string()),
                    ("skipped", &skipped.to_string()),
                ],
            ),
        );
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn cancel_tool_import(&mut self, cx: &mut Context<Self>) {
        self.pending_tool_import = None;
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn render_tool_import_dialog(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let pending = self.pending_tool_import.as_ref()?;
        let s = Theme::scaled;
        let mut rows: Vec<gpui::AnyElement> = Vec::new();

        for profile in &pending.bundle.profiles {
            let db = profile
                .database
                .clone()
                .unwrap_or_else(|| crate::localization::text("shell.files.import_no_database"));
            let project = profile
                .project
                .as_deref()
                .map(|g| format!(" ({g})"))
                .unwrap_or_default();
            let port = profile.port.to_string();
            rows.push(
                div()
                    .min_h(px(s(26.0)))
                    .flex()
                    .items_center()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text())
                    .child(crate::localization::format(
                        "shell.files.import_preview_row",
                        &[
                            ("name", profile.name.as_str()),
                            ("engine", profile.database_type.display_name()),
                            ("server", profile.server.as_str()),
                            ("port", port.as_str()),
                            ("database", db.as_str()),
                            ("project", project.as_str()),
                        ],
                    ))
                    .into_any_element(),
            );
        }
        for skip in &pending.skipped {
            rows.push(
                div()
                    .min_h(px(s(26.0)))
                    .flex()
                    .items_center()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "shell.files.import_preview_skipped",
                        &[
                            ("name", skip.name.clone()),
                            ("reason", crate::localization::message(&skip.reason)),
                        ],
                    ))
                    .into_any_element(),
            );
        }

        // Sources that store passwords in the file itself: say they were
        // deliberately left behind.
        let from_navicat_or_tableplus = matches!(
            pending.kind,
            sqlly_core::ToolFileKind::NavicatNcx
                | sqlly_core::ToolFileKind::TablePlusConnectionsPlist
                | sqlly_core::ToolFileKind::TablePlusConnectionJson
                | sqlly_core::ToolFileKind::AzureDataStudioSettingsJson
                | sqlly_core::ToolFileKind::PgServiceConf
                | sqlly_core::ToolFileKind::DbGateEnv
        );
        let notes = crate::localization::text(
            match (
                pending.has_ssh_handler,
                from_navicat_or_tableplus && pending.had_password_in_source,
            ) {
                (true, true) => "shell.files.import_notes_ssh_ignored",
                (true, false) => "shell.files.import_notes_ssh",
                (false, true) => "shell.files.import_notes_ignored",
                (false, false) => "shell.files.import_notes",
            },
        );

        Some(
            crate::dialog::Dialog::new("tool-import-dialog", |this: &mut Self| {
                this.dialog_frames.get("tool-import-dialog", || {
                    crate::dialog::DialogFrame::new_auto(crate::theme::Theme::scaled(520.0))
                })
            })
            .title(crate::localization::text("shell.files.import_tools_title"))
            .confirm(
                crate::localization::text("shell.files.import_confirm_button"),
                crate::localization::text("shell.files.import_cancel_button"),
            )
            .on_confirm(|this, _window, cx| this.confirm_tool_import(cx))
            .on_reject(|this, _window, cx| this.cancel_tool_import(cx))
            .on_close(|this, _window, cx| this.cancel_tool_import(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "shell.files.import_tools_supported",
                    )),
            )
            .child(div().flex().flex_col().children(rows))
            .child(
                div()
                    .text_size(px(Theme::font_size()))
                    .text_color(Theme::text_dim())
                    .child(notes),
            )
            .build(self, window, cx)
            .into_any_element(),
        )
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn install_sqlly_export(
        &mut self,
        path: std::path::PathBuf,
        snapshot: crate::import_export::ParsedSqllyExport,
        cx: &mut Context<Self>,
    ) {
        let title = path
            .file_stem()
            .map(|value| value.to_string_lossy().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "SQLLY Export".to_string());
        self.new_tab(cx, Some("-- Read-only SQLLY export snapshot\n".to_string()));
        let tab_index = self.active_tab;
        let Some(tab) = self.tabs.get_mut(tab_index) else {
            return;
        };
        tab.name = title.clone();
        tab.dirty = false;
        tab.file_path = None;
        tab.connection_target = None;
        tab.restored_connection_inert = true;
        tab.snapshot_only = true;
        let result_set = snapshot.result_set;
        tab.results.update(cx, |results, cx| {
            results.set_layout_scope(None);
            results.set_source_db_type(snapshot.source_engine);
            results.set_historical_results(
                crate::localization::format(
                    "shell.files.readonly_export_context",
                    &[("path", &path.display().to_string())],
                ),
                vec![crate::query_version_history::SnapshotSet {
                    columns: result_set.columns,
                    rows: result_set.rows,
                    rows_affected: None,
                }],
                cx,
            );
        });
        self.show_results();
        self.set_status(
            StatusLevel::Success,
            crate::localization::format("shell.files.readonly_export_opened", &[("title", &title)]),
        );
        cx.notify();
    }

    /// Prompts for a folder, then opens the picker rooted there in Project
    /// SQL mode so every `.sql` file under it is listed recursively.
    pub(super) fn handle_open_folder(
        &mut self,
        _action: &OpenFolder,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prompt_for_project_folder(cx);
    }

    /// The only way a project root gets chosen: the user picks the folder.
    /// SQLly never scans a folder (home, Documents, Downloads, ...) the user
    /// did not pick, which on macOS would raise folder-access prompts.
    fn prompt_for_project_folder(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.open_folder_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: Some(crate::localization::text("shell.files.open_folder_prompt").into()),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(folder) = paths.into_iter().next()
                {
                    crate::sandbox_access::remember(&folder);
                    let mut cx = async_cx.clone();
                    weak_self
                        .update(&mut cx, |this, cx| {
                            this.project_file_picker_root = Some(folder);
                            this.overlays.project_file_picker = None;
                            this.save_workspace_session(cx);
                            this.open_project_file_picker(cx);
                        })
                        .ok();
                }
            })
            .detach();
        }
    }

    /// Palette "Project Folder: Git Changes…": git status / diff / commit
    /// for the folder opened with File › Open Folder….
    pub(super) fn open_project_git(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.git_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(root) = self.project_file_picker_root.clone() else {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.files.open_project_first"),
                );
                cx.notify();
                return;
            };
            cx.defer(move |cx| crate::git_panel::ProjectGitWindow::open(root, cx));
        }
    }

    pub(super) fn open_project_file_picker(&mut self, cx: &mut Context<Self>) {
        // No project yet: ask for one rather than defaulting to the home
        // folder, whose recursive scan walks into Documents, Downloads and
        // Desktop and makes macOS ask for access to each.
        #[cfg(not(target_arch = "wasm32"))]
        let Some(root) = self.project_file_picker_root.clone() else {
            self.prompt_for_project_folder(cx);
            return;
        };
        // Browser build: no home directory exists; the picker simply shows
        // an empty listing unless a root was set.
        #[cfg(target_arch = "wasm32")]
        let root = self
            .project_file_picker_root
            .clone()
            .unwrap_or_else(|| PathBuf::from("/"));
        let known_objects =
            crate::project_file_picker::known_objects_from_catalog(&self.completion_catalog);
        let app_view = cx.entity();
        // The picker's filter field is a gpui-component InputState, which
        // needs a live `Window` at construction. This method is reachable from
        // async continuations (folder prompt), so re-enter via the active
        // window on the next tick.
        cx.defer(move |app| {
            let Some(win) = app.active_window() else {
                return;
            };
            let _ = win.update(app, |_root, window, app| {
                app_view.update(app, |this, cx| {
                    let picker =
                        cx.new(|cx| ProjectFilePicker::new(root, known_objects, window, cx));
                    picker.update(cx, |p, cx| p.focus_filter(window, cx));
                    cx.subscribe(&picker, Self::handle_project_file_picker_event)
                        .detach();
                    this.overlays.project_file_picker = Some(picker);
                    cx.notify();
                });
            });
        });
    }

    pub(super) fn handle_project_file_picker_event(
        &mut self,
        _picker: Entity<ProjectFilePicker>,
        event: &ProjectFilePickerEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            ProjectFilePickerEvent::Open(path) => {
                self.restore_editor_focus(cx);
                self.overlays.project_file_picker = None;
                match std::fs::read_to_string(path) {
                    Ok(content) => self.finish_opening_sql_file(path.clone(), content, cx),
                    Err(e) => self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.files.open_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &e.to_string()),
                            ],
                        ),
                    ),
                }
                cx.notify();
            }
            ProjectFilePickerEvent::OpenDdl { schema, name, kind } => {
                self.restore_editor_focus(cx);
                self.overlays.project_file_picker = None;
                self.open_model_object_ddl(schema.clone(), name.clone(), *kind, cx);
                cx.notify();
            }
            ProjectFilePickerEvent::RootChanged(root) => {
                self.project_file_picker_root = Some(root.clone());
                self.save_workspace_session(cx);
            }
            ProjectFilePickerEvent::ShowGit(root) => {
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let root = root.clone();
                    cx.defer(move |cx| crate::git_panel::ProjectGitWindow::open(root, cx));
                }
                #[cfg(target_arch = "wasm32")]
                let _ = root;
            }
            ProjectFilePickerEvent::Cancelled => {
                self.restore_editor_focus(cx);
                self.overlays.project_file_picker = None;
                cx.notify();
            }
        }
    }

    pub(super) fn open_sql_file(&mut self, prompt: &str, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = prompt;
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.open_disk_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let prompt: SharedString = prompt.to_string().into();
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(prompt),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                    && let Ok(content) = std::fs::read_to_string(&path)
                {
                    // Recent files reopen this path in later runs.
                    crate::sandbox_access::remember(&path);
                    let mut cx = async_cx.clone();
                    weak_self
                        .update(&mut cx, |this, cx| {
                            this.finish_opening_sql_file(path, content, cx);
                        })
                        .ok();
                }
            })
            .detach();
        }
    }

    /// Open SQL files dropped onto the window from Finder/Explorer: each
    /// `.sql`/`.tsql` path opens as a file-backed tab through the same flow
    /// as Open SQL File; a path already open in a tab refocuses that tab
    /// instead of duplicating it. Non-SQL paths are ignored with a notice.
    pub(super) fn open_dropped_sql_files(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut opened = 0usize;
        let mut skipped = 0usize;
        for path in paths {
            #[cfg(not(target_arch = "wasm32"))]
            if classify_dropped_path(&path) == DroppedKind::Database {
                self.handle_deep_link_raw(path.to_string_lossy().to_string(), cx);
                opened += 1;
                continue;
            }
            let is_sql = path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("sql") || e.eq_ignore_ascii_case("tsql"));
            if !is_sql {
                skipped += 1;
                continue;
            }
            let path_str = path.to_string_lossy().to_string();
            if let Some(idx) = self
                .tabs
                .iter()
                .position(|t| t.file_path.as_deref() == Some(path_str.as_str()))
            {
                self.select_tab(idx, cx);
                opened += 1;
                continue;
            }
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    self.finish_opening_sql_file(path, content, cx);
                    opened += 1;
                }
                Err(e) => {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.files.dropped_open_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &e.to_string()),
                            ],
                        ),
                    );
                }
            }
        }
        if opened > 0 {
            self.set_status(
                StatusLevel::Success,
                crate::localization::format(
                    if opened == 1 {
                        "shell.files.drop_opened_singular"
                    } else {
                        "shell.files.drop_opened_plural"
                    },
                    &[("count", &opened.to_string())],
                ),
            );
        } else if skipped > 0 {
            self.set_status(
                StatusLevel::Info,
                crate::localization::text("shell.files.drop_unsupported"),
            );
        }
        cx.notify();
    }

    /// Puts loaded file contents into a fresh file-backed query tab and
    /// restores any resume metadata recorded for that path. Shared by the
    /// native Open SQL panel and the project-file picker.
    pub(super) fn finish_opening_sql_file(
        &mut self,
        path: PathBuf,
        content: String,
        cx: &mut Context<Self>,
    ) {
        let path_str = path.to_string_lossy();
        if path_str.ends_with(&format!(".{}", crate::notebook_model::NOTEBOOK_EXTENSION))
            || crate::notebook_model::is_ipynb_path(&path_str)
        {
            match crate::notebook_model::Notebook::from_file_text(&path_str, &content) {
                Ok(model) => {
                    self.note_recent_file(&path_str);
                    self.new_notebook_tab(model, Some(path_str.into_owned()), cx);
                }
                Err(message) => {
                    self.set_status(StatusLevel::Error, message);
                    cx.notify();
                }
            }
            return;
        }
        self.note_recent_file(&path.to_string_lossy());
        self.new_tab(cx, Some(content));
        if let Some(tab) = self.tabs.last_mut() {
            tab.file_path = Some(path.to_string_lossy().to_string());
            if !tab.user_named {
                // Language-neutral: the file's own name, never a translated
                // placeholder that would be persisted in the session.
                tab.name = path
                    .file_stem()
                    .filter(|stem| !stem.is_empty())
                    .or_else(|| path.file_name())
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.to_string_lossy().to_string());
            }
            tab.auto_named = false;
            tab.auto_number = None;
            #[cfg(not(target_arch = "wasm32"))]
            tab.record_clean_disk(&path);
            // Load resume metadata for this file-path key. Compute effective
            // state before applying so layout/diff/FIM gating works correctly.
            let key = tab.metadata_key();
            if let Some(record) = self.ai_summary_metadata.get(&key).cloned() {
                tab.resume_effective_enabled =
                    crate::preferences::QueryResumePolicy::effective_enabled(
                        self.query_resume_prefs.scope,
                        true, // file-backed
                        record.resume.as_ref().and_then(|r| r.override_enabled),
                    );
                Self::apply_metadata_record_to_tab(tab, &record);
            }
        }
        // Recompute resume state — the new tab is now file-backed.
        let tab_idx = self.tabs.len() - 1;
        self.handle_file_backing_changed(tab_idx, cx);
    }

    pub(super) fn handle_save_sql(
        &mut self,
        _action: &SaveSql,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.files.save_browser_unavailable"),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self
                .tabs
                .get(self.active_tab)
                .is_some_and(|t| t.is_notebook())
            {
                self.save_notebook(self.active_tab, false, cx);
                return;
            }
            let (editor_entity, existing_path) = {
                let Some(tab) = self.tabs.get(self.active_tab) else {
                    return;
                };
                (tab.editor.clone(), tab.file_path.clone())
            };
            let fmt_prefs = crate::preferences::load_sql_formatting_preferences();
            if fmt_prefs.format_on_save {
                editor_entity.update(cx, |ed, cx| ed.format_sql_document_with(&fmt_prefs, cx));
            }
            let text = editor_entity.read(cx).text().to_string();

            if let Some(path_str) = existing_path {
                match std::fs::write(&path_str, &text) {
                    Ok(()) => {
                        if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                            tab.dirty = false;
                            tab.record_clean_disk(std::path::Path::new(&path_str));
                        }
                        self.set_status(
                            StatusLevel::Success,
                            crate::localization::format(
                                "shell.files.saved_to_path",
                                &[("path", &path_str)],
                            ),
                        );
                    }
                    Err(e) => {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.files.save_failed",
                                &[("error", &e.to_string())],
                            ),
                        );
                    }
                }
                cx.notify();
                return;
            }

            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("query.sql"));
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    match std::fs::write(&path, &text) {
                        Ok(()) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    if let Some(tab) = this.tabs.get_mut(this.active_tab) {
                                        // Capture the old metadata key before
                                        // changing the file path, then migrate
                                        // the record so it's findable under the
                                        // new key. Mirrors Swift save-as flow
                                        // (QueryWorkspaceController.swift:2989-2998).
                                        let old_key = tab.metadata_key();
                                        tab.file_path = Some(path.to_string_lossy().to_string());
                                        tab.dirty = false;
                                        tab.record_clean_disk(&path);
                                        let new_key = tab.metadata_key();
                                        if old_key != new_key {
                                            this.ai_summary_metadata.migrate(&old_key, &new_key);
                                        }
                                    }
                                    // Recompute resume state — the tab
                                    // transitioned from unsaved to file-backed.
                                    this.handle_file_backing_changed(this.active_tab, cx);
                                    // Persist the migrated metadata to disk.
                                    if let Some(path) =
                                        crate::query_ai_summary::ai_summary_metadata_file()
                                    {
                                        let _ =
                                        crate::query_ai_summary::QueryAISummaryMetadataStore::save(
                                            &this.ai_summary_metadata,
                                            &path,
                                        );
                                    }
                                    this.set_status(
                                        StatusLevel::Success,
                                        crate::localization::format(
                                            "shell.files.saved_to_path",
                                            &[("path", &path.display().to_string())],
                                        ),
                                    );
                                    cx.notify();
                                })
                                .ok();
                        }
                        Err(e) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.set_status(
                                        StatusLevel::Error,
                                        crate::localization::format(
                                            "shell.files.save_failed",
                                            &[("error", &e.to_string())],
                                        ),
                                    );
                                    cx.notify();
                                })
                                .ok();
                        }
                    }
                }
            })
            .detach();
        }
    }

    /// Save the active result set to a file in any enabled export format.
    /// The format follows the extension the user picks in the save dialog
    /// (`.json`, `.xlsx`, `.md`, `.sql`, …); the suggested filename carries
    /// the first format enabled in Export preferences. Text and binary paths
    /// stay separate — XLSX goes through `export_bytes`, never coerced
    /// through a string.
    pub(super) fn handle_save_results(
        &mut self,
        _action: &SaveResultsCsv,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.files.export_results_browser_unavailable"),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let results_entity = {
                let Some(tab) = self.tabs.get(self.active_tab) else {
                    return;
                };
                tab.results.clone()
            };
            // Probe cheaply for "anything to export" before opening a dialog
            // (rendering a whole CSV here read a windowed result from disk).
            if !results_entity.read(cx).has_active_result_set() {
                self.set_status(StatusLevel::Info, Self::no_results_to_export_message());
                cx.notify();
                return;
            }
            let export_prefs = crate::preferences::ExportPreferences::load();
            let default_format = ExportFormat::all()
                .iter()
                .copied()
                .find(|f| export_prefs.standard_enabled(*f))
                .unwrap_or(ExportFormat::Csv);
            let default_name = format!("results.{}", default_format.extension());

            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&default_name));
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    let format = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .and_then(ExportFormat::from_extension)
                        .unwrap_or(default_format);
                    let mut cx = async_cx.clone();
                    weak_self
                        .update(&mut cx, |this, cx| {
                            if crate::export_stream::is_streamable(format) {
                                this.start_export_task(path, format, results_entity, cx);
                                return;
                            }
                            this.save_results_file(path, format, results_entity, cx);
                        })
                        .ok();
                }
            })
            .detach();
        }
    }

    /// Write the active result set to `path` in a non-streamed `format`
    /// (File ▸ Save Results). A windowed result's first rows are read off
    /// the UI thread first; the write then runs with them in memory.
    #[cfg(not(target_arch = "wasm32"))]
    fn save_results_file(
        &mut self,
        path: std::path::PathBuf,
        format: ExportFormat,
        results_entity: Entity<crate::results::ResultsView>,
        cx: &mut Context<Self>,
    ) {
        let weak = cx.weak_entity();
        let (rerun_path, rerun_results) = (path.clone(), results_entity.clone());
        let deferred = results_entity.update(cx, |view, cx| {
            let slot = view.active_slot();
            let limit = crate::result_export::EXPORT_MAX_ROWS;
            let caption = crate::localization::format(
                "shell.files.export_preparing",
                &[("count", &view.spill_read_count(slot, limit).to_string())],
            );
            view.defer_until_spill_prefixes(vec![(slot, limit)], caption, cx, move |cx| {
                let _ = weak.update(cx, |this, cx| {
                    this.save_results_file(rerun_path, format, rerun_results, cx)
                });
            })
        });
        if deferred {
            return;
        }
        // Re-read at write time — results may have changed
        // while the dialog was open, and the freshest set
        // is what the user expects to land on disk.
        let opts = crate::preferences::ExportOptionsPreferences::load().to_options(format);
        let (write_result, truncated) = if format == ExportFormat::Xlsx {
            results_entity.update(cx, |v, cx| {
                let slot = v.active_slot();
                v.prepare_excel_export(slot, cx);
            });
            match results_entity.read(cx).export_bytes_capped(
                format,
                crate::result_export::EXPORT_MAX_ROWS,
                cx,
            ) {
                Some((bytes, truncated)) => (std::fs::write(&path, bytes), truncated),
                None => {
                    self.set_status(StatusLevel::Info, Self::no_results_to_export_message());
                    cx.notify();
                    return;
                }
            }
        } else {
            match results_entity.read(cx).export_string_capped_with(
                format,
                crate::result_export::EXPORT_MAX_ROWS,
                &opts,
                cx,
            ) {
                Some((text, truncated)) => (
                    crate::results::write_export_bytes(&path, text.as_bytes(), opts.gzip)
                        .map_err(std::io::Error::other),
                    truncated,
                ),
                None => {
                    self.set_status(StatusLevel::Info, Self::no_results_to_export_message());
                    cx.notify();
                    return;
                }
            }
        };
        match write_result {
            Ok(()) => {
                let level = if truncated {
                    StatusLevel::Warning
                } else {
                    StatusLevel::Success
                };
                let saved_path = crate::export_stream::gz_path(&path, opts.gzip);
                let saved_path = saved_path.display().to_string();
                let saved = if truncated {
                    crate::localization::format(
                        "shell.files.results_saved_truncated",
                        &[
                            ("format", format.label()),
                            ("path", &saved_path),
                            ("count", &crate::result_export::EXPORT_MAX_ROWS.to_string()),
                        ],
                    )
                } else {
                    crate::localization::format(
                        "shell.files.results_saved",
                        &[("format", format.label()), ("path", &saved_path)],
                    )
                };
                self.set_status(level, saved.clone());
                self.record_export_saved(saved, cx);
                if truncated {
                    results_entity.update(cx, |view, cx| view.note_truncated_export_active(cx));
                }
                // The grid's rows carry 8 KiB previews of
                // oversized cells; say so.
                results_entity.update(cx, |view, cx| {
                    view.note_preview_cells_in_active_export(cx);
                });
            }
            Err(e) => self.set_status(
                StatusLevel::Error,
                crate::localization::format(
                    "shell.files.result_save_failed",
                    &[("error", &e.to_string())],
                ),
            ),
        }
        cx.notify();
    }

    /// Export the loaded schema model as a Markdown data dictionary to a
    /// file the user picks. Local text generation only — no server work.
    pub(super) fn handle_export_schema_docs(
        &mut self,
        _action: &ExportSchemaDocs,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.schema_docs_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.completion_snapshot.is_empty() {
                self.set_status(
                    StatusLevel::Info,
                    crate::localization::text("shell.files.schema_docs_no_model"),
                );
                cx.notify();
                return;
            }
            let title = self
                .active_profile
                .as_ref()
                .map(|profile| match &profile.database {
                    Some(database) => format!("{} / {database}", profile.name),
                    None => profile.name.clone(),
                })
                .unwrap_or_else(|| {
                    crate::localization::text("shell.files.schema_docs_fallback_title")
                });
            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("schema-docs.md"));
            let weak = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        // Large models split into an index + one file per schema
                        // so nothing is truncated; small ones stay a single
                        // file. The index links carry the chosen stem/extension
                        // verbatim so they match the sibling filenames below.
                        let base_name = path
                            .file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or("schema-docs");
                        let extension = path.extension().and_then(|e| e.to_str()).unwrap_or("md");
                        let files = crate::schema_documentation::generate_documentation(
                            &this.completion_snapshot,
                            &title,
                            base_name,
                            extension,
                        );
                        match write_schema_doc_files(&path, &files) {
                            Ok(count) => {
                                let saved = if count > 1 {
                                    crate::localization::format(
                                        "shell.files.schema_docs_saved_multiple",
                                        &[
                                            ("path", &path.display().to_string()),
                                            ("count", &count.to_string()),
                                        ],
                                    )
                                } else {
                                    crate::localization::format(
                                        "shell.files.schema_docs_saved_single",
                                        &[("path", &path.display().to_string())],
                                    )
                                };
                                this.set_status(StatusLevel::Success, saved.clone());
                                this.record_export_saved(saved, cx);
                            }
                            Err(e) => this.set_status(
                                StatusLevel::Error,
                                crate::localization::format(
                                    "shell.files.schema_docs_save_failed",
                                    &[("error", &e.to_string())],
                                ),
                            ),
                        }
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    /// Run `SELECT * FROM [schema].[table]` in the background, collect the
    /// rows, and generate an INSERT or MERGE script via `table_actions`.
    /// Mirrors the Swift `QueryWorkspaceController.streamTableToFile` path
    /// for the `.insertInto` and `.updateOrInsert` save formats.
    /// Explorer "Save Data As" for the file formats: prompt for a save path,
    /// run a bounded SELECT on the object's own connection, and write the
    /// rows straight to the chosen file — no manual export step.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn save_table_data_to_file(
        &mut self,
        schema: String,
        name: String,
        format: crate::schema_tree::TableSaveFormat,
        cx: &mut Context<Self>,
    ) {
        use crate::schema_tree::TableSaveFormat;
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.files.save_data_no_connection"),
            );
            cx.notify();
            return;
        };
        let qt = crate::table_actions::QualifiedTable::new(&name).with_schema(&schema);
        let db_type = profile.database_type;
        const SAVE_DATA_ROW_CAP: usize = 10_000;
        let select_sql =
            crate::table_actions::select_top_rows_sql_for(db_type, SAVE_DATA_ROW_CAP, &qt);
        let extension = match format {
            TableSaveFormat::Csv => "csv",
            TableSaveFormat::Excel => "xlsx",
            TableSaveFormat::SqllyExport => "sqllyexport",
            _ => return,
        };
        let default_name = format!("{name}.{extension}");
        let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some(&default_name));
        let services = self.services_for_profile(&profile);
        let identifier = qt.multipart_identifier();
        let csv_opts = crate::results::adhoc_export_options(
            ExportFormat::Csv,
            &crate::preferences::ExportPreferences::load(),
        );
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            {
                let mut cx = async_cx.clone();
                let identifier = identifier.clone();
                weak.update(&mut cx, |this, cx| {
                    this.set_status(
                        StatusLevel::Busy,
                        crate::localization::format(
                            "shell.files.save_data_exporting",
                            &[("identifier", &identifier)],
                        ),
                    );
                    cx.notify();
                })
                .ok();
            }
            let (tx, result_rx) =
                tokio::sync::oneshot::channel::<Result<(usize, bool), TableWorkFailure>>();
            let write_path = path.clone();
            crate::tokio_handle().spawn(async move {
                let result = async {
                    // Full values: a saved file must hold every cell whole.
                    let handle = services
                        .queries
                        .execute_full_values(&profile, None, &select_sql, 256)
                        .await
                        .map_err(|e| TableWorkFailure::SaveQuery(e.message()))?;
                    let (columns, rows) = collect_table_rows(handle).await?;
                    let row_count = rows.len();
                    let truncated = row_count >= SAVE_DATA_ROW_CAP;
                    let bytes: Vec<u8> = match format {
                        TableSaveFormat::Csv => {
                            crate::result_export::csv_with(&columns, &rows, &csv_opts).into_bytes()
                        }
                        TableSaveFormat::Excel => {
                            crate::result_export_xlsx::excel_xlsx(&columns, &rows, &name, &name)
                        }
                        TableSaveFormat::SqllyExport => {
                            crate::result_export::sqlly_export_envelope(db_type, &columns, &rows)
                                .into_bytes()
                        }
                        _ => Vec::new(),
                    };
                    std::fs::write(&write_path, bytes).map_err(|e| {
                        TableWorkFailure::Message(crate::localization::deferred(
                            "shell.files.save_data_write_failed",
                            &[
                                ("path", write_path.display().to_string()),
                                ("error", e.to_string()),
                            ],
                        ))
                    })?;
                    Ok((row_count, truncated))
                }
                .await;
                let _ = tx.send(result);
            });
            let Ok(result) = result_rx.await else {
                return;
            };
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                match result {
                    Ok((rows, truncated)) => {
                        let saved = if truncated {
                            crate::localization::format(
                                "shell.files.save_data_saved_truncated",
                                &[
                                    ("rows", &rows.to_string()),
                                    ("identifier", &identifier),
                                    ("path", &path.display().to_string()),
                                    ("count", &SAVE_DATA_ROW_CAP.to_string()),
                                ],
                            )
                        } else {
                            crate::localization::format(
                                "shell.files.save_data_saved",
                                &[
                                    ("rows", &rows.to_string()),
                                    ("identifier", &identifier),
                                    ("path", &path.display().to_string()),
                                ],
                            )
                        };
                        this.set_status(StatusLevel::Success, saved.clone());
                        this.record_export_saved(saved, cx);
                    }
                    Err(failure) => this.set_status(StatusLevel::Error, failure.text()),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn run_script_table_as(
        &mut self,
        schema: String,
        name: String,
        kind: crate::schema_tree::TableScriptKind,
        cx: &mut Context<Self>,
    ) {
        let Some(profile) = self.active_profile.clone() else {
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.files.script_no_connection"),
            );
            cx.notify();
            return;
        };

        let qt = crate::table_actions::QualifiedTable::new(&name).with_schema(&schema);
        let db_type = profile.database_type;
        let select_sql = format!("SELECT * FROM {}", qt.schema_qualified_table_for(db_type));
        // The script's SELECT runs on the server, so it follows the profile's
        // route like any other query.
        let services = self.services_for_profile(&profile);
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<String, TableWorkFailure>>();
        let script_kind_label = match &kind {
            crate::schema_tree::TableScriptKind::Insert => "INSERT".to_string(),
            crate::schema_tree::TableScriptKind::Merge => "MERGE".to_string(),
            crate::schema_tree::TableScriptKind::TempTable => {
                crate::localization::text("shell.files.script_kind_temp_table")
            }
        };

        self.set_status(
            StatusLevel::Busy,
            crate::localization::format(
                "shell.files.script_generating",
                &[
                    ("kind", &script_kind_label),
                    ("table", &qt.multipart_identifier()),
                ],
            ),
        );
        cx.notify();

        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    // MERGE scripts prefer the table's real primary key as the
                    // match key; discovery is best-effort (an error simply
                    // falls back to the first column, called out in the
                    // generated script).
                    let key_columns: Vec<String> =
                        if matches!(kind, crate::schema_tree::TableScriptKind::Merge) {
                            let database = profile.database.clone().unwrap_or_default();
                            match services
                                .metadata
                                .list_primary_keys(&profile, &database)
                                .await
                            {
                                Ok(mut pk_rows) => {
                                    pk_rows.retain(|row| {
                                        row.table.eq_ignore_ascii_case(&qt.table)
                                            && (qt.schema.is_none()
                                                || qt.schema.as_deref().is_some_and(|schema| {
                                                    row.schema.eq_ignore_ascii_case(schema)
                                                }))
                                    });
                                    pk_rows.sort_by_key(|row| row.key_ordinal);
                                    pk_rows.into_iter().map(|row| row.column).collect()
                                }
                                Err(_) => Vec::new(),
                            }
                        } else {
                            Vec::new()
                        };
                    // Full values: a data script must INSERT every value whole.
                    let result = match services
                        .queries
                        .execute_full_values(&profile, None, &select_sql, 256)
                        .await
                    {
                        Ok(handle) => {
                            collect_table_script(handle, db_type, &qt, &kind, &key_columns).await
                        }
                        Err(e) => Err(TableWorkFailure::ScriptQuery(e.message())),
                    };
                    let _ = tx.send(result);
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Ok(script) = rx.await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        match script {
                            Ok(sql) => {
                                // A new tab, for the same reason
                                // `OpenSqlInNewTab` uses one: writing the script
                                // into the focused editor would clear its undo
                                // stack and take unsaved work with it.
                                this.new_tab(cx, Some(sql));
                                this.set_status(
                                    StatusLevel::Success,
                                    crate::localization::format(
                                        "shell.files.script_generated",
                                        &[("kind", &script_kind_label)],
                                    ),
                                );
                            }
                            Err(failure) => {
                                this.set_status(StatusLevel::Error, failure.text());
                            }
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    /// Remember an opened SQL file at the front of the recent-files list.
    pub(super) fn note_recent_file(&mut self, path: &str) {
        self.recent_files.retain(|existing| existing != path);
        self.recent_files.insert(0, path.to_string());
        self.recent_files.truncate(RECENT_FILES_CAP);
        save_recent_files(&self.recent_files);
    }

    /// Open a recent file directly (from the tab context menu's Recent
    /// Files section). Missing files are dropped from the list.
    pub(super) fn open_recent_file(&mut self, path: String, cx: &mut Context<Self>) {
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                self.finish_opening_sql_file(std::path::PathBuf::from(&path), content, cx)
            }
            Err(_) => {
                self.recent_files.retain(|existing| existing != &path);
                save_recent_files(&self.recent_files);
                self.set_status(
                    StatusLevel::Warning,
                    crate::localization::format(
                        "shell.files.recent_file_missing",
                        &[("path", &path)],
                    ),
                );
                cx.notify();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn handle_sql_library_event(
        &mut self,
        event: &crate::sql_library::SqlLibraryEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        use crate::sql_library::SqlLibraryEvent;
        match event {
            SqlLibraryEvent::OpenFile(path, fm) => match std::fs::read_to_string(path) {
                Ok(content) => {
                    self.finish_opening_sql_file(path.clone(), content, cx);
                    self.apply_library_front_matter(fm, cx);
                }
                Err(error) => {
                    self.set_status(
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.files.library_open_failed",
                            &[
                                ("path", &path.display().to_string()),
                                ("error", &error.to_string()),
                            ],
                        ),
                    );
                    cx.notify();
                }
            },
            SqlLibraryEvent::SaveActiveTabRequested { folder } => {
                self.save_active_tab_to_library_folder(folder.clone(), cx);
            }
            SqlLibraryEvent::RootChangeRequested => {
                self.handle_sql_library_root_change(cx);
            }
            SqlLibraryEvent::Refreshed => {}
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn apply_library_front_matter(
        &mut self,
        fm: &crate::sql_library_model::FrontMatter,
        cx: &mut Context<Self>,
    ) {
        let Some(name) = fm.connection.as_deref() else {
            return;
        };
        let Some(mut profile) = crate::connection::ConnectionStore::global()
            .list()
            .into_iter()
            .find(|p| p.name == name)
        else {
            self.set_status(
                StatusLevel::Info,
                crate::localization::format(
                    "shell.files.library_connection_missing",
                    &[("name", name)],
                ),
            );
            cx.notify();
            return;
        };
        if let Some(database) = fm.database.clone() {
            profile.database = Some(database);
        }
        match super::retarget::retarget_decision(self.active_profile.as_ref(), &profile) {
            super::retarget::RetargetDecision::Connect => {
                self.connect_profile_then(profile, cx, |_app, _cx| {});
            }
            super::retarget::RetargetDecision::SwitchDatabase(db) => {
                self.switch_database(db, cx);
            }
            super::retarget::RetargetDecision::AlreadyActive => {}
        }
    }

    pub(super) fn handle_save_to_library(
        &mut self,
        _action: &SaveToLibrary,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.library_save_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let folder = crate::preferences::SqlLibraryPreferences::load().effective_root();
            self.save_active_tab_to_library_folder(folder, cx);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn save_active_tab_to_library_folder(&mut self, folder: PathBuf, cx: &mut Context<Self>) {
        use crate::sql_library_model::{
            FRONT_MATTER_PREFIX, FrontMatter, render_front_matter, unique_file_name,
        };

        let (editor_entity, tab_name) = {
            let Some(tab) = self.tabs.get(self.active_tab) else {
                return;
            };
            (tab.editor.clone(), tab.name.clone())
        };
        let fmt_prefs = crate::preferences::load_sql_formatting_preferences();
        if fmt_prefs.format_on_save {
            editor_entity.update(cx, |ed, cx| ed.format_sql_document_with(&fmt_prefs, cx));
        }
        let mut text = editor_entity.read(cx).text().to_string();
        let fm = FrontMatter {
            connection: self.active_profile.as_ref().map(|p| p.name.clone()),
            database: self
                .active_profile
                .as_ref()
                .and_then(|p| p.database.clone()),
        };
        if !text.starts_with(FRONT_MATTER_PREFIX)
            && let Some(line) = render_front_matter(&fm)
        {
            text = format!("{line}\n{text}");
            editor_entity.update(cx, |ed, cx| ed.set_text(text.clone(), cx));
        }
        let _ = std::fs::create_dir_all(&folder);
        let path = unique_file_name(&folder, &tab_name);
        match std::fs::write(&path, &text) {
            Ok(()) => {
                if let Some(tab) = self.tabs.get_mut(self.active_tab) {
                    let old_key = tab.metadata_key();
                    tab.file_path = Some(path.to_string_lossy().to_string());
                    tab.dirty = false;
                    tab.record_clean_disk(&path);
                    let new_key = tab.metadata_key();
                    if old_key != new_key {
                        self.ai_summary_metadata.migrate(&old_key, &new_key);
                    }
                }
                self.handle_file_backing_changed(self.active_tab, cx);
                self.sidebar
                    .update(cx, |sidebar, cx| sidebar.rescan_library(cx));
                self.set_status(
                    StatusLevel::Success,
                    crate::localization::format(
                        "shell.files.library_saved",
                        &[("path", &path.display().to_string())],
                    ),
                );
            }
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.files.library_save_failed",
                        &[("error", &error.to_string())],
                    ),
                );
            }
        }
        cx.notify();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn handle_sql_library_root_change(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(crate::localization::text("shell.files.library_choose_folder").into()),
        });
        let weak_self = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(folder) = paths.into_iter().next()
            {
                crate::sandbox_access::remember(&folder);
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        let mut prefs = crate::preferences::SqlLibraryPreferences::load();
                        prefs.root = Some(folder.to_string_lossy().into_owned());
                        prefs.save();
                        this.sidebar.update(cx, |sidebar, cx| {
                            sidebar.set_library_root(folder, cx);
                        });
                        this.save_workspace_session(cx);
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ensure_file_watch_ticker(&mut self, cx: &mut Context<Self>) {
        if self.file_watch_ticker.is_some() {
            return;
        }
        if !self.tabs.iter().any(|tab| tab.is_watchable()) {
            return;
        }
        if self.file_watch.is_none() {
            match crate::file_watch::FileWatchHub::new() {
                Ok(hub) => self.file_watch = Some(hub),
                Err(err) => {
                    tracing::warn!("file watcher unavailable: {err}");
                    return;
                }
            }
        }
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        self.file_watch_ticker = Some(cx.spawn(async move |_this, _cx| {
            loop {
                async_cx
                    .background_executor()
                    .timer(std::time::Duration::from_millis(500))
                    .await;
                let mut cx = async_cx.clone();
                match weak.update(&mut cx, |this, cx| this.file_watch_tick(cx)) {
                    Ok(true) => {}
                    Ok(false) | Err(_) => break,
                }
            }
        }));
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn file_watch_tick(&mut self, cx: &mut Context<Self>) -> bool {
        use crate::file_watch::{DiskState, settle, snapshot};
        use std::path::PathBuf;

        let paths: Vec<PathBuf> = self
            .tabs
            .iter()
            .filter(|tab| tab.is_watchable())
            .filter_map(|tab| tab.file_path.as_ref().map(PathBuf::from))
            .collect();
        if paths.is_empty() {
            self.file_watch_ticker = None;
            return false;
        }
        for tab in &mut self.tabs {
            if tab.is_watchable()
                && tab.disk_snapshot.is_none()
                && let Some(path) = tab.file_path.clone()
            {
                tab.record_clean_disk(std::path::Path::new(&path));
            }
        }
        let drained = {
            let Some(hub) = self.file_watch.as_mut() else {
                return false;
            };
            hub.sync(paths);
            hub.drain()
        };
        if drained.is_empty() {
            return true;
        }
        let mut changed = false;
        let active = self.active_tab;
        for i in 0..self.tabs.len() {
            if !self.tabs[i].is_watchable() {
                continue;
            }
            let Some(path_str) = self.tabs[i].file_path.clone() else {
                continue;
            };
            let path = PathBuf::from(&path_str);
            if !drained.iter().any(|event| file_paths_match(&path, event)) {
                continue;
            }
            let current = snapshot(&path);
            let Some(recorded) = self.tabs[i].disk_snapshot else {
                continue;
            };
            let (classified, refreshed) = settle(&path, &recorded, &current);
            if let Some(refreshed) = refreshed {
                self.tabs[i].disk_snapshot = Some(refreshed);
            }
            if self.tabs[i].disk_state != classified {
                self.tabs[i].disk_state = classified;
                changed = true;
            }
            if classified == DiskState::Clean {
                self.tabs[i].disk_prompt_ack = None;
                continue;
            }
            if self.tabs[i].disk_prompt_ack.as_ref() == Some(&current) {
                continue;
            }
            if i == active && self.pending.disk_change.is_none() {
                self.pending.disk_change = Some(PendingDiskChange {
                    tab_id: self.tabs[i].id.clone(),
                    state: classified,
                    current,
                });
                changed = true;
            }
        }
        if changed {
            cx.notify();
        }
        true
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn maybe_queue_disk_prompt_for_tab(&mut self, index: usize) {
        use crate::file_watch::{DiskState, snapshot};
        use std::path::PathBuf;

        if self.pending.disk_change.is_some() {
            return;
        }
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        if tab.disk_state == DiskState::Clean {
            return;
        }
        let Some(path_str) = tab.file_path.as_ref() else {
            return;
        };
        let current = snapshot(&PathBuf::from(path_str));
        if tab.disk_prompt_ack.as_ref() == Some(&current) {
            return;
        }
        self.pending.disk_change = Some(PendingDiskChange {
            tab_id: tab.id.clone(),
            state: tab.disk_state,
            current,
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn materialize_disk_change_prompt(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.pending.disk_prompt_open {
            return;
        }
        let Some(pending) = self.pending.disk_change.clone() else {
            return;
        };
        let Some(index) = self.tab_index_by_id(&pending.tab_id) else {
            self.pending.disk_change = None;
            return;
        };
        if index != self.active_tab {
            self.pending.disk_change = None;
            return;
        }
        let dirty = self.tabs.get(index).is_some_and(|tab| tab.dirty);
        let name = self
            .tabs
            .get(index)
            .map(|tab| tab.name.clone())
            .unwrap_or_else(|| crate::localization::text("tabs.close.fallback_name"));
        let spec = disk_prompt_spec(pending.state, dirty, &name);
        let buttons: Vec<PromptButton> = spec
            .buttons
            .iter()
            .map(|label| PromptButton::new(label.clone()))
            .collect();
        let rx = window.prompt(
            spec.level,
            &spec.message,
            Some(spec.detail.as_str()),
            &buttons,
            cx,
        );
        self.pending.disk_prompt_open = true;
        let tab_id = pending.tab_id.clone();
        let state = pending.state;
        let current = pending.current;
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        cx.spawn(async move |_this, _cx| {
            let choice = rx.await;
            let mut cx = async_cx.clone();
            weak.update(&mut cx, |this, cx| {
                this.pending.disk_prompt_open = false;
                this.pending.disk_change = None;
                this.apply_disk_prompt_choice(tab_id, state, current, choice.ok(), cx);
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn apply_disk_prompt_choice(
        &mut self,
        tab_id: String,
        state: crate::file_watch::DiskState,
        current: crate::file_watch::WatchSnapshot,
        choice: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        use crate::file_watch::DiskState;

        let Some(index) = self.tab_index_by_id(&tab_id) else {
            return;
        };
        match state {
            DiskState::ModifiedOnDisk => match choice {
                Some(0) => self.reload_tab_from_disk(index, cx),
                _ => {
                    if let Some(tab) = self.tabs.get_mut(index) {
                        tab.disk_prompt_ack = Some(current);
                    }
                    cx.notify();
                }
            },
            DiskState::DeletedOnDisk => match choice {
                Some(1) => self.close_tab(index, cx),
                _ => {
                    if let Some(tab) = self.tabs.get_mut(index) {
                        tab.disk_prompt_ack = Some(current);
                    }
                    cx.notify();
                }
            },
            DiskState::Clean => {}
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn reload_tab_from_disk(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(path) = self.tabs.get(index).and_then(|tab| tab.file_path.clone()) else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let editor = self.tabs[index].editor.clone();
                editor.update(cx, |ed, cx| ed.set_text(text, cx));
                if let Some(tab) = self.tabs.get_mut(index) {
                    tab.dirty = false;
                    tab.record_clean_disk(std::path::Path::new(&path));
                }
                cx.notify();
            }
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.files.reload_failed",
                        &[("error", &error.to_string())],
                    ),
                );
                cx.notify();
            }
        }
    }

    pub(super) fn open_notebook(&mut self, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            self.set_status(
                StatusLevel::Error,
                crate::browser_preview::unavailable(&crate::localization::text(
                    "shell.files.notebook_open_disk_unavailable",
                ))
                .to_string(),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let prompt: SharedString =
                crate::localization::text("shell.files.open_notebook_prompt").into();
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: Some(prompt),
            });
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                {
                    crate::sandbox_access::remember(&path);
                    let mut cx = async_cx.clone();
                    weak_self
                        .update(&mut cx, |this, cx| {
                            this.open_notebook_from_path(&path.to_string_lossy(), cx);
                        })
                        .ok();
                }
            })
            .detach();
        }
    }

    pub(super) fn open_notebook_from_path(&mut self, path: &str, cx: &mut Context<Self>) -> bool {
        match std::fs::read_to_string(path) {
            Ok(text) => match crate::notebook_model::Notebook::from_file_text(path, &text) {
                Ok(model) => {
                    self.note_recent_file(path);
                    self.new_notebook_tab(model, Some(path.to_string()), cx);
                    true
                }
                Err(message) => {
                    self.set_status(StatusLevel::Error, message);
                    cx.notify();
                    false
                }
            },
            Err(error) => {
                self.set_status(
                    StatusLevel::Error,
                    crate::localization::format(
                        "shell.files.notebook_open_failed",
                        &[("error", &error.to_string())],
                    ),
                );
                cx.notify();
                false
            }
        }
    }

    pub(super) fn save_notebook(&mut self, tab_idx: usize, save_as: bool, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (tab_idx, save_as);
            self.set_status(
                StatusLevel::Error,
                crate::localization::text("shell.files.notebook_save_unavailable"),
            );
            cx.notify();
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(tab) = self.tabs.get(tab_idx) else {
                return;
            };
            let Some(notebook) = tab.notebook.clone() else {
                return;
            };
            let model = notebook.read(cx).model().clone();
            if let Some(path) = tab.notebook_path.clone().filter(|_| !save_as) {
                match std::fs::write(&path, model.to_file_text(&path)) {
                    Ok(()) => {
                        if let Some(tab) = self.tabs.get_mut(tab_idx) {
                            tab.dirty = false;
                        }
                        notebook.update(cx, |nb, _| nb.clear_dirty());
                        self.set_status(
                            StatusLevel::Success,
                            crate::localization::format(
                                "shell.files.notebook_saved_to_path",
                                &[("path", &path.to_string())],
                            ),
                        );
                        self.save_workspace_session(cx);
                        cx.notify();
                    }
                    Err(error) => {
                        self.set_status(
                            StatusLevel::Error,
                            crate::localization::format(
                                "shell.files.notebook_save_failed",
                                &[("error", &error.to_string())],
                            ),
                        );
                        cx.notify();
                    }
                }
                return;
            }
            // A notebook opened from Jupyter keeps offering its own format.
            let extension = if tab
                .notebook_path
                .as_deref()
                .is_some_and(crate::notebook_model::is_ipynb_path)
            {
                crate::notebook_model::IPYNB_EXTENSION
            } else {
                crate::notebook_model::NOTEBOOK_EXTENSION
            };
            let rx = cx.prompt_for_new_path(
                std::path::Path::new("."),
                Some(&format!("notebook.{extension}")),
            );
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    match std::fs::write(&path, model.to_file_text(&path.to_string_lossy())) {
                        Ok(()) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    if let Some(tab) = this.tabs.get_mut(tab_idx) {
                                        tab.notebook_path =
                                            Some(path.to_string_lossy().to_string());
                                        tab.dirty = false;
                                        // Named after its file from now on.
                                        tab.auto_named = false;
                                        tab.auto_number = None;
                                        if !tab.user_named {
                                            tab.name = path
                                                .file_stem()
                                                .map(|s| s.to_string_lossy().into_owned())
                                                .unwrap_or_else(|| tab.name.clone());
                                        }
                                    }
                                    if let Some(nb) =
                                        this.tabs.get(tab_idx).and_then(|t| t.notebook.clone())
                                    {
                                        nb.update(cx, |nb, _| nb.clear_dirty());
                                    }
                                    this.set_status(
                                        StatusLevel::Success,
                                        crate::localization::format(
                                            "shell.files.notebook_saved_to_path",
                                            &[("path", &path.display().to_string())],
                                        ),
                                    );
                                    this.save_workspace_session(cx);
                                    cx.notify();
                                })
                                .ok();
                        }
                        Err(error) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.set_status(
                                        StatusLevel::Error,
                                        crate::localization::format(
                                            "shell.files.notebook_save_failed",
                                            &[("error", &error.to_string())],
                                        ),
                                    );
                                    cx.notify();
                                })
                                .ok();
                        }
                    }
                }
            })
            .detach();
        }
    }

    /// The browser preview can't write files, so both notebook exports say so.
    #[cfg(target_arch = "wasm32")]
    fn notebook_export_unavailable(&mut self, cx: &mut Context<Self>) {
        self.set_status(
            StatusLevel::Error,
            crate::browser_preview::unavailable(&crate::localization::text(
                "shell.files.notebook_export_unavailable",
            ))
            .to_string(),
        );
        cx.notify();
    }

    /// Writes a copy of the notebook as a Jupyter `.ipynb` file (Azure Data
    /// Studio, VS Code, Jupyter) without changing where the tab saves.
    pub(super) fn export_notebook_ipynb(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = tab_idx;
            self.notebook_export_unavailable(cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(notebook) = self.tabs.get(tab_idx).and_then(|t| t.notebook.clone()) else {
                return;
            };
            let model = notebook.read(cx).model();
            let text = model.to_ipynb();
            let cell_count = model.cells.len();
            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("notebook.ipynb"));
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                let Ok(Ok(Some(path))) = rx.await else {
                    return;
                };
                let (level, message) = match std::fs::write(&path, &text) {
                    Ok(()) => (
                        StatusLevel::Success,
                        crate::localization::format(
                            "shell.files.notebook_exported_ipynb",
                            &[
                                ("count", &cell_count.to_string()),
                                ("path", &path.display().to_string()),
                            ],
                        ),
                    ),
                    Err(error) => (
                        StatusLevel::Error,
                        crate::localization::format(
                            "shell.files.notebook_export_failed",
                            &[("error", &error.to_string())],
                        ),
                    ),
                };
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.set_status(level, message);
                        cx.notify();
                    })
                    .ok();
            })
            .detach();
        }
    }

    pub(super) fn export_notebook_markdown(&mut self, tab_idx: usize, cx: &mut Context<Self>) {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = tab_idx;
            self.notebook_export_unavailable(cx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(tab) = self.tabs.get(tab_idx) else {
                return;
            };
            let Some(notebook) = tab.notebook.clone() else {
                return;
            };
            let title = tab.name.clone();
            let tables = notebook.read(cx).result_markdown_tables(cx);
            let results: Vec<crate::notebook_model::MarkdownCellResult<'_>> = tables
                .iter()
                .map(|(id, table)| crate::notebook_model::MarkdownCellResult {
                    cell_id: id,
                    table: table.clone(),
                })
                .collect();
            let markdown =
                crate::notebook_model::export_markdown(notebook.read(cx).model(), &title, &results);
            let cell_count = notebook.read(cx).model().cells.len();
            let rx = cx.prompt_for_new_path(std::path::Path::new("."), Some("notebook.md"));
            let weak_self = cx.weak_entity();
            let async_cx = cx.to_async();
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    match std::fs::write(&path, &markdown) {
                        Ok(()) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.set_status(
                                        StatusLevel::Success,
                                        crate::localization::format(
                                            "shell.files.notebook_exported",
                                            &[("count", &cell_count.to_string())],
                                        ),
                                    );
                                    cx.notify();
                                })
                                .ok();
                        }
                        Err(error) => {
                            let mut cx = async_cx.clone();
                            weak_self
                                .update(&mut cx, |this, cx| {
                                    this.set_status(
                                        StatusLevel::Error,
                                        crate::localization::format(
                                            "shell.files.notebook_export_failed",
                                            &[("error", &error.to_string())],
                                        ),
                                    );
                                    cx.notify();
                                })
                                .ok();
                        }
                    }
                }
            })
            .detach();
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod dropped_path_tests {
    use super::*;

    #[test]
    fn classify_dropped_path_recognises_sql_and_database_files() {
        assert_eq!(
            classify_dropped_path(std::path::Path::new("/tmp/demo.sqlite")),
            DroppedKind::Database
        );
        assert_eq!(
            classify_dropped_path(std::path::Path::new("/tmp/analytics.DuckDB")),
            DroppedKind::Database
        );
        assert_eq!(
            classify_dropped_path(std::path::Path::new("/tmp/query.sql")),
            DroppedKind::Sql
        );
        assert_eq!(
            classify_dropped_path(std::path::Path::new("/tmp/readme.txt")),
            DroppedKind::Other
        );
    }
}

#[cfg(test)]
mod disk_prompt_tests {
    use super::*;
    use crate::file_watch::DiskState;

    #[test]
    fn modified_prompt_warns_about_unsaved_edits_only_when_dirty() {
        let dirty = disk_prompt_spec(DiskState::ModifiedOnDisk, true, "Orders");
        assert_eq!(
            dirty.detail,
            crate::localization::text("shell.files.disk_modified_detail_dirty")
        );
        assert_eq!(
            dirty.buttons,
            vec![
                crate::localization::text("shell.files.disk_reload_button"),
                crate::localization::text("shell.files.disk_keep_button"),
            ]
        );
        let clean = disk_prompt_spec(DiskState::ModifiedOnDisk, false, "Orders");
        assert_eq!(
            clean.detail,
            crate::localization::text("shell.files.disk_modified_detail_clean")
        );
        assert_eq!(clean.buttons, dirty.buttons);
    }

    #[test]
    fn deleted_prompt_offers_keep_and_close() {
        let spec = disk_prompt_spec(DiskState::DeletedOnDisk, true, "Orders");
        assert_eq!(
            spec.buttons,
            vec![
                crate::localization::text("shell.files.disk_keep_button"),
                crate::localization::text("shell.files.disk_close_button"),
            ]
        );
        assert!(matches!(spec.level, gpui::PromptLevel::Warning));
        assert_eq!(
            spec.message,
            crate::localization::format("shell.files.disk_deleted_message", &[("name", "Orders")])
        );
    }

    #[cfg(all(unix, not(target_arch = "wasm32")))]
    #[test]
    fn redis_insight_private_keys_are_written_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("sqlly-ri-pem-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key = sqlly_core::tool_import::redis_insight::PemFile {
            path: dir.join("k.pem"),
            contents: "-----BEGIN PRIVATE KEY-----\n".into(),
            private: true,
        };
        let ca = sqlly_core::tool_import::redis_insight::PemFile {
            path: dir.join("ca.pem"),
            contents: "-----BEGIN CERTIFICATE-----\n".into(),
            private: false,
        };
        super::write_pem(&key).unwrap();
        super::write_pem(&ca).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&key.path), 0o600);
        assert_eq!(
            std::fs::read_to_string(&ca.path).unwrap(),
            "-----BEGIN CERTIFICATE-----\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
