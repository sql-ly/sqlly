//! Export database as SQL — interactive dump dialog and schedule editor.

use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_lang::sqlmodel::ConnectionKind;
use sqlly_services::{
    DumpContent, DumpLayout, DumpOptions, DumpPhase, DumpProgress, DumpReport, NativeDumpRequest,
    NativeTool, PgFormat, find_tool, run_database_dump, run_native_dump,
};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::dump_schedule::{Cadence, DumpSchedule, QueryExportFormat, ScheduleKind};
use crate::preferences::native_tool_locations;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

const BATCH_SIZE_MIN: usize = 1;
const BATCH_SIZE_MAX: usize = 5000;
const OPEN_IN_TAB_MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DumpDialogMode {
    Export,
    Schedule,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DumpEngine {
    #[default]
    BuiltIn,
    Native,
}

/// What a schedule runs — the Schedule-mode counterpart of [`DumpEngine`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ScheduleTask {
    #[default]
    BuiltinDump,
    NativeBackup,
    QueryExport,
}

/// Form state distilled for validation and dump execution.
#[derive(Clone, Debug)]
pub struct DatabaseDumpModel {
    pub database: String,
    pub path: String,
    pub options: DumpOptions,
    pub cadence: Cadence,
    pub retention: usize,
    pub engine: DumpEngine,
    pub pg_format: PgFormat,
    pub pg_no_owner: bool,
    pub mysql_single_transaction: bool,
    /// Extra mysqldump options exactly as typed; split and checked by
    /// [`Self::mysql_extra_args`].
    pub mysql_extra_options: String,
    /// Remove DEFINER clauses from the finished mysqldump file.
    pub mysql_strip_definer: bool,
    pub task: ScheduleTask,
    /// Saved query for a QueryExport schedule: its path relative to the SQL
    /// library root, and its display name for file naming.
    pub query_path: String,
    pub query_name: String,
    pub export_format: QueryExportFormat,
}

impl Default for DatabaseDumpModel {
    fn default() -> Self {
        Self {
            database: String::new(),
            path: String::new(),
            options: DumpOptions::default(),
            cadence: Cadence::Daily,
            retention: 5,
            engine: DumpEngine::BuiltIn,
            pg_format: PgFormat::Custom,
            pg_no_owner: true,
            mysql_single_transaction: true,
            mysql_extra_options: String::new(),
            mysql_strip_definer: false,
            task: ScheduleTask::BuiltinDump,
            query_path: String::new(),
            query_name: String::new(),
            export_format: QueryExportFormat::Csv,
        }
    }
}

impl DatabaseDumpModel {
    /// The typed extra mysqldump options as separate argv entries, or the
    /// reason they are refused (unclosed quote, not a `--option`, or an
    /// option SQLLY controls such as the output file or credentials).
    pub fn mysql_extra_args(&self) -> Result<Vec<String>, String> {
        sqlly_services::parse_mysqldump_extra_args(&self.mysql_extra_options)
            .map_err(crate::localization::message)
    }

