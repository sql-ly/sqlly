//! Cross-connection table data transfer wizard — pick source tables, target
//! connection/database/schema, mode and casing, then run with progress.

#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

use std::collections::{BTreeSet, HashSet};
use std::time::Instant;

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::connection::ConnectionRoute;
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};
use sqlly_services::metadata::{BulkColumnRow, ForeignKeyRow};
use sqlly_services::{
    NameCasing, TransferCancel, TransferMode, TransferOutcome, TransferPlan, TransferProgress,
    TransferTable, metadata::PrimaryKeyRow, ordered_tables, statements_for,
};

#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::{open_sink, run_transfer};

use crate::app::StatusLevel;
use crate::connection::ConnectionStore;
use crate::dialog::{Dialog, DialogFrame};
use crate::mutation_review::SafetyFeatures;
use crate::service_router::ServiceRouter;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

const MAX_TRANSFER_PROFILES: usize = 200;

#[derive(Clone, Debug, PartialEq, Eq)]
struct TableEntry {
    schema: Option<String>,
    name: String,
    key: String,
}

impl TableEntry {
    fn label(&self) -> String {
        match &self.schema {
            Some(schema) if !schema.is_empty() => format!("{schema}.{}", self.name),
            _ => self.name.clone(),
        }
    }
}

fn table_key(schema: Option<&str>, name: &str) -> String {
    format!(
        "{}|{}",
        schema.unwrap_or("").to_ascii_lowercase(),
        name.to_ascii_lowercase()
    )
}

fn profile_label(profile: &ConnectionProfile) -> String {
    let id = profile.id.0.to_string();
    format!(
        "{} · {} · {}:{} · {}",
        profile.name,
        profile.database_type.display_name(),
        profile.server,
        profile.port,
        &id[..8]
    )
}

fn database_list_sql(database_type: DatabaseType) -> Option<&'static str> {
    match database_type.connection_kind() {
        ConnectionKind::Tds => {
            Some("SELECT name FROM sys.databases WHERE state = 0 AND database_id > 4 ORDER BY name")
        }
        ConnectionKind::PostgresWire => Some(
            "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY datname",
        ),
        ConnectionKind::MySqlWire => Some(
            "SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN ('information_schema','mysql','performance_schema','sys') ORDER BY schema_name",
        ),
        ConnectionKind::SqliteFile => None,
        ConnectionKind::DuckDbFile => None,
        _ => None,
    }
}

/// Default target schema input per engine (mirrors import/export).
pub fn default_target_schema(db: DatabaseType) -> &'static str {
    match db {
        DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase => "dbo",
        DatabaseType::Postgres => "public",
        _ => "",
    }
}

fn localized_mode_label(mode: TransferMode) -> String {
    let key = match mode {
        TransferMode::CreateTable => "results.data_transfer.mode_create_table",
        TransferMode::Append => "results.data_transfer.mode_append",
        TransferMode::TruncateThenInsert => "results.data_transfer.mode_truncate_insert",
        TransferMode::UpsertOnPrimaryKey => "results.data_transfer.mode_upsert",
    };
    crate::localization::text(key)
}

fn localized_casing_label(casing: NameCasing) -> String {
    let key = match casing {
        NameCasing::AsIs => "results.data_transfer.casing_keep",
        NameCasing::Lower => "results.data_transfer.casing_lower",
        NameCasing::Upper => "results.data_transfer.casing_upper",
    };
    crate::localization::text(key)
}

/// Modes in the order of the mode select's labels.
const MODE_CHOICES: [TransferMode; 4] = [
    TransferMode::CreateTable,
    TransferMode::Append,
    TransferMode::TruncateThenInsert,
    TransferMode::UpsertOnPrimaryKey,
];

/// Casings in the order of the casing select's labels.
const CASING_CHOICES: [NameCasing; 3] = [NameCasing::AsIs, NameCasing::Lower, NameCasing::Upper];

fn mode_from_row(row: Option<usize>) -> TransferMode {
    row.and_then(|row| MODE_CHOICES.get(row).copied())
        .unwrap_or(TransferMode::CreateTable)
}

fn casing_from_row(row: Option<usize>) -> NameCasing {
    row.and_then(|row| CASING_CHOICES.get(row).copied())
        .unwrap_or(NameCasing::AsIs)
}

fn fk_parent_key(row: &ForeignKeyRow) -> String {
    format!(
        "{}.{}",
        row.to_schema.to_ascii_lowercase(),
        row.to_table.to_ascii_lowercase()
    )
}

fn fk_child_key(row: &ForeignKeyRow) -> String {
    format!(
        "{}.{}",
        row.from_schema.to_ascii_lowercase(),
        row.from_table.to_ascii_lowercase()
    )
}

/// Returns an error line when source and target routes describe the same table
/// set (same profile, database, and schema).
pub fn same_source_and_target_error(
    source_profile_id: uuid::Uuid,
    target_profile_id: uuid::Uuid,
    source_db: &str,
    target_db: &str,
    target_schema: &str,
    selected: &[(Option<String>, String)],
) -> Option<String> {
    if source_profile_id != target_profile_id || source_db.trim() != target_db.trim() {
        return None;
    }
    if selected.is_empty() {
        return None;
    }
    let target_schema = target_schema.trim();
    let overlaps = selected.iter().any(|(schema, _name)| {
        let source_schema = schema.as_deref().unwrap_or("").trim();
        source_schema.eq_ignore_ascii_case(target_schema)
            || (source_schema.is_empty() && target_schema.is_empty())
    });
    overlaps.then(|| crate::localization::text("results.data_transfer.same_source_and_target"))
}