    /// Load saved extra options back into the text field: entries holding
    /// whitespace or quotes are re-quoted so they split the same way again.
    pub fn set_mysql_extra_args(&mut self, args: &[String]) {
        self.mysql_extra_options = args
            .iter()
            .map(|arg| {
                if arg
                    .chars()
                    .any(|c| c.is_whitespace() || c == '"' || c == '\'')
                {
                    format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
                } else {
                    arg.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
    }

    pub fn validate(&self, native_tool_found: bool, mode: DumpDialogMode) -> Result<(), String> {
        if self.database.trim().is_empty() {
            return Err(crate::localization::text(
                "explorer.database_dump.database_required",
            ));
        }
        let path = self.path.trim();
        if path.is_empty() {
            return Err(crate::localization::text(
                "explorer.database_dump.output_required",
            ));
        }
        let native = match mode {
            DumpDialogMode::Export => self.engine == DumpEngine::Native,
            DumpDialogMode::Schedule => self.task == ScheduleTask::NativeBackup,
        };
        let query_export =
            mode == DumpDialogMode::Schedule && self.task == ScheduleTask::QueryExport;
        let path_buf = PathBuf::from(path);
        if mode == DumpDialogMode::Schedule {
            // Every run writes a new timestamped file (or per-object folder)
            // into this folder, which the run creates when missing.
            if path_buf.exists() && !path_buf.is_dir() {
                return Err(crate::localization::text(
                    "explorer.database_dump.schedule_folder_required",
                ));
            }
            if !path_buf.exists()
                && path_buf
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| {
                        ["sql", "dump", "tar", "csv", "jsonl", "gz"]
                            .contains(&e.to_ascii_lowercase().as_str())
                    })
            {
                return Err(crate::localization::text(
                    "explorer.database_dump.schedule_folder_not_file",
                ));
            }
        }
        if query_export {
            if self.query_path.trim().is_empty() {
                return Err(crate::localization::text(
                    "explorer.database_dump.saved_query_required",
                ));
            }
            return Ok(());
        }
        if !native
            && (self.options.batch_size < BATCH_SIZE_MIN
                || self.options.batch_size > BATCH_SIZE_MAX)
        {
            return Err(crate::localization::format(
                "explorer.database_dump.batch_size_invalid",
                &[
                    ("minimum", &BATCH_SIZE_MIN.to_string()),
                    ("maximum", &BATCH_SIZE_MAX.to_string()),
                ],
            ));
        }
        if mode == DumpDialogMode::Schedule {
            // Folder already checked above; the run names the file.
        } else if self.options.layout == DumpLayout::FilePerObject {
            if path_buf.extension().is_some_and(|e| e == "sql") {
                return Err(crate::localization::text(
                    "explorer.database_dump.per_object_sql_extension",
                ));
            }
            if path_buf.exists() && !path_buf.is_dir() {
                return Err(crate::localization::text(
                    "explorer.database_dump.per_object_directory_required",
                ));
            }
        } else if path_buf.exists() && path_buf.is_dir() {
            return Err(crate::localization::text(
                "explorer.database_dump.single_file_required",
            ));
        }
        if native {
            if self.options.layout != DumpLayout::SingleFile {
                return Err(crate::localization::text(
                    "explorer.database_dump.native_single_file_required",
                ));
            }
            if !native_tool_found {
                return Err(crate::localization::text(
                    "explorer.database_dump.native_tool_missing",
                ));
            }
        }
        Ok(())
    }

    pub fn suggested_path(
        database: &str,
        layout: DumpLayout,
        engine: DumpEngine,
        pg_format: PgFormat,
        kind: ConnectionKind,
    ) -> String {
        if database.is_empty() {
            return String::new();
        }
        let safe = database
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        if layout == DumpLayout::FilePerObject {
            format!("/tmp/sqlly-dumps/{safe}")
        } else if engine == DumpEngine::Native {
            let ext = match kind {
                ConnectionKind::PostgresWire => match pg_format {
                    PgFormat::Custom => "dump",
                    PgFormat::Tar => "tar",
                    PgFormat::Plain => "sql",
                },
                _ => "sql",
            };
            format!("/tmp/sqlly-dumps/{safe}.{ext}")
        } else {
            format!("/tmp/sqlly-dumps/{safe}.sql")
        }
    }

    /// Default output folder for a scheduled task: a folder under the home
    /// directory (not `/tmp`, which the OS clears), created by the first run.
    pub fn schedule_default_dir() -> String {
        dirs::home_dir()
            .map(|home| home.join("SQLLY Backups"))
            .unwrap_or_else(|| std::env::temp_dir().join("sqlly-dumps"))
            .display()
            .to_string()
    }

    pub fn to_options(&self) -> DumpOptions {
        self.options.clone()
    }
}

#[derive(Clone, Debug, Default)]
struct DumpProgressLine {
    phase: Option<DumpPhase>,
    object: String,
    objects_done: usize,
    objects_total: usize,
    rows_written: u64,
}

impl EventEmitter<ToolDialogEvent> for DatabaseDumpDialog {}

pub struct DatabaseDumpDialog {
    context: ToolDialogContext,
    mode: DumpDialogMode,
    /// The schedule's id: the edited one's, or a fresh one fixed at open.
    schedule_id: uuid::Uuid,
    model: DatabaseDumpModel,
    databases: Vec<String>,
    databases_loading: bool,
    load_error: Option<String>,
    db_select: Entity<SelectState<Vec<String>>>,
    path_input: Entity<InputState>,
    batch_input: Entity<InputState>,
    retention_input: Entity<InputState>,
    /// Extra mysqldump options (MySQL/MariaDB native dumps only).
    mysql_options_input: Entity<InputState>,
    content_select: Entity<SelectState<Vec<String>>>,
    engine_select: Entity<SelectState<Vec<String>>>,
    format_select: Entity<SelectState<Vec<String>>>,
    cadence_select: Entity<SelectState<Vec<String>>>,
    task_select: Entity<SelectState<Vec<String>>>,
    query_select: Entity<SelectState<Vec<String>>>,
    export_format_select: Entity<SelectState<Vec<String>>>,
    /// SQL library entries offered to QueryExport schedules: (label, library-relative path).
    library_queries: Vec<(String, String)>,
    /// The user's effective SQL library folder (Preferences may move it).
    library_root: PathBuf,
    selects_dirty: bool,
    pending_path_prefill: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    running: bool,
    progress: DumpProgressLine,
    cancel_flag: Option<Arc<AtomicBool>>,
    task_progress: Option<crate::background_tasks::TaskProgress>,
    last_task_rows: u64,
    last_task_bytes: u64,
    last_output: Option<PathBuf>,
    last_report: Option<DumpReport>,
    _subscriptions: Vec<Subscription>,
    _progress_task: Option<gpui::Task<()>>,
}

impl DatabaseDumpDialog {
    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn new_export(
        context: ToolDialogContext,
        database: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self::new_inner(context, DumpDialogMode::Export, None, database, window, cx)
    }

    pub fn new_schedule(
        context: ToolDialogContext,
        existing: Option<DumpSchedule>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let edit_id = existing.as_ref().map(|s| s.id);
        let database = existing.as_ref().map(|s| s.database.clone());
        let mut this = Self::new_inner(
            context,
            DumpDialogMode::Schedule,
            edit_id,
            database,
            window,
            cx,
        );
        if let Some(schedule) = existing {
            this.model.path = schedule.output_dir.display().to_string();
            this.model.options = schedule.options.clone();
            this.model.cadence = schedule.cadence;
            this.model.retention = schedule.retention;
            match &schedule.kind {
                ScheduleKind::BuiltinDump => {}
                ScheduleKind::NativeBackup {
                    pg_format,
                    pg_no_owner,
                    mysql_single_transaction,
                    mysql_extra_args,
                    mysql_strip_definer,
                } => {
                    this.model.task = ScheduleTask::NativeBackup;
                    this.model.pg_format = *pg_format;
                    this.model.pg_no_owner = *pg_no_owner;
                    this.model.mysql_single_transaction = *mysql_single_transaction;
                    this.model.set_mysql_extra_args(mysql_extra_args);
                    this.model.mysql_strip_definer = *mysql_strip_definer;
                }
                ScheduleKind::QueryExport {
                    query_path,
                    query_name,
                    format,
                } => {
                    this.model.task = ScheduleTask::QueryExport;
                    this.model.query_path = query_path.display().to_string();
                    this.model.query_name = query_name.clone();
                    this.model.export_format = *format;
                }
            }
            this.pending_path_prefill = None;
            this.path_input.update(cx, |state, cx| {
                state.set_value(this.model.path.clone(), window, cx);
            });
            this.batch_input.update(cx, |state, cx| {
                state.set_value(this.model.options.batch_size.to_string(), window, cx);
            });
            this.retention_input.update(cx, |state, cx| {
                state.set_value(this.model.retention.to_string(), window, cx);
            });
            this.mysql_options_input.update(cx, |state, cx| {
                state.set_value(this.model.mysql_extra_options.clone(), window, cx);
            });
            this.selects_dirty = true;
        }
        this
    }

    fn new_inner(
        context: ToolDialogContext,
        mode: DumpDialogMode,
        edit_schedule_id: Option<uuid::Uuid>,
        database: Option<String>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/tmp/sqlly-dumps/database.sql"));
        let batch_input = cx.new(|cx| InputState::new(window, cx).placeholder("500"));
        let mysql_options_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.database_dump.mysql_extra_options_placeholder",
            ))
        });
        let retention_input = cx.new(|cx| InputState::new(window, cx).placeholder("5"));
        let content_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, content_labels),
                Some(gpui_component::IndexPath::new(2)),
                window,
                cx,
            )
        });
        let engine_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.database_dump.engine_builtin"),
                        crate::localization::text("explorer.database_dump.engine_native"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let format_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        crate::localization::text("explorer.database_dump.format_custom"),
                        crate::localization::text("explorer.database_dump.format_plain"),
                        crate::localization::text("explorer.database_dump.format_tar"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let cadence_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, cadence_labels),
                Some(gpui_component::IndexPath::new(1)),
                window,
                cx,
            )
        });
        let native_capable = matches!(
            context.profile.database_type.connection_kind(),
            ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
        );
        let postgres =
            context.profile.database_type.connection_kind() == ConnectionKind::PostgresWire;
        let task_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, move || {
                    let mut items = vec![crate::localization::text(
                        "explorer.database_dump.task_builtin",
                    )];
                    if native_capable {
                        items.push(crate::localization::text(if postgres {
                            "explorer.database_dump.task_native_postgres"
                        } else {
                            "explorer.database_dump.task_native_mysql"
                        }));
                    }
                    items.push(crate::localization::text(
                        "explorer.database_dump.task_query_export",
                    ));
                    items
                }),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let library_root = crate::preferences::SqlLibraryPreferences::load().effective_root();
        let library_queries: Vec<(String, String)> = if mode == DumpDialogMode::Schedule {
            crate::sql_library_model::scan_library(&library_root)
                .map(|tree| {
                    tree.entries
                        .into_iter()
                        .map(|entry| {
                            let relative = entry.relative.to_string_lossy().replace('\\', "/");
                            (format!("{} — {relative}", entry.name), relative)
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let query_select = cx.new(|cx| {
            SelectState::new(
                library_queries
                    .iter()
                    .map(|(label, _)| label.clone())
                    .collect::<Vec<_>>(),
                None,
                window,
                cx,
            )
        });
        let export_format_select = cx.new(|cx| {
            SelectState::new(
                vec!["CSV".to_string(), "JSON Lines".to_string()],
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });

        let subscriptions = vec![
            cx.subscribe_in(
                &db_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(db) = value {
                        let db = db.to_string();
                        if this.model.database != db {
                            this.pending_path_prefill = Some(this.suggested_path_for(&db));
                            this.model.database = db;
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &path_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.model.path = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &batch_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        if let Ok(n) = text.trim().parse::<usize>() {
                            this.model.options.batch_size = n;
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &mysql_options_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.model.mysql_extra_options = state.read(cx).value().to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &retention_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        let text = state.read(cx).value().to_string();
                        if let Ok(n) = text.trim().parse::<usize>() {
                            this.model.retention = n;
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &content_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.options.content = content_from_index(confirmed_row(select, cx));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &engine_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.engine = engine_from_index(confirmed_row(select, cx));
                        if this.model.engine == DumpEngine::Native {
                            // Vendor tools write one file, and the
                            // per-object checkbox is hidden for them.
                            this.model.options.layout = DumpLayout::SingleFile;
                        }
                        if !this.model.database.is_empty() {
                            this.pending_path_prefill =
                                Some(this.suggested_path_for(&this.model.database));
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &format_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.pg_format = pg_format_from_index(confirmed_row(select, cx));
                        if !this.model.database.is_empty() {
                            this.pending_path_prefill =
                                Some(this.suggested_path_for(&this.model.database));
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &cadence_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.cadence = cadence_from_index(confirmed_row(select, cx));
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &task_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        let native_capable = matches!(
                            this.context.profile.database_type.connection_kind(),
                            ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
                        );
                        this.model.task =
                            task_from_index(confirmed_row(select, cx), native_capable);
                        if this.model.task == ScheduleTask::NativeBackup {
                            this.model.options.layout = DumpLayout::SingleFile;
                        }
                        if !this.model.database.is_empty() {
                            this.pending_path_prefill =
                                Some(this.suggested_path_for(&this.model.database));
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &query_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(label) = value {
                        if let Some((_, relative)) = this
                            .library_queries
                            .iter()
                            .find(|(candidate, _)| candidate == label)
                            .cloned()
                        {
                            this.model.query_path = relative;
                            this.model.query_name =
                                label.split(" — ").next().unwrap_or(label).to_string();
                        }
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &export_format_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if value.is_some() {
                        this.model.export_format = if confirmed_row(select, cx) == Some(1) {
                            QueryExportFormat::JsonLines
                        } else {
                            QueryExportFormat::Csv
                        };
                        cx.notify();
                    }
                },
            ),
        ];

        let mut model = DatabaseDumpModel::default();
        if let Some(db) = database
            .or_else(|| context.profile.database.clone())
            .filter(|d| !d.is_empty())
        {
            model.database = db.clone();
            model.path = if mode == DumpDialogMode::Schedule {
                DatabaseDumpModel::schedule_default_dir()
            } else {
                DatabaseDumpModel::suggested_path(
                    &db,
                    model.options.layout,
                    model.engine,
                    model.pg_format,
                    context.profile.database_type.connection_kind(),
                )
            };
        }

        let mut this = Self {
            context,
            mode,
            schedule_id: edit_schedule_id.unwrap_or_else(uuid::Uuid::new_v4),
            model,
            databases: Vec::new(),
            databases_loading: true,
            load_error: None,
            db_select,
            path_input,
            batch_input,
            retention_input,
            mysql_options_input: mysql_options_input.clone(),
            content_select,
            engine_select,
            format_select,
            cadence_select,
            task_select,
            query_select,
            export_format_select,
            library_queries,
            library_root,
            selects_dirty: false,
            pending_path_prefill: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(580.0)),
            running: false,
            progress: DumpProgressLine::default(),
            cancel_flag: None,
            task_progress: None,
            last_task_rows: 0,
            last_task_bytes: 0,
            last_output: None,
            last_report: None,
            _subscriptions: subscriptions,
            _progress_task: None,
        };
        if !this.model.path.is_empty() {
            this.pending_path_prefill = Some(this.model.path.clone());
        }
        this.load_databases(cx);
        this
    }

    fn suggested_path_for(&self, database: &str) -> String {
        if self.mode == DumpDialogMode::Schedule {
            // Schedules keep whatever folder is set; only an empty field
            // gets the default.
            if !self.model.path.trim().is_empty() {
                return self.model.path.clone();
            }
            return DatabaseDumpModel::schedule_default_dir();
        }
        DatabaseDumpModel::suggested_path(
            database,
            self.model.options.layout,
            self.model.engine,
            self.model.pg_format,
            self.context.profile.database_type.connection_kind(),
        )
    }

    /// Whether this form uses pg_dump / mysqldump.
    fn uses_native_tool(&self) -> bool {
        match self.mode {
            DumpDialogMode::Export => self.model.engine == DumpEngine::Native,
            DumpDialogMode::Schedule => self.model.task == ScheduleTask::NativeBackup,
        }
    }

    /// Whether this form drives mysqldump, the only tool the extra options
    /// and DEFINER stripping apply to.
    fn uses_native_mysqldump(&self) -> bool {
        self.uses_native_tool()
            && self.context.profile.database_type.connection_kind() == ConnectionKind::MySqlWire
    }

    /// Extra mysqldump options for a request: empty unless this form drives
    /// mysqldump (validation has already refused bad ones).
    fn request_mysql_extra_args(&self) -> Vec<String> {
        if self.uses_native_mysqldump() {
            self.model.mysql_extra_args().unwrap_or_default()
        } else {
            Vec::new()
        }
    }

    /// Form validation plus the route check: vendor tools dial the database
    /// directly, which a relay-routed connection cannot offer.
    fn validation(&self) -> Result<(), String> {
        self.model
            .validate(self.native_dump_tool().is_some(), self.mode)?;
        if self.uses_native_mysqldump() {
            self.model.mysql_extra_args()?;
        }
        if self.uses_native_tool()
            && let Some(reason) = crate::app::tools::direct_route_refusal(
                &self.context.profile,
                &crate::localization::text("explorer.database_dump.native_backup_feature"),
            )
        {
            return Err(reason);
        }
        Ok(())
    }

    fn native_dump_tool(&self) -> Option<sqlly_services::ToolLocation> {
        let tool = match self.context.profile.database_type.connection_kind() {
            ConnectionKind::PostgresWire => NativeTool::PgDump,
            ConnectionKind::MySqlWire => NativeTool::MysqlDump,
            _ => return None,
        };
        find_tool(&native_tool_locations(), tool).cloned()
    }

    fn load_databases(&mut self, cx: &mut gpui::Context<Self>) {
        if matches!(
            self.context.profile.database_type.connection_kind(),
            ConnectionKind::SqliteFile | ConnectionKind::HranaHttp | ConnectionKind::DuckDbFile
        ) {
            self.databases_loading = false;
            if let Some(db) = self
                .context
                .profile
                .database
                .clone()
                .filter(|d| !d.is_empty())
            {
                self.databases = vec![db.clone()];
                if self.model.database.is_empty() {
                    self.pending_path_prefill = Some(self.suggested_path_for(&db));
                    self.model.database = db;
                }
                self.selects_dirty = true;
            }
            cx.notify();
            return;
        }
        self.databases_loading = true;
        let sql = match self.context.profile.database_type.connection_kind() {
            ConnectionKind::Tds => {
                "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name".to_string()
            }
            ConnectionKind::PostgresWire => {
                "SELECT datname FROM pg_database WHERE datallowconn ORDER BY datname".to_string()
            }
            ConnectionKind::MySqlWire => "SHOW DATABASES".to_string(),
            _ => "SELECT name FROM sqlite_master WHERE type='database' ORDER BY name".to_string(),
        };
        crate::tool_dialog::run_query(&self.context, sql, cx, |this, outcome, cx| {
            this.databases_loading = false;
            if let Some(err) = outcome.error {
                this.load_error = Some(err);
            } else {
                this.databases = outcome
                    .sets
                    .first()
                    .map(|set| {
                        set.rows
                            .iter()
                            .filter_map(|r| match r.first() {
                                Some(sqlly_core::CellValue::String(s)) => Some(s.clone()),
                                _ => None,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let initial = if !this.model.database.is_empty()
                    && this.databases.contains(&this.model.database)
                {
                    Some(this.model.database.clone())
                } else {
                    this.databases.first().cloned()
                };
                if let Some(db) = initial {
                    if this.model.database.is_empty() {
                        this.pending_path_prefill = Some(this.suggested_path_for(&db));
                        this.model.database = db;
                    }
                    this.selects_dirty = true;
                }
            }
            cx.notify();
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::Relaxed);
        }
        if let Some(progress) = &self.task_progress
            && progress.is_cancelled()
            && let Some(flag) = &self.cancel_flag
        {
            flag.store(true, Ordering::Relaxed);
        }
        cx.emit(ToolDialogEvent::Close);
    }

    pub fn attach_task(&mut self, progress: crate::background_tasks::TaskProgress) {
        self.task_progress = Some(progress);
        self.last_task_rows = 0;
        self.last_task_bytes = 0;
    }

    fn choose_path(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        let per_object = self.model.options.layout == DumpLayout::FilePerObject;
        // Schedules always write into a folder (each run names its file).
        let directory_mode = per_object || self.mode == DumpDialogMode::Schedule;
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        if directory_mode {
            let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
                files: false,
                directories: true,
                multiple: false,
                prompt: None,
            });
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(paths))) = rx.await
                    && let Some(path) = paths.into_iter().next()
                {
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        this.pending_path_prefill = Some(path.display().to_string());
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        } else {
            let default = Path::new(".");
            let hint = if self.model.database.is_empty() {
                "database.sql".to_string()
            } else {
                format!("{}.sql", self.model.database)
            };
            let rx = cx.prompt_for_new_path(default, Some(&hint));
            cx.spawn(async move |_this, _cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    let mut cx = async_cx.clone();
                    weak.update(&mut cx, |this, cx| {
                        this.pending_path_prefill = Some(path.display().to_string());
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
    }

    fn save_schedule(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.validation() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        let kind = match self.model.task {
            ScheduleTask::BuiltinDump => ScheduleKind::BuiltinDump,
            ScheduleTask::NativeBackup => ScheduleKind::NativeBackup {
                pg_format: self.model.pg_format,
                pg_no_owner: self.model.pg_no_owner,
                mysql_single_transaction: self.model.mysql_single_transaction,
                mysql_extra_args: self.request_mysql_extra_args(),
                mysql_strip_definer: self.model.mysql_strip_definer && self.uses_native_mysqldump(),
            },
            ScheduleTask::QueryExport => ScheduleKind::QueryExport {
                query_path: PathBuf::from(self.model.query_path.trim()),
                query_name: self.model.query_name.clone(),
                format: self.model.export_format,
            },
        };
        let schedule = DumpSchedule {
            id: self.schedule_id,
            profile_id: self.context.profile.id.0,
            database: self.model.database.clone(),
            output_dir: PathBuf::from(self.model.path.trim()),
            cadence: self.model.cadence,
            retention: self.model.retention,
            options: self.model.to_options(),
            kind,
            enabled: true,
            last_run: None,
            last_result: None,
        };
        cx.emit(ToolDialogEvent::SaveDumpSchedule {
            schedule: Box::new(schedule),
        });
        cx.emit(ToolDialogEvent::Close);
    }

    /// The equivalent one-shot CLI line for the current form — the "runs on
    /// cron" counterpart of what this dialog does inside the app.
    fn cli_command_text(&self) -> Option<String> {
        let model = &self.model;
        if model.database.trim().is_empty() || model.path.trim().is_empty() {
            return None;
        }
        let connection = &self.context.profile.name;
        let cli = crate::cli_command::cli_path();
        match self.mode {
            DumpDialogMode::Export => match model.engine {
                DumpEngine::BuiltIn => Some(crate::cli_command::dump_command(
                    &cli,
                    connection,
                    &model.database,
                    model.path.trim(),
                    &model.options,
                )),
                DumpEngine::Native => Some(crate::cli_command::native_dump_command(
                    &cli,
                    connection,
                    &model.database,
                    model.path.trim(),
                    &model.options,
                    self.context.profile.database_type.connection_kind(),
                    crate::cli_command::NativeSettings {
                        pg_format: model.pg_format,
                        pg_no_owner: model.pg_no_owner,
                        mysql_single_transaction: model.mysql_single_transaction,
                        mysql_extra_args: self.request_mysql_extra_args(),
                        mysql_strip_definer: model.mysql_strip_definer,
                    },
                )),
            },
            DumpDialogMode::Schedule => {
                let schedule = DumpSchedule {
                    // The line names files by schedule id; a new schedule's
                    // id is fixed at dialog open so the copy matches the save.
                    id: self.schedule_id,
                    profile_id: self.context.profile.id.0,
                    database: model.database.clone(),
                    output_dir: PathBuf::from(model.path.trim()),
                    cadence: model.cadence,
                    retention: model.retention,
                    options: model.to_options(),
                    kind: match model.task {
                        ScheduleTask::BuiltinDump => ScheduleKind::BuiltinDump,
                        ScheduleTask::NativeBackup => ScheduleKind::NativeBackup {
                            pg_format: model.pg_format,
                            pg_no_owner: model.pg_no_owner,
                            mysql_single_transaction: model.mysql_single_transaction,
                            mysql_extra_args: self.request_mysql_extra_args(),
                            mysql_strip_definer: model.mysql_strip_definer
                                && self.uses_native_mysqldump(),
                        },
                        ScheduleTask::QueryExport => {
                            if model.query_path.trim().is_empty() {
                                return None;
                            }
                            ScheduleKind::QueryExport {
                                query_path: PathBuf::from(model.query_path.trim()),
                                query_name: model.query_name.clone(),
                                format: model.export_format,
                            }
                        }
                    },
                    enabled: true,
                    last_run: None,
                    last_result: None,
                };
                Some(crate::cli_command::schedule_command(
                    &cli,
                    &schedule,
                    connection,
                    &self.library_root,
                    self.context.profile.database_type.connection_kind(),
                ))
            }
        }
    }

    fn copy_cli_command(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(command) = self.cli_command_text() else {
            return;
        };
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(command));
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message: crate::cli_command::copied_status(),
        });
    }

    fn start_export(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.validation() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        self.running = true;
        self.progress = DumpProgressLine::default();
        self.last_output = None;
        self.last_report = None;

        let profile = self.context.profile.clone();
        let services = self.context.services.clone();
        let database = self.model.database.clone();
        let output = PathBuf::from(self.model.path.trim());
        let output_fallback = output.clone();
        cx.emit(ToolDialogEvent::DumpStarted {
            output_path: output.clone(),
        });
        let opts = self.model.to_options();
        let engine = self.model.engine;
        let native_req = if engine == DumpEngine::Native {
            Some(NativeDumpRequest {
                database: database.clone(),
                output: output.clone(),
                options: opts.clone(),
                tables: None,
                pg_format: self.model.pg_format,
                pg_no_owner: self.model.pg_no_owner,
                mysql_single_transaction: self.model.mysql_single_transaction,
                mysql_extra_args: self.request_mysql_extra_args(),
                mysql_strip_definer: self.model.mysql_strip_definer && self.uses_native_mysqldump(),
            })
        } else {
            None
        };
        let native_tool = self.native_dump_tool();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<DumpProgress>(8);
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let weak_dump = weak.clone();
        let async_cx_dump = async_cx.clone();

        let progress_task = cx.spawn(async move |_this, _cx| {
            while let Some(progress) = rx.recv().await {
                let mut cx = async_cx.clone();
                weak.update(&mut cx, |this, cx| {
                    if let Some(task_progress) = &this.task_progress {
                        if task_progress.is_cancelled()
                            && let Some(flag) = &this.cancel_flag
                        {
                            flag.store(true, Ordering::Relaxed);
                        }
                        let delta_rows = progress.rows_written.saturating_sub(this.last_task_rows);
                        let delta_bytes =
                            progress.bytes_written.saturating_sub(this.last_task_bytes);
                        if delta_rows > 0 {
                            task_progress.add_rows(delta_rows);
                        }
                        if delta_bytes > 0 {
                            task_progress.add_bytes(delta_bytes);
                        }
                        this.last_task_rows = progress.rows_written;
                        this.last_task_bytes = progress.bytes_written;
                    }
                    this.progress = DumpProgressLine {
                        phase: Some(progress.phase),
                        object: progress.object.clone(),
                        objects_done: progress.objects_done,
                        objects_total: progress.objects_total,
                        rows_written: progress.rows_written,
                    };
                    cx.notify();
                })
                .ok();
            }
        });

        cx.spawn(async move |_this, _cx| {
            let mut progress_fn = move |p: DumpProgress| {
                let _ = tx.try_send(p);
            };
            let result = if let (Some(req), Some(tool)) = (native_req, native_tool) {
                run_native_dump(&services, &profile, &tool, &req, cancel, &mut progress_fn).await
            } else {
                run_database_dump(
                    &services,
                    &profile,
                    &database,
                    &output,
                    &opts,
                    cancel,
                    &mut progress_fn,
                )
                .await
            };

            let mut cx = async_cx_dump.clone();
            weak_dump
                .update(&mut cx, |this, cx| {
                    this.running = false;
                    this.cancel_flag = None;
                    this._progress_task = None;
                    match result {
                        Ok(report) => {
                            this.last_output = report
                                .files
                                .first()
                                .cloned()
                                .or(Some(output_fallback.clone()));
                            this.last_report = Some(report.clone());
                            let path = this
                                .last_output
                                .as_ref()
                                .map(|p| p.display().to_string())
                                .unwrap_or_default();
                            let level = if report.cancelled {
                                StatusLevel::Info
                            } else {
                                StatusLevel::Success
                            };
                            let message = if report.cancelled {
                                crate::localization::format(
                                    "explorer.database_dump.cancelled",
                                    &[("path", &path)],
                                )
                            } else {
                                crate::localization::format(
                                    "explorer.database_dump.saved",
                                    &[("count", &report.rows_written.to_string()), ("path", &path)],
                                )
                            };
                            cx.emit(ToolDialogEvent::DumpCompleted {
                                message,
                                level,
                                primary_path: this.last_output.clone(),
                                cancelled: report.cancelled,
                            });
                        }
                        Err(err) => {
                            cx.emit(ToolDialogEvent::Status {
                                level: StatusLevel::Error,
                                message: crate::localization::format(
                                    "explorer.database_dump.failed",
                                    &[("error", &crate::localization::message(err.message()))],
                                ),
                            });
                        }
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();

        self._progress_task = Some(progress_task);
        cx.notify();
    }

    fn open_result_in_tab(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(path) = self.last_output.clone() else {
            return;
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > OPEN_IN_TAB_MAX_BYTES => {
                cx.emit(ToolDialogEvent::Status {
                    level: StatusLevel::Error,
                    message: crate::localization::text("explorer.database_dump.too_large_for_tab"),
                });
            }
            Ok(_) => match std::fs::read_to_string(&path) {
                Ok(sql) => {
                    cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                    cx.emit(ToolDialogEvent::Close);
                }
                Err(err) => {
                    cx.emit(ToolDialogEvent::Status {
                        level: StatusLevel::Error,
                        message: crate::localization::format(
                            "explorer.database_dump.read_failed",
                            &[("error", &err.to_string())],
                        ),
                    });
                }
            },
            Err(err) => {
                cx.emit(ToolDialogEvent::Status {
                    level: StatusLevel::Error,
                    message: crate::localization::format(
                        "explorer.database_dump.read_failed",
                        &[("error", &err.to_string())],
                    ),
                });
            }
        }
    }

    fn checkbox(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        set: fn(&mut DatabaseDumpModel, bool),
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(id)
            .checked(checked)
            .label(crate::localization::text(label))
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        set(&mut this.model, checked);
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }
}

impl DatabaseDumpModel {
    fn set_drop(m: &mut Self, v: bool) {
        m.options.drop_if_exists = v;
    }
    fn set_views(m: &mut Self, v: bool) {
        m.options.include_views = v;
    }
    fn set_routines(m: &mut Self, v: bool) {
        m.options.include_routines = v;
    }
    fn set_triggers(m: &mut Self, v: bool) {
        m.options.include_triggers = v;
    }
    fn set_sequences(m: &mut Self, v: bool) {
        m.options.include_sequences = v;
    }
    fn set_privileges(m: &mut Self, v: bool) {
        m.options.include_privileges = v;
    }
    fn set_pg_no_owner(m: &mut Self, v: bool) {
        m.pg_no_owner = v;
    }
    fn set_mysql_strip_definer(m: &mut Self, v: bool) {
        m.mysql_strip_definer = v;
    }
    fn set_mysql_single_transaction(m: &mut Self, v: bool) {
        m.mysql_single_transaction = v;
    }
}

/// Row of the item the select just confirmed. Choice selects dispatch on this
/// position (parallel to the `*_CHOICES` arrays below), never on the
/// translated label text.
fn confirmed_row(select: &Entity<SelectState<Vec<String>>>, cx: &gpui::App) -> Option<usize> {
    select.read(cx).selected_index(cx).map(|ix| ix.row)
}

/// Order of the content select's labels.
const CONTENT_CHOICES: [DumpContent; 3] = [
    DumpContent::StructureOnly,
    DumpContent::DataOnly,
    DumpContent::StructureAndData,
];

fn content_labels() -> Vec<String> {
    CONTENT_CHOICES
        .iter()
        .map(|content| {
            crate::localization::text(match content {
                DumpContent::StructureOnly => "explorer.database_dump.content_structure",
                DumpContent::DataOnly => "explorer.database_dump.content_data",
                DumpContent::StructureAndData => "explorer.database_dump.content_structure_data",
            })
        })
        .collect()
}

fn content_from_index(row: Option<usize>) -> DumpContent {
    row.and_then(|row| CONTENT_CHOICES.get(row).copied())
        .unwrap_or(DumpContent::StructureAndData)
}

/// Order of the engine select's labels.
const ENGINE_CHOICES: [DumpEngine; 2] = [DumpEngine::BuiltIn, DumpEngine::Native];

fn engine_from_index(row: Option<usize>) -> DumpEngine {
    row.and_then(|row| ENGINE_CHOICES.get(row).copied())
        .unwrap_or(DumpEngine::BuiltIn)
}

/// Order of the pg_dump format select's labels.
const PG_FORMAT_CHOICES: [PgFormat; 3] = [PgFormat::Custom, PgFormat::Plain, PgFormat::Tar];

/// Row of `format` in the pg_dump format select.
fn pg_format_row(format: PgFormat) -> usize {
    PG_FORMAT_CHOICES
        .iter()
        .position(|choice| *choice == format)
        .unwrap_or(0)
}

fn pg_format_from_index(row: Option<usize>) -> PgFormat {
    row.and_then(|row| PG_FORMAT_CHOICES.get(row).copied())
        .unwrap_or(PgFormat::Custom)
}

/// Order of the cadence select's labels.
const CADENCE_CHOICES: [Cadence; 3] = [Cadence::Hourly, Cadence::Daily, Cadence::Weekly];

fn cadence_labels() -> Vec<String> {
    CADENCE_CHOICES
        .iter()
        .map(|cadence| {
            crate::localization::text(match cadence {
                Cadence::Hourly => "explorer.database_dump.cadence_hourly",
                Cadence::Daily => "explorer.database_dump.cadence_daily",
                Cadence::Weekly => "explorer.database_dump.cadence_weekly",
            })
        })
        .collect()
}

fn cadence_from_index(row: Option<usize>) -> Cadence {
    row.and_then(|row| CADENCE_CHOICES.get(row).copied())
        .unwrap_or(Cadence::Daily)
}

/// Order of the schedule task select's labels; the native backup entry only
/// exists for engines with a vendor dump tool.
fn task_choices(native_capable: bool) -> Vec<ScheduleTask> {
    let mut tasks = vec![ScheduleTask::BuiltinDump];
    if native_capable {
        tasks.push(ScheduleTask::NativeBackup);
    }
    tasks.push(ScheduleTask::QueryExport);
    tasks
}

fn task_from_index(row: Option<usize>, native_capable: bool) -> ScheduleTask {
    row.and_then(|row| task_choices(native_capable).get(row).copied())
        .unwrap_or(ScheduleTask::BuiltinDump)
}

fn phase_label(phase: DumpPhase) -> String {
    crate::localization::text(match phase {
        DumpPhase::Enumerating => "explorer.database_dump.phase_enumerating",
        DumpPhase::Structure => "explorer.database_dump.phase_structure",
        DumpPhase::Data => "explorer.database_dump.phase_data",
        DumpPhase::Finishing => "explorer.database_dump.phase_finishing",
    })
}

impl gpui::Focusable for DatabaseDumpDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DatabaseDumpDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            let items = self.databases.clone();
            let idx = self
                .databases
                .iter()
                .position(|d| *d == self.model.database)
                .map(gpui_component::IndexPath::new);
            self.db_select.update(cx, |state, cx| {
                state.set_items(items, window, cx);
                state.set_selected_index(idx, window, cx);
            });
            let content_items = content_labels();
            let content_idx = CONTENT_CHOICES
                .iter()
                .position(|content| *content == self.model.options.content)
                .map(gpui_component::IndexPath::new);
            self.content_select.update(cx, |state, cx| {
                state.set_items(content_items, window, cx);
                state.set_selected_index(content_idx, window, cx);
            });
            // Set the pg_dump format by value so an edited schedule shows
            // its saved format rather than the constructor's default row.
            let pg_format_idx = pg_format_row(self.model.pg_format);
            self.format_select.update(cx, |state, cx| {
                state.set_selected_index(
                    Some(gpui_component::IndexPath::new(pg_format_idx)),
                    window,
                    cx,
                );
            });
            if self.mode == DumpDialogMode::Schedule {
                let cadence_items = cadence_labels();
                let cadence_idx = CADENCE_CHOICES
                    .iter()
                    .position(|cadence| *cadence == self.model.cadence)
                    .map(gpui_component::IndexPath::new);
                self.cadence_select.update(cx, |state, cx| {
                    state.set_items(cadence_items, window, cx);
                    state.set_selected_index(cadence_idx, window, cx);
                });
                let native_capable = matches!(
                    self.context.profile.database_type.connection_kind(),
                    ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
                );
                let task_idx = Some(gpui_component::IndexPath::new(match self.model.task {
                    ScheduleTask::BuiltinDump => 0,
                    ScheduleTask::NativeBackup => 1,
                    ScheduleTask::QueryExport => {
                        if native_capable {
                            2
                        } else {
                            1
                        }
                    }
                }));
                self.task_select.update(cx, |state, cx| {
                    state.set_selected_index(task_idx, window, cx);
                });
                let query_idx = self
                    .library_queries
                    .iter()
                    .position(|(_, relative)| *relative == self.model.query_path)
                    .map(gpui_component::IndexPath::new);
                self.query_select.update(cx, |state, cx| {
                    state.set_selected_index(query_idx, window, cx);
                });
                let format_idx = match self.model.export_format {
                    QueryExportFormat::Csv => 0,
                    QueryExportFormat::JsonLines => 1,
                };
                self.export_format_select.update(cx, |state, cx| {
                    state.set_selected_index(
                        Some(gpui_component::IndexPath::new(format_idx)),
                        window,
                        cx,
                    );
                });
            }
        }
        if let Some(path) = self.pending_path_prefill.take() {
            self.model.path = path.clone();
            self.path_input.update(cx, |state, cx| {
                state.set_value(path, window, cx);
            });
        }

        let validation = self.validation().err();
        let show_sequences =
            self.context.profile.database_type.connection_kind() == ConnectionKind::PostgresWire;
        let per_object = self.model.options.layout == DumpLayout::FilePerObject;

        let field = |label: &'static str, control: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(110.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(label)),
                )
                .child(div().flex_1().min_w(px(0.0)).child(control))
        };

        let conn_kind = self.context.profile.database_type.connection_kind();
        let native_capable = matches!(
            conn_kind,
            ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
        );
        let native_tool = self.native_dump_tool();
        let query_export =
            self.mode == DumpDialogMode::Schedule && self.model.task == ScheduleTask::QueryExport;
        let native_ui = match self.mode {
            DumpDialogMode::Export => self.model.engine == DumpEngine::Native,
            DumpDialogMode::Schedule => self.model.task == ScheduleTask::NativeBackup,
        };

        let mut body = div().flex().flex_col().gap(px(s(10.0))).child(field(
            "explorer.database_dump.database_label",
            if self.databases_loading {
                div()
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.database_dump.databases_loading",
                    ))
                    .into_any_element()
            } else if let Some(error) = &self.load_error {
                div()
                    .text_color(Theme::error())
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .child(crate::localization::format(
                        "explorer.database_dump.databases_error",
                        &[("error", error)],
                    ))
                    .into_any_element()
            } else {
                Select::new(&self.db_select).w_full().into_any_element()
            },
        ));

        if self.mode == DumpDialogMode::Schedule {
            body = body.child(field(
                "explorer.database_dump.task_label",
                Select::new(&self.task_select).w_full().into_any_element(),
            ));
        }

        if (self.mode == DumpDialogMode::Export || native_ui) && native_capable && !query_export {
            if self.mode == DumpDialogMode::Export {
                body = body.child(field(
                    "explorer.database_dump.engine_label",
                    Select::new(&self.engine_select).w_full().into_any_element(),
                ));
            }
            if native_tool.is_none() {
                body = body.child(
                    div()
                        .pl(px(s(118.0)))
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            if conn_kind == ConnectionKind::MySqlWire {
                                "explorer.database_dump.mysql_tool_hint"
                            } else {
                                "explorer.database_dump.postgres_tool_hint"
                            },
                        )),
                );
            }
            if native_ui && conn_kind == ConnectionKind::PostgresWire {
                body = body
                    .child(field(
                        "explorer.database_dump.format_label",
                        Select::new(&self.format_select).w_full().into_any_element(),
                    ))
                    .child(self.checkbox(
                        "dump-pg-no-owner",
                        "explorer.database_dump.no_owner_checkbox",
                        self.model.pg_no_owner,
                        DatabaseDumpModel::set_pg_no_owner,
                        cx,
                    ));
            }
            if native_ui && conn_kind == ConnectionKind::MySqlWire {
                body = body
                    .child(self.checkbox(
                        "dump-mysql-stxn",
                        "explorer.database_dump.mysql_single_transaction_checkbox",
                        self.model.mysql_single_transaction,
                        DatabaseDumpModel::set_mysql_single_transaction,
                        cx,
                    ))
                    .child(self.checkbox(
                        "dump-mysql-strip-definer",
                        "explorer.database_dump.mysql_strip_definer_checkbox",
                        self.model.mysql_strip_definer,
                        DatabaseDumpModel::set_mysql_strip_definer,
                        cx,
                    ))
                    .child(field(
                        "explorer.database_dump.mysql_extra_options_label",
                        Input::new(&self.mysql_options_input).into_any_element(),
                    ));
                if !self.model.mysql_extra_options.trim().is_empty()
                    && let Err(reason) = self.model.mysql_extra_args()
                {
                    body = body.child(
                        div()
                            .pl(px(s(118.0)))
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::error())
                            .child(reason),
                    );
                }
            }
        }

        body = body.child(field(
            if per_object || self.mode == DumpDialogMode::Schedule {
                "explorer.database_dump.output_folder_label"
            } else {
                "explorer.database_dump.output_file_label"
            },
            div()
                .flex()
                .flex_row()
                .gap(px(s(6.0)))
                .child(div().flex_1().child(Input::new(&self.path_input)))
                .child(
                    Button::new("dump-choose-path")
                        .label(crate::localization::text(
                            "explorer.database_dump.choose_button",
                        ))
                        .disabled(self.running)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.choose_path(window, cx);
                        })),
                )
                .into_any_element(),
        ));
        if self.mode == DumpDialogMode::Schedule {
            body = body.child(
                div()
                    .pl(px(s(118.0)))
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.database_dump.schedule_output_explanation",
                    )),
            );
        }

        if query_export {
            body = body
                .child(field(
                    "explorer.database_dump.saved_query_label",
                    Select::new(&self.query_select).w_full().into_any_element(),
                ))
                .child(field(
                    "explorer.database_dump.export_format_label",
                    Select::new(&self.export_format_select)
                        .w_full()
                        .into_any_element(),
                ));
            if self.library_queries.is_empty() {
                body = body.child(
                    div()
                        .pl(px(s(118.0)))
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.database_dump.library_empty",
                        )),
                );
            }
        } else {
            body = body
                .child(field(
                    "explorer.database_dump.content_label",
                    Select::new(&self.content_select)
                        .w_full()
                        .into_any_element(),
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(6.0)))
                        .child(self.checkbox(
                            "dump-drop",
                            "explorer.database_dump.drop_existing_checkbox",
                            self.model.options.drop_if_exists,
                            DatabaseDumpModel::set_drop,
                            cx,
                        ))
                        .children({
                            let mut rows = Vec::new();
                            if !native_ui {
                                let this = cx.entity().downgrade();
                                rows.push(
                                    Checkbox::new("dump-per-object")
                                        .checked(per_object)
                                        .label(crate::localization::text(
                                            "explorer.database_dump.per_object_checkbox",
                                        ))
                                        .on_click(move |checked: &bool, _window, app| {
                                            let per_object = *checked;
                                            if let Some(this) = this.upgrade() {
                                                this.update(app, |this, cx| {
                                                    this.model.options.layout = if per_object {
                                                        DumpLayout::FilePerObject
                                                    } else {
                                                        DumpLayout::SingleFile
                                                    };
                                                    this.pending_path_prefill =
                                                        Some(this.suggested_path_for(
                                                            &this.model.database,
                                                        ));
                                                    cx.notify();
                                                });
                                            }
                                        })
                                        .into_any_element(),
                                );
                            }
                            rows.push(self.checkbox(
                                "dump-views",
                                "explorer.database_dump.include_views",
                                self.model.options.include_views,
                                DatabaseDumpModel::set_views,
                                cx,
                            ));
                            rows.push(self.checkbox(
                                "dump-routines",
                                "explorer.database_dump.include_routines",
                                self.model.options.include_routines,
                                DatabaseDumpModel::set_routines,
                                cx,
                            ));
                            rows.push(self.checkbox(
                                "dump-triggers",
                                "explorer.database_dump.include_triggers",
                                self.model.options.include_triggers,
                                DatabaseDumpModel::set_triggers,
                                cx,
                            ));
                            rows
                        }),
                );
        }

        if show_sequences && !query_export {
            body = body.child(div().pl(px(s(118.0))).child(self.checkbox(
                "dump-sequences",
                "explorer.database_dump.include_sequences",
                self.model.options.include_sequences,
                DatabaseDumpModel::set_sequences,
                cx,
            )));
        }

        // PostgreSQL built-in dumps can restate each object's GRANT/REVOKE
        // statements (pg_dump writes its own in native mode).
        if show_sequences && !query_export && !native_ui {
            body = body.child(div().pl(px(s(118.0))).child(self.checkbox(
                "dump-privileges",
                "explorer.database_dump.include_privileges",
                self.model.options.include_privileges,
                DatabaseDumpModel::set_privileges,
                cx,
            )));
        }

        if !native_ui && !query_export {
            body = body.child(field(
                "explorer.database_dump.batch_size_label",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(Input::new(&self.batch_input))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "explorer.database_dump.batch_size_hint",
                                &[
                                    ("minimum", &BATCH_SIZE_MIN.to_string()),
                                    ("maximum", &BATCH_SIZE_MAX.to_string()),
                                ],
                            )),
                    )
                    .into_any_element(),
            ));
        }

        if self.mode == DumpDialogMode::Schedule {
            body = body
                .child(field(
                    "explorer.database_dump.cadence_label",
                    Select::new(&self.cadence_select)
                        .w_full()
                        .into_any_element(),
                ))
                .child(field(
                    "explorer.database_dump.keep_label",
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(4.0)))
                        .child(Input::new(&self.retention_input))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "explorer.database_dump.keep_hint",
                                )),
                        )
                        .into_any_element(),
                ))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.74)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.database_dump.schedule_runtime_hint",
                        )),
                );
        }

        if let Some(reason) = &validation {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(reason.clone()),
            );
        }

        if self.running || self.progress.phase.is_some() {
            let phase = self
                .progress
                .phase
                .as_ref()
                .map(|p| phase_label(*p))
                .unwrap_or_else(|| {
                    crate::localization::text("explorer.database_dump.phase_starting")
                });
            let (key, arguments): (&'static str, Vec<(&'static str, String)>) =
                if self.progress.objects_total > 0 {
                    (
                        "explorer.database_dump.progress_objects",
                        vec![
                            ("phase", phase),
                            ("object", self.progress.object.clone()),
                            ("done", self.progress.objects_done.to_string()),
                            ("total", self.progress.objects_total.to_string()),
                            ("rows", self.progress.rows_written.to_string()),
                        ],
                    )
                } else if !self.progress.object.is_empty() {
                    (
                        "explorer.database_dump.progress_object",
                        vec![
                            ("phase", phase),
                            ("object", self.progress.object.clone()),
                            ("rows", self.progress.rows_written.to_string()),
                        ],
                    )
                } else {
                    (
                        "explorer.database_dump.progress_phase",
                        vec![
                            ("phase", phase),
                            ("rows", self.progress.rows_written.to_string()),
                        ],
                    )
                };
            let arguments = arguments
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect::<Vec<_>>();
            body = body.child(
                div()
                    .pt(px(s(6.0)))
                    .border_t_1()
                    .border_color(Theme::border())
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(key, &arguments)),
            );
        }

        let primary_label = crate::localization::text(if self.mode == DumpDialogMode::Schedule {
            "explorer.database_dump.save_schedule_button"
        } else {
            "explorer.database_dump.export_button"
        });

        // Footer is state-dependent (Cancel replaces Close while running), so
        // the chrome's default Close stays off and the buttons come as a Vec.
        let mut buttons: Vec<gpui::AnyElement> = Vec::new();

        if self.last_output.is_some() && !self.running {
            let open_ok = self.model.engine == DumpEngine::BuiltIn
                || (self.model.engine == DumpEngine::Native
                    && self.model.pg_format == PgFormat::Plain
                    && conn_kind == ConnectionKind::PostgresWire);
            if open_ok {
                buttons.push(
                    Button::new("dump-open-tab")
                        .label(crate::localization::text(
                            "explorer.database_dump.open_tab_button",
                        ))
                        .on_click(cx.listener(|this, _, _window, cx| this.open_result_in_tab(cx)))
                        .into_any_element(),
                );
            }
        }

        if self.running {
            buttons.push(
                Button::new("dump-cancel-run")
                    .label(crate::localization::text(
                        "explorer.database_dump.cancel_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if let Some(flag) = &this.cancel_flag {
                            flag.store(true, Ordering::Relaxed);
                        }
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        } else {
            let cli_command = self.cli_command_text();
            buttons.push(
                Button::new("dump-copy-cli")
                    .label(crate::localization::text(
                        "explorer.database_dump.copy_cli_button",
                    ))
                    .disabled(cli_command.is_none())
                    .on_click(cx.listener(|this, _, _window, cx| this.copy_cli_command(cx)))
                    .into_any_element(),
            );
            buttons.push(
                Button::new("dump-close")
                    .label(crate::localization::text(
                        "explorer.database_dump.close_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.close(cx)))
                    .into_any_element(),
            );
            buttons.push(
                Button::new("dump-primary")
                    .primary()
                    .label(primary_label)
                    .disabled(validation.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.mode == DumpDialogMode::Schedule {
                            this.save_schedule(cx);
                        } else {
                            this.start_export(cx);
                        }
                    }))
                    .into_any_element(),
            );
        }

        let hint = div()
            .flex_1()
            .min_w(px(0.0))
            .text_size(px(Theme::font_size_ratio(0.74)))
            .text_color(Theme::text_dim())
            .child(crate::localization::text(
                if self.mode == DumpDialogMode::Schedule {
                    "explorer.database_dump.schedule_hint"
                } else {
                    "explorer.database_dump.export_hint"
                },
            ));

        let title = if self.mode == DumpDialogMode::Schedule {
            crate::localization::format(
                "explorer.database_dump.schedule_title",
                &[("connection", &self.context.profile.name)],
            )
        } else {
            crate::localization::format(
                "explorer.database_dump.export_title",
                &[("connection", &self.context.profile.name)],
            )
        };

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("database-dump-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .use_default_close(false)
            .dismiss_on_scrim_click(false)
            .key_context("DatabaseDumpDialog")
            .track_focus(&focus_handle)
            // Same guard as the old escape handler: no dismissal (✕ or Escape)
            // while a dump is running — Cancel is the only way out mid-run.
            .on_close(|this, _window, cx| {
                if !this.running {
                    this.close(cx);
                }
            })
            .leading_button(hint)
            .child(body);
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choice_selects_map_rows_not_label_text() {
        assert_eq!(content_from_index(Some(0)), DumpContent::StructureOnly);
        assert_eq!(content_from_index(Some(1)), DumpContent::DataOnly);
        assert_eq!(content_from_index(Some(2)), DumpContent::StructureAndData);
        assert_eq!(content_from_index(None), DumpContent::StructureAndData);
        assert_eq!(content_labels().len(), CONTENT_CHOICES.len());

        assert_eq!(engine_from_index(Some(0)), DumpEngine::BuiltIn);
        assert_eq!(engine_from_index(Some(1)), DumpEngine::Native);

        // Previously matched English literals, so a translated label always
        // fell back to Custom.
        assert_eq!(pg_format_from_index(Some(0)), PgFormat::Custom);
        assert_eq!(pg_format_from_index(Some(1)), PgFormat::Plain);
        assert_eq!(pg_format_from_index(Some(2)), PgFormat::Tar);
        // An edited schedule's saved format selects its own row.
        for format in PG_FORMAT_CHOICES {
            assert_eq!(pg_format_from_index(Some(pg_format_row(format))), format);
        }

        assert_eq!(cadence_from_index(Some(0)), Cadence::Hourly);
        assert_eq!(cadence_from_index(Some(1)), Cadence::Daily);
        assert_eq!(cadence_from_index(Some(2)), Cadence::Weekly);
        assert_eq!(cadence_labels().len(), CADENCE_CHOICES.len());

        assert_eq!(task_from_index(Some(1), true), ScheduleTask::NativeBackup);
        assert_eq!(task_from_index(Some(2), true), ScheduleTask::QueryExport);
        assert_eq!(task_from_index(Some(1), false), ScheduleTask::QueryExport);
        assert_eq!(task_from_index(Some(0), false), ScheduleTask::BuiltinDump);
        assert_eq!(task_from_index(Some(9), false), ScheduleTask::BuiltinDump);
    }

    #[test]
    fn validation_requires_path_and_valid_batch() {
        let model = DatabaseDumpModel::default();
        assert!(model.validate(true, DumpDialogMode::Export).is_err());
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out.sql".into(),
            options: DumpOptions {
                batch_size: 0,
                ..DumpOptions::default()
            },
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Export).is_err());
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out.sql".into(),
            options: DumpOptions {
                batch_size: 500,
                ..DumpOptions::default()
            },
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Export).is_ok());
    }

    #[test]
    fn per_object_requires_directory_path() {
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out.sql".into(),
            options: DumpOptions {
                layout: DumpLayout::FilePerObject,
                batch_size: 100,
                ..DumpOptions::default()
            },
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Export).is_err());
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out-dir".into(),
            options: DumpOptions {
                layout: DumpLayout::FilePerObject,
                batch_size: 100,
                ..DumpOptions::default()
            },
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Export).is_ok());
    }

    #[test]
    fn to_options_clones_dump_options() {
        let model = DatabaseDumpModel {
            database: "Sales".into(),
            path: "/tmp/s.sql".into(),
            options: DumpOptions {
                content: DumpContent::DataOnly,
                batch_size: 250,
                ..DumpOptions::default()
            },
            ..Default::default()
        };
        let opts = model.to_options();
        assert_eq!(opts.content, DumpContent::DataOnly);
        assert_eq!(opts.batch_size, 250);
    }

    #[test]
    fn native_engine_requires_single_file_layout() {
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out-dir".into(),
            options: DumpOptions {
                layout: DumpLayout::FilePerObject,
                batch_size: 100,
                ..DumpOptions::default()
            },
            engine: DumpEngine::Native,
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Export).is_err());
    }

    #[test]
    fn mysql_extra_options_split_and_reject_reserved_flags() {
        let mut model = DatabaseDumpModel {
            mysql_extra_options: "--skip-comments  --where=\"id > 5\"".into(),
            ..Default::default()
        };
        assert_eq!(
            model.mysql_extra_args().unwrap(),
            vec!["--skip-comments".to_string(), "--where=id > 5".to_string()]
        );
        model.mysql_extra_options = "--result-file=/etc/passwd".into();
        assert!(model.mysql_extra_args().is_err());
        model.mysql_extra_options = "otherdb".into();
        assert!(model.mysql_extra_args().is_err());
        model.mysql_extra_options = String::new();
        assert!(model.mysql_extra_args().unwrap().is_empty());
    }

    #[test]
    fn saved_mysql_extra_options_round_trip_through_the_text_field() {
        let saved = vec![
            "--skip-comments".to_string(),
            "--where=name = 'a b' and x = \"y\\z\"".to_string(),
        ];
        let mut model = DatabaseDumpModel::default();
        model.set_mysql_extra_args(&saved);
        assert_eq!(model.mysql_extra_args().unwrap(), saved);
    }

    #[test]
    fn native_engine_requires_located_tool() {
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/out.dump".into(),
            engine: DumpEngine::Native,
            ..Default::default()
        };
        assert!(model.validate(false, DumpDialogMode::Export).is_err());
        assert!(model.validate(true, DumpDialogMode::Export).is_ok());
    }

    #[test]
    fn suggested_path_uses_native_extension() {
        assert_eq!(
            DatabaseDumpModel::suggested_path(
                "sales",
                DumpLayout::SingleFile,
                DumpEngine::Native,
                PgFormat::Custom,
                ConnectionKind::PostgresWire,
            ),
            "/tmp/sqlly-dumps/sales.dump"
        );
        assert_eq!(
            DatabaseDumpModel::suggested_path(
                "sales",
                DumpLayout::SingleFile,
                DumpEngine::Native,
                PgFormat::Plain,
                ConnectionKind::PostgresWire,
            ),
            "/tmp/sqlly-dumps/sales.sql"
        );
    }

    #[test]
    fn schedule_query_export_needs_a_saved_query() {
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/exports".into(),
            task: ScheduleTask::QueryExport,
            ..Default::default()
        };
        assert!(
            model
                .validate(true, DumpDialogMode::Schedule)
                .unwrap_err()
                .contains("saved query")
        );
        let model = DatabaseDumpModel {
            query_path: "reports/daily.sql".into(),
            query_name: "daily".into(),
            ..model
        };
        assert!(model.validate(true, DumpDialogMode::Schedule).is_ok());
    }

    #[test]
    fn schedule_native_backup_needs_the_tool() {
        let model = DatabaseDumpModel {
            database: "db".into(),
            path: "/tmp/sqlly-schedule-test-backups".into(),
            task: ScheduleTask::NativeBackup,
            ..Default::default()
        };
        assert!(model.validate(false, DumpDialogMode::Schedule).is_err());
        assert!(model.validate(true, DumpDialogMode::Schedule).is_ok());
    }

    #[test]
    fn schedules_take_an_output_folder_for_every_task() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("Sales.sql");
        std::fs::write(&file, "x").expect("write");
        for task in [
            ScheduleTask::BuiltinDump,
            ScheduleTask::NativeBackup,
            ScheduleTask::QueryExport,
        ] {
            let model = DatabaseDumpModel {
                database: "Sales".into(),
                path: dir.path().display().to_string(),
                task,
                query_path: "reports/daily.sql".into(),
                query_name: "daily".into(),
                ..Default::default()
            };
            // An existing folder is accepted (it used to be refused as
            // "Single-file export requires a file path").
            assert!(
                model.validate(true, DumpDialogMode::Schedule).is_ok(),
                "{task:?}"
            );
            // A folder that does not exist yet is fine: the run creates it.
            let missing = DatabaseDumpModel {
                path: dir.path().join("nightly").display().to_string(),
                ..model.clone()
            };
            assert!(
                missing.validate(true, DumpDialogMode::Schedule).is_ok(),
                "{task:?}"
            );
            // An existing file, or a new name that looks like one, is refused.
            let existing_file = DatabaseDumpModel {
                path: file.display().to_string(),
                ..model.clone()
            };
            assert!(
                existing_file
                    .validate(true, DumpDialogMode::Schedule)
                    .is_err(),
                "{task:?}"
            );
            let file_like = DatabaseDumpModel {
                path: dir.path().join("Sales.dump").display().to_string(),
                ..model
            };
            assert!(
                file_like.validate(true, DumpDialogMode::Schedule).is_err(),
                "{task:?}"
            );
        }
    }

    #[test]
    fn schedule_default_dir_is_a_folder_not_a_file() {
        let dir = DatabaseDumpModel::schedule_default_dir();
        assert!(!dir.ends_with(".sql") && !dir.ends_with(".dump"), "{dir}");
        let model = DatabaseDumpModel {
            database: "Sales".into(),
            path: dir,
            ..Default::default()
        };
        assert!(model.validate(true, DumpDialogMode::Schedule).is_ok());
    }
}