/// Build a transfer plan from source metadata and the checked table keys
/// (`schema|name`, lower-cased segments).
#[allow(clippy::too_many_arguments)]
pub fn plan_from_selection(
    source: DatabaseType,
    target: DatabaseType,
    target_schema: Option<String>,
    mode: TransferMode,
    casing: NameCasing,
    selected_keys: &BTreeSet<String>,
    columns: &[BulkColumnRow],
    primary_keys: &[PrimaryKeyRow],
    foreign_keys: &[ForeignKeyRow],
) -> TransferPlan {
    let mut tables = Vec::new();
    for key in selected_keys {
        let (schema_part, name_part) = key.split_once('|').unwrap_or(("", key.as_str()));
        let object_schema = schema_part;
        let column_match = columns.iter().find(|row| {
            row.object.eq_ignore_ascii_case(name_part)
                && row.schema.eq_ignore_ascii_case(object_schema)
        });
        let schema = column_match.map(|row| row.schema.clone()).or_else(|| {
            if schema_part.is_empty() {
                None
            } else {
                Some(schema_part.to_string())
            }
        });
        let name = column_match
            .map(|row| row.object.clone())
            .unwrap_or_else(|| name_part.to_string());
        let schema_for_match = schema.as_deref().unwrap_or("");
        let mut table_columns: Vec<_> = columns
            .iter()
            .filter(|row| {
                row.object.eq_ignore_ascii_case(&name)
                    && row.schema.eq_ignore_ascii_case(schema_for_match)
            })
            .map(|row| {
                let pk_ordinal = primary_keys
                    .iter()
                    .find(|pk| {
                        pk.table.eq_ignore_ascii_case(&name)
                            && pk.schema.eq_ignore_ascii_case(schema_for_match)
                            && pk.column.eq_ignore_ascii_case(&row.column)
                    })
                    .map(|pk| pk.key_ordinal);
                sqlly_services::TransferColumn {
                    name: row.column.clone(),
                    source_type: row.type_name.clone().unwrap_or_default(),
                    nullable: row.nullable.unwrap_or(true),
                    is_identity: row.is_identity,
                    pk_ordinal,
                }
            })
            .collect();
        table_columns.sort_by(|a, b| {
            a.pk_ordinal
                .unwrap_or(i64::MAX)
                .cmp(&b.pk_ordinal.unwrap_or(i64::MAX))
                .then_with(|| a.name.cmp(&b.name))
        });

        let child_key = format!(
            "{}.{}",
            schema_for_match.to_ascii_lowercase(),
            name.to_ascii_lowercase()
        );
        let depends_on: Vec<String> = foreign_keys
            .iter()
            .filter(|row| fk_child_key(row) == child_key)
            .map(fk_parent_key)
            .filter(|parent| {
                selected_keys.iter().any(|sel| {
                    let (schema_part, name_part) =
                        sel.split_once('|').unwrap_or(("", sel.as_str()));
                    format!(
                        "{}.{}",
                        schema_part.to_ascii_lowercase(),
                        name_part.to_ascii_lowercase()
                    ) == *parent
                })
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        tables.push(TransferTable {
            schema,
            name,
            columns: table_columns,
            depends_on,
        });
    }

    TransferPlan {
        source,
        target,
        target_schema,
        mode,
        casing,
        tables,
    }
}

pub fn collect_plan_warnings(plan: &TransferPlan) -> Vec<String> {
    let mut warnings = Vec::new();
    let (_, order_warnings) = ordered_tables(plan);
    warnings.extend(order_warnings.into_iter().map(crate::localization::message));
    for table in &plan.tables {
        match statements_for(plan, table) {
            Ok(stmts) => {
                warnings.extend(stmts.warnings.into_iter().map(crate::localization::message))
            }
            Err(message) => warnings.push(crate::localization::message(message)),
        }
    }
    warnings.sort();
    warnings.dedup();
    warnings
}

pub fn summary_text(
    selected_count: usize,
    target_profile: &str,
    target_db: &str,
    target_schema: &str,
    mode: TransferMode,
    warnings: &[String],
    gate_error: Option<&str>,
) -> String {
    let mut lines = vec![
        crate::localization::format(
            if selected_count == 1 {
                "results.data_transfer.plan_target_summary_one"
            } else {
                "results.data_transfer.plan_target_summary"
            },
            &[
                ("table_count", selected_count.to_string().as_str()),
                ("profile_name", target_profile),
                ("database_name", target_db),
                ("schema_name", target_schema),
            ],
        ),
        crate::localization::format(
            "results.data_transfer.plan_mode",
            &[("mode", &localized_mode_label(mode))],
        ),
        crate::localization::text("results.data_transfer.plan_row_estimate_unknown"),
    ];
    for warning in warnings {
        lines.push(warning.clone());
    }
    if let Some(error) = gate_error {
        lines.push(error.to_string());
    }
    lines.join("\n")
}

fn format_outcome_summary(plan: &TransferPlan, outcome: &TransferOutcome) -> String {
    let mut lines = vec![
        crate::localization::format(
            "results.data_transfer.outcome_mode",
            &[("mode", &localized_mode_label(plan.mode))],
        ),
        crate::localization::format(
            "results.data_transfer.outcome_target_engine",
            &[("engine_name", plan.target.display_name())],
        ),
    ];
    if outcome.tables.is_empty() {
        lines.push(crate::localization::text(
            "results.data_transfer.outcome_no_tables",
        ));
    } else {
        lines.push(crate::localization::text(
            "results.data_transfer.outcome_tables_heading",
        ));
        for (table, rows) in &outcome.tables {
            lines.push(crate::localization::format(
                "results.data_transfer.outcome_table_rows",
                &[("table_name", table), ("row_count", &rows.to_string())],
            ));
        }
    }
    for warning in &outcome.warnings {
        lines.push(crate::localization::format(
            "results.data_transfer.outcome_warning",
            &[("warning", &crate::localization::message(warning))],
        ));
    }
    if let Some(error) = &outcome.error {
        lines.push(crate::localization::format(
            "results.data_transfer.outcome_error",
            &[("error", &crate::localization::message(error))],
        ));
    }
    if outcome.cancelled {
        lines.push(crate::localization::text(
            "results.data_transfer.outcome_cancelled",
        ));
    }
    lines.join("\n")
}

fn target_route_error(profile: &ConnectionProfile) -> Option<String> {
    if matches!(profile.route, ConnectionRoute::ViaRelay { .. }) {
        Some(crate::localization::text(
            "results.data_transfer.direct_target_required",
        ))
    } else {
        None
    }
}

fn mutations_allowed(safety: &SafetyFeatures) -> bool {
    !safety.only_allow_select
}

fn target_writes_error(profile: &ConnectionProfile) -> Option<String> {
    let store = ConnectionStore::global();
    let safety = SafetyFeatures::effective_for_profile(profile, store);
    if !mutations_allowed(&safety) {
        return Some(crate::localization::format(
            "results.data_transfer.writes_disabled_safety",
            &[("profile_name", &profile.name)],
        ));
    }
    if safety
        .production_guard()
        .forces_select_only(None, Instant::now())
    {
        return Some(crate::localization::format(
            "results.data_transfer.writes_disabled_production",
            &[("profile_name", &profile.name)],
        ));
    }
    None
}

impl EventEmitter<ToolDialogEvent> for DataTransferPane {}

pub struct DataTransferPane {
    router: ServiceRouter,
    profiles: Vec<ConnectionProfile>,
    source_profile: Option<ConnectionProfile>,
    target_profile: Option<ConnectionProfile>,
    source_profile_select: Entity<SelectState<Vec<String>>>,
    target_profile_select: Entity<SelectState<Vec<String>>>,
    source_databases: Vec<String>,
    target_databases: Vec<String>,
    source_databases_loading: bool,
    target_databases_loading: bool,
    source_db: Option<String>,
    target_db: Option<String>,
    source_database_select: Entity<SelectState<Vec<String>>>,
    target_database_select: Entity<SelectState<Vec<String>>>,
    target_schema_input: Entity<InputState>,
    target_schema: String,
    mode_select: Entity<SelectState<Vec<String>>>,
    casing_select: Entity<SelectState<Vec<String>>>,
    mode: TransferMode,
    casing: NameCasing,
    tables: Vec<TableEntry>,
    tables_loading: bool,
    selected: HashSet<String>,
    running: bool,
    progress_table: Option<String>,
    progress_rows: u64,
    progress_index: usize,
    progress_total: usize,
    cancel: TransferCancel,
    task_progress: Option<crate::background_tasks::TaskProgress>,
    last_task_rows: u64,
    plan_warnings: Vec<String>,
    error: Option<String>,
    last_summary: Option<String>,
    selects_dirty: bool,
    tables_generation: u64,
    source_database_generation: u64,
    target_database_generation: u64,
    frame: DialogFrame,
    /// True once the pane is popped out into its own OS window: the window
    /// boundary is the dialog boundary, and close tears the window down.
    windowed: bool,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

enum TransferSide {
    Source,
    Target,
}

impl DataTransferPane {
    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn new(
        context: ToolDialogContext,
        router: ServiceRouter,
        preselect: Option<(Option<String>, Option<String>, String)>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let mut profiles = ConnectionStore::global()
            .list()
            .into_iter()
            .filter(|profile| {
                crate::capabilities::admin_tool(
                    profile.database_type,
                    crate::capabilities::AdminTool::DataTransfer,
                )
                .is_available()
            })
            .take(MAX_TRANSFER_PROFILES)
            .collect::<Vec<_>>();
        if !profiles
            .iter()
            .any(|profile| profile.id == context.profile.id)
        {
            profiles.push(context.profile.clone());
        }
        profiles.sort_by_key(profile_label);

        let source_profile = profiles
            .iter()
            .find(|profile| profile.id == context.profile.id)
            .cloned();
        let target_profile = profiles
            .iter()
            .find(|profile| Some(profile.id) != source_profile.as_ref().map(|source| source.id))
            .cloned()
            .or_else(|| source_profile.clone());

        let (preselect_db, preselect_schema, preselect_table) =
            preselect.unwrap_or((None, None, String::new()));

        let source_profile_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let target_profile_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let source_database_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let target_database_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let mode_select = cx.new(|cx| {
            SelectState::new(
                MODE_CHOICES.map(localized_mode_label).to_vec(),
                None,
                window,
                cx,
            )
        });
        let casing_select = cx.new(|cx| {
            SelectState::new(
                CASING_CHOICES.map(localized_casing_label).to_vec(),
                None,
                window,
                cx,
            )
        });

        let default_schema = target_profile
            .as_ref()
            .map(|profile| default_target_schema(profile.database_type).to_string())
            .unwrap_or_default();
        let preselect_table_key = if preselect_table.is_empty() {
            None
        } else {
            Some(
                preselect_schema
                    .as_ref()
                    .filter(|schema| !schema.is_empty())
                    .map(|schema| table_key(Some(schema.as_str()), &preselect_table))
                    .unwrap_or_else(|| table_key(None, &preselect_table)),
            )
        };
        let target_schema = preselect_schema
            .filter(|schema| !schema.is_empty())
            .unwrap_or_else(|| default_schema.clone());
        let target_schema_input = cx.new(|cx| {
            let mut state = InputState::new(window, cx);
            state.set_value(&target_schema, window, cx);
            state
        });

        let subscriptions = vec![
            cx.subscribe_in(
                &source_profile_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.source_profile = value
                        .as_ref()
                        .and_then(|label| {
                            this.profiles
                                .iter()
                                .find(|profile| profile_label(profile) == label.as_str())
                        })
                        .cloned();
                    this.invalidate_tables();
                    this.load_databases(TransferSide::Source, cx);
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &target_profile_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.target_profile = value
                        .as_ref()
                        .and_then(|label| {
                            this.profiles
                                .iter()
                                .find(|profile| profile_label(profile) == label.as_str())
                        })
                        .cloned();
                    if let Some(profile) = &this.target_profile {
                        let schema = default_target_schema(profile.database_type);
                        this.target_schema = schema.to_string();
                        this.target_schema_input.update(cx, |state, cx| {
                            state.set_value(schema, _window, cx);
                        });
                    }
                    this.load_databases(TransferSide::Target, cx);
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &source_database_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.source_db = value.as_ref().map(|value| value.to_string());
                    this.invalidate_tables();
                    this.load_tables(cx);
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &target_database_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.target_db = value.as_ref().map(|value| value.to_string());
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &target_schema_input,
                window,
                |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.target_schema = state.read(cx).value().to_string();
                        this.refresh_plan_preview(cx);
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &mode_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.mode = if value.is_some() {
                        mode_from_row(select.read(cx).selected_index(cx).map(|ix| ix.row))
                    } else {
                        TransferMode::CreateTable
                    };
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
            cx.subscribe_in(
                &casing_select,
                window,
                |this: &mut Self, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    this.casing = if value.is_some() {
                        casing_from_row(select.read(cx).selected_index(cx).map(|ix| ix.row))
                    } else {
                        NameCasing::AsIs
                    };
                    this.refresh_plan_preview(cx);
                    cx.notify();
                },
            ),
        ];

        let mut this = Self {
            router,
            profiles,
            source_profile,
            target_profile,
            source_profile_select,
            target_profile_select,
            source_databases: Vec::new(),
            target_databases: Vec::new(),
            source_databases_loading: false,
            target_databases_loading: false,
            source_db: preselect_db,
            target_db: None,
            source_database_select,
            target_database_select,
            target_schema_input,
            target_schema,
            mode_select,
            casing_select,
            mode: TransferMode::CreateTable,
            casing: NameCasing::AsIs,
            tables: Vec::new(),
            tables_loading: false,
            selected: HashSet::new(),
            running: false,
            progress_table: None,
            progress_rows: 0,
            progress_index: 0,
            progress_total: 0,
            cancel: TransferCancel::default(),
            task_progress: None,
            last_task_rows: 0,
            plan_warnings: Vec::new(),
            error: None,
            last_summary: None,
            selects_dirty: true,
            tables_generation: 0,
            source_database_generation: 0,
            target_database_generation: 0,
            frame: DialogFrame::new(Theme::scaled(860.0), Theme::scaled(640.0)),
            windowed: false,
            focus_handle: cx.focus_handle(),
            _subscriptions: subscriptions,
        };

        if let Some(key) = preselect_table_key {
            this.selected.insert(key);
        }

        this.load_databases(TransferSide::Source, cx);
        this.load_databases(TransferSide::Target, cx);
        this.load_tables(cx);
        this
    }

    pub fn take_last_summary(&mut self) -> Option<String> {
        self.last_summary.take()
    }

    fn invalidate_tables(&mut self) {
        self.tables_generation += 1;
        self.tables.clear();
        self.tables_loading = false;
    }

    fn selected_table_pairs(&self) -> Vec<(Option<String>, String)> {
        self.selected
            .iter()
            .filter_map(|key| {
                self.tables
                    .iter()
                    .find(|table| table.key == *key)
                    .map(|table| (table.schema.clone(), table.name.clone()))
            })
            .collect()
    }

    fn gate_error(&self) -> Option<String> {
        let (Some(source_profile), Some(target_profile), Some(source_db), Some(target_db)) = (
            self.source_profile.as_ref(),
            self.target_profile.as_ref(),
            self.source_db.as_ref(),
            self.target_db.as_ref(),
        ) else {
            return None;
        };
        if let Some(error) = target_route_error(target_profile) {
            return Some(error);
        }
        if let Some(error) = target_writes_error(target_profile) {
            return Some(error);
        }
        same_source_and_target_error(
            source_profile.id.0,
            target_profile.id.0,
            source_db,
            target_db,
            &self.target_schema,
            &self.selected_table_pairs(),
        )
    }

    fn refresh_plan_preview(&mut self, cx: &mut gpui::Context<Self>) {
        let _ = cx;
        self.plan_warnings.clear();
        if self.selected.is_empty() {
            return;
        }
        let (Some(source_profile), Some(target_profile)) =
            (self.source_profile.clone(), self.target_profile.clone())
        else {
            return;
        };
        let plan = plan_from_selection(
            source_profile.database_type,
            target_profile.database_type,
            Some(self.target_schema.clone()).filter(|schema| !schema.trim().is_empty()),
            self.mode,
            self.casing,
            &self.selected.iter().cloned().collect(),
            &[],
            &[],
            &[],
        );
        self.plan_warnings = collect_plan_warnings(&plan);
    }

    fn load_databases(&mut self, side: TransferSide, cx: &mut gpui::Context<Self>) {
        let profile = match side {
            TransferSide::Source => self.source_profile.clone(),
            TransferSide::Target => self.target_profile.clone(),
        };
        let Some(profile) = profile else { return };
        let generation = match side {
            TransferSide::Source => {
                self.source_database_generation += 1;
                self.source_databases_loading = true;
                self.source_database_generation
            }
            TransferSide::Target => {
                self.target_database_generation += 1;
                self.target_databases_loading = true;
                self.target_database_generation
            }
        };
        let Some(sql) = database_list_sql(profile.database_type) else {
            let database = profile
                .database
                .clone()
                .unwrap_or_else(|| profile.server.clone());
            self.apply_databases(side, generation, vec![database], None, cx);
            return;
        };
        let context = ToolDialogContext::new(profile.clone(), self.router.services_for(&profile));
        crate::tool_dialog::run_query(&context, sql.to_string(), cx, move |this, outcome, cx| {
            let databases = outcome
                .sets
                .first()
                .map(|set| {
                    set.rows
                        .iter()
                        .filter_map(|row| match row.first() {
                            Some(CellValue::String(value)) => Some(value.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            this.apply_databases(side, generation, databases, outcome.error, cx);
        });
    }

    fn apply_databases(
        &mut self,
        side: TransferSide,
        generation: u64,
        databases: Vec<String>,
        error: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        let (current_generation, loading, values, selected, profile) = match side {
            TransferSide::Source => (
                &self.source_database_generation,
                &mut self.source_databases_loading,
                &mut self.source_databases,
                &mut self.source_db,
                self.source_profile.as_ref(),
            ),
            TransferSide::Target => (
                &self.target_database_generation,
                &mut self.target_databases_loading,
                &mut self.target_databases,
                &mut self.target_db,
                self.target_profile.as_ref(),
            ),
        };
        if *current_generation != generation {
            return;
        }
        *loading = false;
        if let Some(error) = error {
            self.error = Some(crate::localization::format(
                "results.data_transfer.database_list_failed",
                &[
                    (
                        "profile_name",
                        profile.map(|profile| profile.name.as_str()).unwrap_or(""),
                    ),
                    ("error", &error),
                ],
            ));
        } else {
            *values = databases;
            if selected.is_none() {
                *selected = profile
                    .and_then(|profile| profile.database.clone())
                    .filter(|database| values.contains(database))
                    .or_else(|| values.first().cloned());
            }
            self.error = None;
        }
        self.selects_dirty = true;
        if matches!(side, TransferSide::Source) {
            self.load_tables(cx);
        }
        cx.notify();
    }

    fn load_tables(&mut self, cx: &mut gpui::Context<Self>) {
        let (Some(mut profile), Some(database)) =
            (self.source_profile.clone(), self.source_db.clone())
        else {
            return;
        };
        profile.database = Some(database.clone());
        self.tables_loading = true;
        self.tables_generation += 1;
        let generation = self.tables_generation;
        let services = self.router.services_for(&profile);
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<Vec<TableEntry>, String>>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let outcome = services
                        .metadata
                        .list_tables(&profile, &database, None)
                        .await
                        .map(|nodes| {
                            nodes
                                .into_iter()
                                .filter(|node| matches!(node.kind, sqlly_core::ObjectKind::Table))
                                .map(|node| {
                                    let schema = node.schema.filter(|s| !s.is_empty());
                                    let key = table_key(schema.as_deref(), &node.name);
                                    TableEntry {
                                        schema,
                                        name: node.name,
                                        key,
                                    }
                                })
                                .collect::<Vec<_>>()
                        })
                        .map_err(|error| error.to_string());
                    let _ = tx.send(outcome).await;
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        cx.spawn(async move |_this, _cx| {
            if let Some(outcome) = rx.recv().await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.apply_tables(generation, outcome, cx);
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn apply_tables(
        &mut self,
        generation: u64,
        outcome: Result<Vec<TableEntry>, String>,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.tables_generation != generation {
            return;
        }
        self.tables_loading = false;
        match outcome {
            Ok(tables) => {
                self.tables = tables;
                self.selected
                    .retain(|key| self.tables.iter().any(|t| t.key == *key));
                self.error = None;
            }
            Err(message) => {
                self.error = Some(message);
                self.tables.clear();
            }
        }
        self.refresh_plan_preview(cx);
        cx.notify();
    }

    fn toggle_table(&mut self, key: &str, checked: bool, cx: &mut gpui::Context<Self>) {
        if checked {
            self.selected.insert(key.to_string());
        } else {
            self.selected.remove(key);
        }
        self.refresh_plan_preview(cx);
        cx.notify();
    }

    fn run_enabled(&self) -> bool {
        !self.running
            && !self.selected.is_empty()
            && self.source_profile.is_some()
            && self.target_profile.is_some()
            && self.source_db.is_some()
            && self.target_db.is_some()
            && self.gate_error().is_none()
            && self.error.is_none()
    }

    fn run_transfer(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.run_enabled() {
            return;
        }

        #[cfg(target_arch = "wasm32")]
        {
            self.error = Some(crate::localization::text(
                "results.data_transfer.browser_unavailable",
            ));
            cx.notify();
            return;
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let Some(mut source_profile) = self.source_profile.clone() else {
                return;
            };
            let Some(mut target_profile) = self.target_profile.clone() else {
                return;
            };
            let Some(source_db) = self.source_db.clone() else {
                return;
            };
            let Some(target_db) = self.target_db.clone() else {
                return;
            };
            source_profile.database = Some(source_db.clone());
            target_profile.database = Some(target_db.clone());

            let selected_keys: BTreeSet<String> = self.selected.iter().cloned().collect();
            let mode = self.mode;
            let casing = self.casing;
            let target_schema = Some(self.target_schema.clone()).filter(|s| !s.trim().is_empty());
            let source_services = self.router.services_for(&source_profile);
            let cancel = TransferCancel::default();
            self.cancel = cancel.clone();
            self.running = true;
            self.progress_table = None;
            self.progress_rows = 0;
            self.error = None;

            let (plan_tx, mut plan_rx) =
                tokio::sync::mpsc::channel::<Result<TransferPlan, String>>(1);
            let (progress_tx, mut progress_rx) = tokio::sync::mpsc::channel::<TransferProgress>(64);

            cx.background_spawn({
                let source_services = source_services.clone();
                let source_profile = source_profile.clone();
                async move {
                    crate::tokio_handle()
                        .spawn(async move {
                            let outcome = async {
                                let columns = source_services
                                    .metadata
                                    .list_all_columns(&source_profile, &source_db)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let primary_keys = source_services
                                    .metadata
                                    .list_primary_keys(&source_profile, &source_db)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                let foreign_keys = source_services
                                    .metadata
                                    .list_foreign_keys(&source_profile, &source_db)
                                    .await
                                    .map_err(|e| e.to_string())?;
                                Ok(plan_from_selection(
                                    source_profile.database_type,
                                    target_profile.database_type,
                                    target_schema,
                                    mode,
                                    casing,
                                    &selected_keys,
                                    &columns,
                                    &primary_keys,
                                    &foreign_keys,
                                ))
                            }
                            .await;
                            let _ = plan_tx.send(outcome).await;
                        })
                        .await
                        .ok();
                }
            })
            .detach();

            let async_cx = cx.to_async();
            let weak_self = cx.weak_entity();
            cx.spawn(async move |_this, _cx| {
                let plan = match plan_rx.recv().await {
                    Some(Ok(plan)) => plan,
                    Some(Err(message)) => {
                        let mut cx = async_cx.clone();
                        weak_self
                            .update(&mut cx, |this, cx| {
                                this.running = false;
                                this.error = Some(message);
                                cx.notify();
                            })
                            .ok();
                        return;
                    }
                    None => return,
                };

                let source_services = source_services.clone();
                let source_profile = source_profile.clone();
                let target_profile = target_profile.clone();
                let cancel = cancel.clone();
                let progress_tx = progress_tx.clone();
                let mut emit_cx = async_cx.clone();
                weak_self
                    .update(&mut emit_cx, |_, cx| {
                        cx.emit(ToolDialogEvent::TransferStarted);
                    })
                    .ok();
                _cx.background_spawn(async move {
                    crate::tokio_handle()
                        .spawn(async move {
                            let mut sink = match open_sink(&target_profile).await {
                                Ok(sink) => sink,
                                Err(error) => {
                                    let outcome = TransferOutcome {
                                        error: Some(sqlly_core::DisplayMessage::raw(
                                            error.to_string(),
                                        )),
                                        ..Default::default()
                                    };
                                    let _ =
                                        progress_tx.send(TransferProgress::Finished(outcome)).await;
                                    return;
                                }
                            };
                            let outcome = run_transfer(
                                &source_services,
                                &source_profile,
                                &mut sink,
                                &plan,
                                progress_tx.clone(),
                                cancel,
                            )
                            .await;
                            let _ = progress_tx.send(TransferProgress::Finished(outcome)).await;
                        })
                        .await
                        .ok();
                })
                .detach();

                while let Some(event) = progress_rx.recv().await {
                    let mut cx = async_cx.clone();
                    let finished = matches!(event, TransferProgress::Finished(_));
                    weak_self
                        .update(&mut cx, |this, cx| {
                            this.apply_progress(event, cx);
                        })
                        .ok();
                    if finished {
                        break;
                    }
                }
            })
            .detach();
            cx.notify();
        }
    }

    fn apply_progress(&mut self, event: TransferProgress, cx: &mut gpui::Context<Self>) {
        match event {
            TransferProgress::TableStarted {
                table,
                index,
                total,
            } => {
                self.progress_table = Some(table);
                self.progress_rows = 0;
                self.progress_index = index + 1;
                self.progress_total = total;
                self.last_task_rows = 0;
            }
            TransferProgress::Rows { copied, .. } => {
                if let Some(task_progress) = &self.task_progress {
                    let delta = copied.saturating_sub(self.last_task_rows);
                    if delta > 0 {
                        task_progress.add_rows(delta);
                    }
                    if task_progress.is_cancelled() {
                        self.cancel.request();
                    }
                }
                self.last_task_rows = copied;
                self.progress_rows = copied;
            }
            TransferProgress::TableFinished { .. } => {}
            TransferProgress::Warning(message) => {
                self.plan_warnings
                    .push(crate::localization::message(message));
            }
            TransferProgress::Finished(outcome) => {
                self.running = false;
                self.progress_table = None;
                let level = if outcome.error.is_some() {
                    StatusLevel::Error
                } else if outcome.cancelled {
                    StatusLevel::Info
                } else {
                    StatusLevel::Success
                };
                let message = if outcome.error.is_some() {
                    crate::localization::text("results.data_transfer.failed_status")
                } else if outcome.cancelled {
                    crate::localization::text("results.data_transfer.stopped_status")
                } else {
                    crate::localization::text("results.data_transfer.finished_status")
                };
                let plan = plan_from_selection(
                    self.source_profile
                        .as_ref()
                        .map(|profile| profile.database_type)
                        .unwrap_or(DatabaseType::SqlServer),
                    self.target_profile
                        .as_ref()
                        .map(|profile| profile.database_type)
                        .unwrap_or(DatabaseType::SqlServer),
                    Some(self.target_schema.clone()).filter(|s| !s.trim().is_empty()),
                    self.mode,
                    self.casing,
                    &self.selected.iter().cloned().collect(),
                    &[],
                    &[],
                    &[],
                );
                self.last_summary = Some(format_outcome_summary(&plan, &outcome));
                cx.emit(ToolDialogEvent::TransferFinished {
                    cancelled: outcome.cancelled,
                    failed: outcome.error.is_some(),
                });
                cx.emit(ToolDialogEvent::Status { level, message });
            }
        }
        cx.notify();
    }

    pub fn attach_task(&mut self, progress: crate::background_tasks::TaskProgress) {
        self.task_progress = Some(progress);
        self.last_task_rows = 0;
    }

    fn stop(&mut self, cx: &mut gpui::Context<Self>) {
        if self.running {
            self.cancel.request();
            cx.notify();
        }
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.running {
            self.stop(cx);
            return;
        }
        if self.windowed {
            window.remove_window();
            return;
        }
        cx.emit(ToolDialogEvent::Close);
    }

    /// Pop out into a standalone OS window: the same pane keeps running (an
    /// in-flight transfer included), the inline overlay closes, and the
    /// window boundary becomes the dialog boundary.
    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        // Opening the window renders this pane into it immediately, which
        // must not happen inside the pane's own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "results.data_transfer.detached_title",
                    Theme::scaled(880.0),
                    Theme::scaled(680.0),
                ),
                None,
                cx,
                move |_, cx| {
                    // Flip to the windowed presentation before the new
                    // window's first render.
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the pane now; drop the inline overlay.
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }
}

impl gpui::Focusable for DataTransferPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DataTransferPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            let profile_items = self.profiles.iter().map(profile_label).collect::<Vec<_>>();
            let source_profile_index = self
                .source_profile
                .as_ref()
                .and_then(|selected| {
                    self.profiles
                        .iter()
                        .position(|profile| profile.id == selected.id)
                })
                .map(gpui_component::IndexPath::new);
            let target_profile_index = self
                .target_profile
                .as_ref()
                .and_then(|selected| {
                    self.profiles
                        .iter()
                        .position(|profile| profile.id == selected.id)
                })
                .map(gpui_component::IndexPath::new);
            let source_database_index = self
                .source_db
                .as_ref()
                .and_then(|database| {
                    self.source_databases
                        .iter()
                        .position(|candidate| candidate == database)
                })
                .map(gpui_component::IndexPath::new);
            let target_database_index = self
                .target_db
                .as_ref()
                .and_then(|database| {
                    self.target_databases
                        .iter()
                        .position(|candidate| candidate == database)
                })
                .map(gpui_component::IndexPath::new);
            self.source_profile_select.update(cx, |state, cx| {
                state.set_items(profile_items.clone(), window, cx);
                state.set_selected_index(source_profile_index, window, cx);
            });
            self.target_profile_select.update(cx, |state, cx| {
                state.set_items(profile_items, window, cx);
                state.set_selected_index(target_profile_index, window, cx);
            });
            self.source_database_select.update(cx, |state, cx| {
                state.set_items(self.source_databases.clone(), window, cx);
                state.set_selected_index(source_database_index, window, cx);
            });
            self.target_database_select.update(cx, |state, cx| {
                state.set_items(self.target_databases.clone(), window, cx);
                state.set_selected_index(target_database_index, window, cx);
            });
            self.mode_select.update(cx, |state, cx| {
                state.set_selected_index(Some(gpui_component::IndexPath::new(0)), window, cx);
            });
            self.casing_select.update(cx, |state, cx| {
                state.set_selected_index(Some(gpui_component::IndexPath::new(0)), window, cx);
            });
        }

        let gate_error = self.gate_error();
        let summary = summary_text(
            self.selected.len(),
            self.target_profile
                .as_ref()
                .map(|profile| profile.name.as_str())
                .unwrap_or("target"),
            self.target_db.as_deref().unwrap_or("database"),
            self.target_schema.trim(),
            self.mode,
            &self.plan_warnings,
            gate_error.as_deref(),
        );

        let mut table_list = div()
            .id("data-transfer-tables")
            .flex_1()
            .min_h(px(0.0))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(px(s(2.0)));
        if self.tables_loading {
            table_list = table_list.child(div().text_color(Theme::text_dim()).child(
                crate::localization::text("results.data_transfer.loading_tables"),
            ));
        } else if let Some(error) = &self.error {
            table_list = table_list.child(
                div()
                    .text_color(Theme::error())
                    .whitespace_normal()
                    .child(error.clone()),
            );
        } else if self.tables.is_empty() {
            table_list = table_list.child(div().text_color(Theme::text_dim()).child(
                crate::localization::text("results.data_transfer.empty_table_instructions"),
            ));
        } else {
            for table in &self.tables {
                let key = table.key.clone();
                let label = table.label();
                let checked = self.selected.contains(&key);
                let weak = cx.weak_entity();
                table_list = table_list.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(6.0)))
                        .child(
                            Checkbox::new(format!("dt-{key}"))
                                .checked(checked)
                                .label(label)
                                .on_click(move |checked: &bool, _window, app| {
                                    if let Some(this) = weak.upgrade() {
                                        this.update(app, |this, cx| {
                                            this.toggle_table(&key, *checked, cx);
                                        });
                                    }
                                }),
                        ),
                );
            }
        }

        let progress_line = if self.running {
            Some(crate::localization::format(
                "results.data_transfer.copy_progress",
                &[
                    ("table_name", self.progress_table.as_deref().unwrap_or("…")),
                    ("row_count", &self.progress_rows.to_string()),
                    ("current_table", &self.progress_index.to_string()),
                    ("table_count", &self.progress_total.max(1).to_string()),
                ],
            ))
        } else {
            None
        };

        let running = self.running;
        let run_enabled = self.run_enabled();
        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("data-transfer", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "results.data_transfer.dialog_title",
            ))
            .key_context("DataTransferPane")
            .track_focus(&focus_handle)
            .bare_body()
            .windowed(self.windowed)
            // While a transfer runs, close() stops it instead of dismissing —
            // ✕ and the Close button share that guard.
            .on_close(|this, window, cx| this.close(window, cx))
            .child(
                // The whole form scrolls: it is taller than the card on
                // smaller frames, and nothing may ever clip under the footer.
                div()
                    .id("data-transfer-body")
                    .flex_1()
                    .min_h(px(0.0))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(s(7.0)))
                            .child(form_row(
                                crate::localization::text(
                                    "results.data_transfer.source_profile_label",
                                ),
                                Select::new(&self.source_profile_select).w_full(),
                            ))
                            .child(form_row(
                                crate::localization::text(
                                    "results.data_transfer.source_database_label",
                                ),
                                Select::new(&self.source_database_select).w_full(),
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_h(px(s(120.0)))
                                    .border_1()
                                    .border_color(Theme::border())
                                    .rounded(px(s(4.0)))
                                    .child(table_list),
                            )
                            .child(form_row(
                                crate::localization::text(
                                    "results.data_transfer.target_profile_label",
                                ),
                                Select::new(&self.target_profile_select).w_full(),
                            ))
                            .child(form_row(
                                crate::localization::text(
                                    "results.data_transfer.target_database_label",
                                ),
                                Select::new(&self.target_database_select).w_full(),
                            ))
                            .child(form_row(
                                crate::localization::text(
                                    "results.data_transfer.target_schema_label",
                                ),
                                Input::new(&self.target_schema_input).w_full(),
                            ))
                            .child(form_row(
                                crate::localization::text("results.data_transfer.mode_label"),
                                Select::new(&self.mode_select).w_full(),
                            ))
                            .child(form_row(
                                crate::localization::text("results.data_transfer.casing_label"),
                                Select::new(&self.casing_select).w_full(),
                            )),
                    )
                    .child(
                        div()
                            .p(px(s(10.0)))
                            .bg(Theme::panel_bg())
                            .border_1()
                            .border_color(Theme::border())
                            .rounded(px(s(4.0)))
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .whitespace_normal()
                            .child({
                                let mut block = div().flex().flex_col().gap(px(s(4.0)));
                                for line in summary.lines() {
                                    let color = if gate_error.is_some()
                                        && line == gate_error.as_deref().unwrap_or("")
                                    {
                                        Theme::error()
                                    } else if self.plan_warnings.iter().any(|w| w == line) {
                                        Theme::warning()
                                    } else {
                                        Theme::text()
                                    };
                                    block = block
                                        .child(div().text_color(color).child(line.to_string()));
                                }
                                block
                            }),
                    ),
            );
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        if let Some(line) = progress_line {
            dialog = dialog.leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(line),
            );
        }
        dialog
            .button(
                Button::new("dt-run")
                    .primary()
                    .label(if running {
                        crate::localization::text("results.data_transfer.stop_button")
                    } else {
                        crate::localization::text("results.data_transfer.run_button")
                    })
                    .disabled(!running && !run_enabled)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        if this.running {
                            this.stop(cx);
                        } else {
                            this.run_transfer(cx);
                        }
                    })),
            )
            .build(self, window, cx)
    }
}

fn form_row(label: impl Into<String>, control: impl IntoElement) -> gpui::Div {
    let s = Theme::scaled;
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(s(8.0)))
        .h(px(Theme::control_height()))
        .child(
            div()
                .w(px(s(220.0)))
                .flex_shrink_0()
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size_ratio(0.84)))
                .child(label.into()),
        )
        .child(div().flex_1().child(control))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::metadata::{BulkColumnRow, ForeignKeyRow};

    #[test]
    fn mode_and_casing_dispatch_on_row() {
        for (row, mode) in MODE_CHOICES.iter().enumerate() {
            assert_eq!(mode_from_row(Some(row)), *mode);
        }
        assert_eq!(mode_from_row(None), TransferMode::CreateTable);
        for (row, casing) in CASING_CHOICES.iter().enumerate() {
            assert_eq!(casing_from_row(Some(row)), *casing);
        }
        assert_eq!(casing_from_row(None), NameCasing::AsIs);
    }

    #[test]
    fn default_target_schema_per_engine() {
        assert_eq!(default_target_schema(DatabaseType::SqlServer), "dbo");
        assert_eq!(default_target_schema(DatabaseType::AzureSqlDatabase), "dbo");
        assert_eq!(default_target_schema(DatabaseType::Postgres), "public");
        assert_eq!(default_target_schema(DatabaseType::MySql), "");
        assert_eq!(default_target_schema(DatabaseType::Sqlite), "");
    }

    #[test]
    fn same_source_and_target_is_rejected() {
        let id = uuid::Uuid::new_v4();
        let err = same_source_and_target_error(
            id,
            id,
            "AppDb",
            "AppDb",
            "dbo",
            &[(Some("dbo".into()), "Customers".into())],
        );
        assert_eq!(
            err.as_deref(),
            Some("Source and target are the same table set")
        );
        assert!(
            same_source_and_target_error(
                id,
                uuid::Uuid::new_v4(),
                "AppDb",
                "AppDb",
                "dbo",
                &[(Some("dbo".into()), "Customers".into())],
            )
            .is_none()
        );
    }

    #[test]
    fn plan_from_selection_marks_fk_parents_in_selection_only() {
        let selected = BTreeSet::from([
            table_key(Some("dbo"), "Orders"),
            table_key(Some("dbo"), "Customers"),
        ]);
        let columns = vec![
            BulkColumnRow {
                schema: "dbo".into(),
                object: "Orders".into(),
                column: "id".into(),
                type_name: Some("int".into()),
                nullable: Some(false),
                is_identity: false,
            },
            BulkColumnRow {
                schema: "dbo".into(),
                object: "Orders".into(),
                column: "customer_id".into(),
                type_name: Some("int".into()),
                nullable: Some(false),
                is_identity: false,
            },
            BulkColumnRow {
                schema: "dbo".into(),
                object: "Customers".into(),
                column: "id".into(),
                type_name: Some("int".into()),
                nullable: Some(false),
                is_identity: false,
            },
        ];
        let foreign_keys = vec![ForeignKeyRow {
            constraint_name: "FK_Orders_Customers".into(),
            from_schema: "dbo".into(),
            from_table: "Orders".into(),
            from_column: "customer_id".into(),
            to_schema: "dbo".into(),
            to_table: "Customers".into(),
            to_column: "id".into(),
        }];
        let plan = plan_from_selection(
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            Some("public".into()),
            TransferMode::Append,
            NameCasing::AsIs,
            &selected,
            &columns,
            &[],
            &foreign_keys,
        );
        let orders = plan
            .tables
            .iter()
            .find(|table| table.name == "Orders")
            .expect("orders table");
        assert_eq!(orders.depends_on, vec!["dbo.customers".to_string()]);

        let child_only = BTreeSet::from([table_key(Some("dbo"), "Orders")]);
        let plan_child_only = plan_from_selection(
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            Some("public".into()),
            TransferMode::Append,
            NameCasing::AsIs,
            &child_only,
            &columns,
            &[],
            &foreign_keys,
        );
        let orders_only = plan_child_only
            .tables
            .iter()
            .find(|table| table.name == "Orders")
            .expect("orders table");
        assert!(orders_only.depends_on.is_empty());
    }

    #[test]
    fn summary_text_lists_mode_and_warnings() {
        let text = summary_text(
            2,
            "Test PG",
            "app",
            "public",
            TransferMode::TruncateThenInsert,
            &["dbo.t.amount: money → numeric (Widened)".into()],
            None,
        );
        assert!(text.contains("2 table(s) → Test PG / app.public"));
        assert!(text.contains("Mode: Truncate then insert"));
        assert!(text.contains("Row estimate: unknown"));
        assert!(text.contains("money → numeric"));
    }
}
