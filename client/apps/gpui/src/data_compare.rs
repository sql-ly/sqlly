//! Cross-connection table data compare with review-only sync scripting.

#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_datatable::SqllyDataTable;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};
use sqlly_services::QueryEvent;
use sqlly_services::datacompare::script::{SyncDirection, SyncScriptRequest, generate_sync_script};
use sqlly_services::datacompare::{CompareError, DataDiff, MergeJoiner, Side, key_indexes};
use sqlly_services::metadata::PrimaryKeyRow;

use crate::app::StatusLevel;
use crate::connection::ConnectionStore;
use crate::dialog::{Dialog, DialogFrame};
use crate::result_set_difference::{self, ColumnChange, DetailedDiff};
use crate::schema_compare::collect_objects_sql;
use crate::service_router::ServiceRouter;
use crate::table_actions::QualifiedTable;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQuerySet};

const MAX_COMPARE_PROFILES: usize = 200;

fn direction_items() -> Vec<String> {
    vec![
        crate::localization::text("results.data_compare.sync_left_to_right"),
        crate::localization::text("results.data_compare.sync_right_to_left"),
    ]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ComparePhase {
    Idle,
    LoadingKeys,
    Comparing,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataSide {
    Left,
    Right,
}

struct SideState {
    profile: Option<ConnectionProfile>,
    databases: Vec<String>,
    db: Option<String>,
    databases_loading: bool,
    database_generation: u64,
    profile_select: Entity<SelectState<Vec<String>>>,
    database_select: Entity<SelectState<Vec<String>>>,
    use_query: bool,
    object_input: Entity<TextareaState>,
}

impl EventEmitter<ToolDialogEvent> for DataComparePane {}

pub struct DataComparePane {
    router: ServiceRouter,
    profiles: Vec<ConnectionProfile>,
    left: SideState,
    right: SideState,
    selects_dirty: bool,
    key_input: Entity<InputState>,
    direction_select: Entity<SelectState<Vec<String>>>,
    direction: SyncDirection,
    phase: ComparePhase,
    error: Option<String>,
    diff: Option<DetailedDiff>,
    columns: Vec<ResultColumn>,
    key_indexes: Vec<usize>,
    truncated: Option<String>,
    class_filter: Option<&'static str>,
    grid: Option<Entity<SqllyDataTable>>,
    generation: u64,
    task_ids: Vec<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
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
        ConnectionKind::ClickHouseHttp => Some(
            "SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name",
        ),
        ConnectionKind::SqliteFile => None,
        ConnectionKind::DuckDbFile => None,
        _ => None,
    }
}

/// Directions in the order of [`direction_items`].
const DIRECTION_CHOICES: [SyncDirection; 2] =
    [SyncDirection::LeftToRight, SyncDirection::RightToLeft];

fn direction_from_row(row: Option<usize>) -> SyncDirection {
    row.and_then(|row| DIRECTION_CHOICES.get(row).copied())
        .unwrap_or(SyncDirection::LeftToRight)
}

pub fn parse_table_input(text: &str) -> (Option<String>, String) {
    let text = text.trim();
    if let Some((schema, table)) = text.split_once('.') {
        (Some(schema.trim().to_string()), table.trim().to_string())
    } else {
        (None, text.to_string())
    }
}

pub fn qualified_table_from_input(text: &str) -> QualifiedTable {
    let (schema, table) = parse_table_input(text);
    let mut table = QualifiedTable::new(table);
    if let Some(schema) = schema {
        table = table.with_schema(schema);
    }
    table
}

pub fn order_by_clause(db: DatabaseType, key_names: &[String]) -> String {
    if key_names.is_empty() {
        return String::new();
    }
    let columns = key_names
        .iter()
        .map(|name| db.quote_identifier(name))
        .collect::<Vec<_>>()
        .join(", ");
    format!(" ORDER BY {columns}")
}

/// The table text is quoted exactly as typed: on case-sensitive catalogs
/// (PostgreSQL, Oracle) `orders` and `ORDERS` name different tables, so the
/// user must type the name as the catalog stores it.
pub fn source_sql_for_table(db: DatabaseType, table_text: &str, key_names: &[String]) -> String {
    let table = qualified_table_from_input(table_text);
    format!(
        "SELECT * FROM {}{}",
        table.schema_qualified_table_for(db),
        order_by_clause(db, key_names)
    )
}

pub fn source_sql_for_query(user_sql: &str, db: DatabaseType, key_names: &[String]) -> String {
    let sql = user_sql.trim().trim_end_matches(';').trim();
    // Oracle rejects `AS` before a table alias; everywhere else it is
    // accepted (and MySQL requires an alias on a derived table).
    let alias = if db.connection_kind() == ConnectionKind::OracleNet {
        "sqlly_cmp"
    } else {
        "AS sqlly_cmp"
    };
    format!(
        "SELECT * FROM ({sql}) {alias}{}",
        order_by_clause(db, key_names)
    )
}

pub fn parse_key_input(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn pk_columns_for(rows: &[PrimaryKeyRow], schema: &str, table: &str) -> Vec<String> {
    let mut matches = rows
        .iter()
        .filter(|row| {
            row.schema.eq_ignore_ascii_case(schema) && row.table.eq_ignore_ascii_case(table)
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|row| row.key_ordinal);
    matches.into_iter().map(|row| row.column.clone()).collect()
}

/// Primary-key columns for the table as the user typed it. A
/// schema-qualified name matches exactly. An unqualified one uses the
/// engine's default schema when it has one (`dbo`, `public`, `main`);
/// engines without one (SQLite, whose catalog reports `main`; MySQL, a
/// database; Oracle, an owner) use the table's only schema in the catalog.
/// An unqualified name found in several schemas matches nothing, so the user
/// is asked for key columns instead of having a key guessed.
pub fn pk_columns_for_table_input(
    rows: &[PrimaryKeyRow],
    database_type: DatabaseType,
    table_text: &str,
) -> Vec<String> {
    let (schema, table) = parse_table_input(table_text);
    if let Some(schema) = schema {
        return pk_columns_for(rows, &schema, &table);
    }
    if let Some(default) = database_type.default_schema() {
        return pk_columns_for(rows, default, &table);
    }
    let mut schemas: Vec<&str> = rows
        .iter()
        .filter(|row| row.table.eq_ignore_ascii_case(&table))
        .map(|row| row.schema.as_str())
        .collect();
    schemas.sort_unstable_by_key(|schema| schema.to_ascii_lowercase());
    schemas.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    match schemas.as_slice() {
        [only] => pk_columns_for(rows, only, &table),
        _ => Vec::new(),
    }
}

pub fn to_detailed_diff(
    diff: DataDiff,
    columns: &[ResultColumn],
    key_indexes: &[usize],
) -> DetailedDiff {
    DetailedDiff {
        added: diff.only_right,
        removed: diff.only_left,
        changed: diff
            .changed
            .into_iter()
            .map(|pair| result_set_difference::ChangedRow {
                key: pair.key,
                before: pair.left.clone(),
                after: pair.right.clone(),
                changes: pair
                    .changed_columns
                    .into_iter()
                    .map(|index| ColumnChange {
                        column_index: index,
                        column_name: columns
                            .get(index)
                            .map(|column| column.name.clone())
                            .unwrap_or_default(),
                        before: pair.left.get(index).cloned().unwrap_or(CellValue::Null),
                        after: pair.right.get(index).cloned().unwrap_or(CellValue::Null),
                    })
                    .collect(),
            })
            .collect(),
        unchanged_count: diff.unchanged,
        key_columns: key_indexes.to_vec(),
    }
}

fn compare_error_message(error: CompareError) -> String {
    match error {
        CompareError::MissingColumn { side, column } => {
            let key = match side {
                Side::Left => "results.data_compare.left_column_missing",
                Side::Right => "results.data_compare.right_column_missing",
            };
            crate::localization::format(key, &[("column_name", &column)])
        }
        CompareError::NullKey { side, row } => {
            let key = match side {
                Side::Left => "results.data_compare.left_null_key",
                Side::Right => "results.data_compare.right_null_key",
            };
            crate::localization::format(key, &[("row_number", &row.to_string())])
        }
        CompareError::DuplicateKey { side, row } => {
            let key = match side {
                Side::Left => "results.data_compare.left_duplicate_key",
                Side::Right => "results.data_compare.right_duplicate_key",
            };
            crate::localization::format(key, &[("row_number", &row.to_string())])
        }
        CompareError::OutOfOrder { .. } => {
            crate::localization::text("results.data_compare.unexpected_row_order")
        }
        CompareError::NoKeyColumns => {
            crate::localization::text("results.data_compare.key_columns_required")
        }
    }
}

impl DataComparePane {
    pub fn new(
        context: ToolDialogContext,
        router: ServiceRouter,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let profiles = ConnectionStore::global()
            .list()
            .into_iter()
            .filter(|profile| collect_objects_sql(profile.database_type, "probe").is_ok())
            .take(MAX_COMPARE_PROFILES)
            .collect::<Vec<_>>();
        Self::with_profiles(context, router, profiles, window, cx)
    }

    /// Build the pane over an explicit connection list (the saved store in
    /// the app; a hermetic list in tests). The context's own profile is
    /// always offered and pre-selected on the left.
    fn with_profiles(
        context: ToolDialogContext,
        router: ServiceRouter,
        mut profiles: Vec<ConnectionProfile>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        if !profiles
            .iter()
            .any(|profile| profile.id == context.profile.id)
        {
            profiles.push(context.profile.clone());
        }
        profiles.sort_by_key(profile_label);

        let left_profile = profiles
            .iter()
            .find(|profile| profile.id == context.profile.id)
            .cloned();
        let right_profile = profiles
            .iter()
            .find(|profile| Some(profile.id) != left_profile.as_ref().map(|left| left.id))
            .cloned()
            .or_else(|| left_profile.clone());

        let mut subscriptions = Vec::new();
        let left = Self::new_side(
            DataSide::Left,
            left_profile.clone(),
            &profiles,
            window,
            cx,
            &mut subscriptions,
        );
        let right = Self::new_side(
            DataSide::Right,
            right_profile.clone(),
            &profiles,
            window,
            cx,
            &mut subscriptions,
        );

        let key_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.data_compare.key_columns_placeholder",
            ))
        });
        let direction_select = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, direction_items),
                None,
                window,
                cx,
            )
        });

        subscriptions.push(cx.subscribe_in(
            &direction_select,
            window,
            |this, select, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                this.direction = if value.is_some() {
                    direction_from_row(select.read(cx).selected_index(cx).map(|ix| ix.row))
                } else {
                    SyncDirection::LeftToRight
                };
                cx.notify();
            },
        ));

        let mut this = Self {
            router,
            profiles,
            left,
            right,
            selects_dirty: true,
            key_input,
            direction_select,
            direction: SyncDirection::LeftToRight,
            phase: ComparePhase::Idle,
            error: None,
            diff: None,
            columns: Vec::new(),
            key_indexes: Vec::new(),
            truncated: None,
            class_filter: None,
            grid: None,
            generation: 0,
            task_ids: Vec::new(),
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(940.0), Theme::scaled(640.0)),
            _subscriptions: subscriptions,
        };
        this.load_databases(DataSide::Left, cx);
        this.load_databases(DataSide::Right, cx);
        this
    }

    fn new_side(
        side: DataSide,
        profile: Option<ConnectionProfile>,
        _profiles: &[ConnectionProfile],
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
        subscriptions: &mut Vec<Subscription>,
    ) -> SideState {
        let profile_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let database_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let object_input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .placeholder("schema.table")
        });

        let profile_select_entity = profile_select.clone();
        subscriptions.push(cx.subscribe_in(
            &profile_select,
            window,
            move |this, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let target = match side {
                    DataSide::Left => &mut this.left,
                    DataSide::Right => &mut this.right,
                };
                target.profile = value
                    .as_ref()
                    .and_then(|label| {
                        this.profiles
                            .iter()
                            .find(|profile| profile_label(profile) == label.as_str())
                    })
                    .cloned();
                this.invalidate_comparison();
                this.load_databases(side, cx);
                cx.notify();
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &database_select,
            window,
            move |this, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                let SelectEvent::Confirm(value) = event;
                let target = match side {
                    DataSide::Left => &mut this.left,
                    DataSide::Right => &mut this.right,
                };
                target.db = value.as_ref().map(|value| value.to_string());
                this.invalidate_comparison();
                cx.notify();
            },
        ));

        subscriptions.push(cx.subscribe_in(
            &object_input,
            window,
            move |this, _, _event: &InputEvent, _window, cx| {
                this.invalidate_comparison();
                cx.notify();
            },
        ));

        let _ = profile_select_entity;
        SideState {
            profile,
            databases: Vec::new(),
            db: None,
            databases_loading: false,
            database_generation: 0,
            profile_select,
            database_select,
            use_query: false,
            object_input,
        }
    }

    fn side_mut(&mut self, side: DataSide) -> &mut SideState {
        match side {
            DataSide::Left => &mut self.left,
            DataSide::Right => &mut self.right,
        }
    }

    fn invalidate_comparison(&mut self) {
        self.generation += 1;
        self.phase = ComparePhase::Idle;
        self.diff = None;
        self.grid = None;
        self.error = None;
        self.columns.clear();
        self.key_indexes.clear();
        self.truncated = None;
        self.task_ids.clear();
    }

    fn load_databases(&mut self, side: DataSide, cx: &mut gpui::Context<Self>) {
        let state = self.side_mut(side);
        let Some(profile) = state.profile.clone() else {
            return;
        };
        state.database_generation += 1;
        let generation = state.database_generation;
        state.databases_loading = true;
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
        side: DataSide,
        generation: u64,
        databases: Vec<String>,
        error: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        let state = self.side_mut(side);
        if state.database_generation != generation {
            return;
        }
        state.databases_loading = false;
        if let Some(error) = error {
            self.error = Some(crate::localization::format(
                "results.data_compare.database_list_failed",
                &[
                    (
                        "profile_name",
                        state
                            .profile
                            .as_ref()
                            .map(|profile| profile.name.as_str())
                            .unwrap_or(""),
                    ),
                    ("error", &error),
                ],
            ));
        } else {
            state.databases = databases;
            state.db = state
                .profile
                .as_ref()
                .and_then(|profile| profile.database.clone())
                .filter(|database| state.databases.contains(database))
                .or_else(|| state.databases.first().cloned());
            self.error = None;
        }
        self.selects_dirty = true;
        cx.notify();
    }

    fn object_text(&self, side: DataSide, cx: &gpui::App) -> String {
        self.side_ref(side)
            .object_input
            .read(cx)
            .value()
            .trim()
            .to_string()
    }

    fn side_ref(&self, side: DataSide) -> &SideState {
        match side {
            DataSide::Left => &self.left,
            DataSide::Right => &self.right,
        }
    }

    fn compare(&mut self, cx: &mut gpui::Context<Self>) {
        let left_profile = self.left.profile.clone();
        let right_profile = self.right.profile.clone();
        let left_db = self.left.db.clone();
        let right_db = self.right.db.clone();
        let left_text = self.object_text(DataSide::Left, cx);
        let right_text = self.object_text(DataSide::Right, cx);

        if left_profile.is_none()
            || right_profile.is_none()
            || left_db.is_none()
            || right_db.is_none()
            || left_text.is_empty()
            || right_text.is_empty()
        {
            self.error = Some(crate::localization::text(
                "results.data_compare.selection_required",
            ));
            cx.notify();
            return;
        }

        if left_profile.as_ref().map(|p| p.id) == right_profile.as_ref().map(|p| p.id)
            && left_db == right_db
            && left_text.eq_ignore_ascii_case(&right_text)
        {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.data_compare.same_table_status"),
            });
            return;
        }

        let key_text = self.key_input.read(cx).value();
        let parsed_keys = parse_key_input(&key_text);
        let needs_pk = parsed_keys.is_empty() && !self.left.use_query && !self.right.use_query;
        if parsed_keys.is_empty() && !needs_pk {
            self.error = Some(crate::localization::text(
                "results.data_compare.query_keys_required",
            ));
            cx.notify();
            return;
        }

        if needs_pk {
            self.phase = ComparePhase::LoadingKeys;
            self.error = None;
            self.generation += 1;
            let generation = self.generation;
            let left_profile = left_profile.unwrap();
            let left_db = left_db.unwrap();
            let left_text = left_text.clone();
            let right_profile = right_profile.unwrap();
            let right_db = right_db.unwrap();
            let right_text = right_text.clone();
            let services = self.router.services_for(&left_profile);
            let left_db_for_pk = left_db.clone();
            let left_profile_for_pk = left_profile.clone();
            // The metadata service spawns its query on tokio, so the lookup
            // must run on the app runtime — a bare gpui background thread
            // has no reactor and `tokio::spawn` panics there.
            let pk_task = spawn_on_app_runtime(cx, async move {
                services
                    .metadata
                    .list_primary_keys(&left_profile_for_pk, &left_db_for_pk)
                    .await
            });

            cx.spawn(async move |weak, cx| {
                let pk_result = pk_task.await;
                weak.update(cx, |this, cx| {
                    if this.generation != generation {
                        return;
                    }
                    match pk_result {
                        Ok(rows) => {
                            let keys = pk_columns_for_table_input(
                                &rows,
                                left_profile.database_type,
                                &left_text,
                            );
                            if keys.is_empty() {
                                this.phase = ComparePhase::Idle;
                                this.error = Some(crate::localization::text(
                                    "results.data_compare.left_primary_key_missing",
                                ));
                                cx.notify();
                                return;
                            }
                            this.start_compare_with_keys(
                                generation,
                                left_profile,
                                left_db,
                                left_text,
                                right_profile,
                                right_db,
                                right_text,
                                keys,
                                cx,
                            );
                        }
                        Err(error) => {
                            this.phase = ComparePhase::Idle;
                            this.error = Some(crate::localization::format(
                                "results.data_compare.primary_key_load_failed",
                                &[("error", &error.to_string())],
                            ));
                            cx.notify();
                        }
                    }
                })
                .ok();
            })
            .detach();
            cx.notify();
            return;
        }

        self.start_compare_with_keys(
            self.generation + 1,
            left_profile.expect("validated above"),
            left_db.expect("validated above"),
            left_text,
            right_profile.expect("validated above"),
            right_db.expect("validated above"),
            right_text,
            parsed_keys,
            cx,
        );
    }

    // Cohesive compare launcher; splitting the args would not clarify it.
    #[allow(clippy::too_many_arguments)]
    fn start_compare_with_keys(
        &mut self,
        generation: u64,
        left_profile: ConnectionProfile,
        left_db: String,
        left_text: String,
        right_profile: ConnectionProfile,
        right_db: String,
        right_text: String,
        key_names: Vec<String>,
        cx: &mut gpui::Context<Self>,
    ) {
        self.generation = generation;
        self.phase = ComparePhase::Comparing;
        self.error = None;
        self.diff = None;
        self.grid = None;
        self.truncated = None;
        self.task_ids.clear();

        let mut left_profile_sql = left_profile.clone();
        left_profile_sql.database = Some(left_db.clone());
        let mut right_profile_sql = right_profile.clone();
        right_profile_sql.database = Some(right_db.clone());

        let left_sql = if self.left.use_query {
            source_sql_for_query(&left_text, left_profile.database_type, &key_names)
        } else {
            source_sql_for_table(left_profile.database_type, &left_text, &key_names)
        };
        let right_sql = if self.right.use_query {
            source_sql_for_query(&right_text, right_profile.database_type, &key_names)
        } else {
            source_sql_for_table(right_profile.database_type, &right_text, &key_names)
        };

        // The streaming compare is driven by `run_streaming_compare`, which is
        // native only (see its `tokio::select!`). The browser preview cannot
        // execute a compare, so report it rather than silently stalling.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let left_services = self.router.services_for(&left_profile);
            let right_services = self.router.services_for(&right_profile);

            // The compare drives two streaming queries whose producers are
            // tokio tasks, so it runs on the app runtime (see
            // `spawn_on_app_runtime`). Its task ids arrive as soon as both
            // queries start, so Close can cancel them mid-stream.
            let (task_tx, task_rx) = tokio::sync::oneshot::channel::<(String, String)>();
            let compare_task = spawn_on_app_runtime(
                cx,
                run_streaming_compare(
                    left_services,
                    left_profile_sql,
                    left_sql,
                    right_services,
                    right_profile_sql,
                    right_sql,
                    key_names,
                    task_tx,
                ),
            );

            cx.spawn(async move |weak, cx| {
                if let Ok((left_task, right_task)) = task_rx.await {
                    weak.update(cx, |this, _| {
                        if this.generation == generation {
                            this.task_ids = vec![left_task, right_task];
                        }
                    })
                    .ok();
                }
            })
            .detach();

            cx.spawn(async move |weak, cx| {
                let result = compare_task.await;
                {
                    weak.update(cx, |this, cx| {
                        if this.generation != generation {
                            return;
                        }
                        this.task_ids.clear();
                        match result {
                            Ok((columns, key_indexes, data_diff)) => {
                                this.truncated = data_diff
                                    .truncated
                                    .as_ref()
                                    .map(crate::localization::message);
                                this.columns = columns;
                                this.key_indexes = key_indexes;
                                this.diff = Some(to_detailed_diff(
                                    data_diff,
                                    &this.columns,
                                    &this.key_indexes,
                                ));
                                this.rebuild_grid(cx);
                                this.phase = ComparePhase::Done;
                                this.error = None;
                            }
                            Err(message) => {
                                this.phase = ComparePhase::Idle;
                                this.error = Some(message);
                            }
                        }
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (
                left_profile_sql,
                right_profile_sql,
                left_sql,
                right_sql,
                key_names,
            );
            self.phase = ComparePhase::Idle;
            self.error = Some(crate::localization::text(
                "results.data_compare.browser_unavailable",
            ));
        }
        cx.notify();
    }

    fn rebuild_grid(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(diff) = self.diff.as_ref() else {
            self.grid = None;
            return;
        };
        let (columns, rows) = crate::results::detailed_diff_table(diff, self.class_filter);
        self.grid = crate::tool_dialog::build_result_grid(&ToolQuerySet { columns, rows }, cx);
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(detailed) = self.diff.as_ref() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.data_compare.compare_required"),
            });
            return;
        };
        let destination = match self.direction {
            SyncDirection::LeftToRight => DataSide::Right,
            SyncDirection::RightToLeft => DataSide::Left,
        };
        let dest_state = self.side_ref(destination);
        if dest_state.use_query {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: crate::localization::text("results.data_compare.sync_table_required"),
            });
            return;
        }
        let Some(mut dest_profile) = dest_state.profile.clone() else {
            return;
        };
        let Some(dest_db) = dest_state.db.clone() else {
            return;
        };
        let dest_text = self.object_text(destination, cx);
        let destination_table = qualified_table_from_input(&dest_text)
            .schema_qualified_table_for(dest_profile.database_type);
        let data_diff = DataDiff {
            only_left: detailed.removed.clone(),
            only_right: detailed.added.clone(),
            changed: detailed
                .changed
                .iter()
                .map(|row| sqlly_services::datacompare::ChangedPair {
                    key: row.key.clone(),
                    left: row.before.clone(),
                    right: row.after.clone(),
                    changed_columns: row.changes.iter().map(|c| c.column_index).collect(),
                })
                .collect(),
            unchanged: detailed.unchanged_count,
            scanned_left: 0,
            scanned_right: 0,
            truncated: self.truncated.clone().map(Into::into),
        };
        let header = crate::localization::format(
            "results.data_compare.sync_script_header",
            &[
                (
                    "left_profile",
                    self.left
                        .profile
                        .as_ref()
                        .map(|profile| profile.name.as_str())
                        .unwrap_or(""),
                ),
                ("left_database", self.left.db.as_deref().unwrap_or("")),
                (
                    "right_profile",
                    self.right
                        .profile
                        .as_ref()
                        .map(|profile| profile.name.as_str())
                        .unwrap_or(""),
                ),
                ("right_database", self.right.db.as_deref().unwrap_or("")),
            ],
        );
        let script = generate_sync_script(&SyncScriptRequest {
            db: dest_profile.database_type,
            destination_table: &destination_table,
            columns: &self.columns,
            key_indexes: &self.key_indexes,
            diff: &data_diff,
            direction: self.direction,
            header_note: &header,
        });
        dest_profile.database = Some(dest_db);
        cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
            sql: script.sql,
            profile: Box::new(dest_profile),
        });
        cx.emit(ToolDialogEvent::Close);
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if self.phase == ComparePhase::Comparing {
            self.generation += 1;
            let task_ids = self.task_ids.clone();
            let left_services = self
                .left
                .profile
                .as_ref()
                .map(|profile| self.router.services_for(profile));
            let right_services = self
                .right
                .profile
                .as_ref()
                .map(|profile| self.router.services_for(profile));
            // Cancel fires each backend's server-side hook, which may itself
            // need the tokio reactor — run it on the app runtime too.
            spawn_on_app_runtime(cx, async move {
                for task_id in task_ids {
                    if let Some(services) = &left_services {
                        let _ = services.queries.cancel(&task_id).await;
                    }
                    if let Some(services) = &right_services {
                        let _ = services.queries.cancel(&task_id).await;
                    }
                }
            })
            .detach();
            self.task_ids.clear();
        }
        cx.emit(ToolDialogEvent::Close);
    }

    fn set_class_filter(&mut self, filter: Option<&'static str>, cx: &mut gpui::Context<Self>) {
        self.class_filter = filter;
        self.rebuild_grid(cx);
        cx.notify();
    }
}

/// Run a services future on the application's tokio runtime and hand its
/// output back as a gpui [`gpui::Task`] the pane can await.
///
/// The query services spawn their producers with `tokio::spawn`, which panics
/// ("must be called from the context of a Tokio 1.x runtime") on a gpui
/// background thread — that is what killed SQLLY when Compare looked up the
/// primary key. On native the future is `block_on`'d on the runtime from the
/// gpui background thread (the `tool_dialog::run_query` pattern), so the
/// foreground await is always woken from a gpui thread. The browser build has
/// no tokio: its services are reactor-free and run as-is.
fn spawn_on_app_runtime<V: 'static, T: Send + 'static>(
    cx: &mut gpui::Context<V>,
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> gpui::Task<T> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        cx.background_spawn(async move { crate::tokio_handle().block_on(future) })
    }
    #[cfg(target_arch = "wasm32")]
    {
        cx.background_spawn(future)
    }
}

// Cohesive streaming-compare driver; splitting the args would not clarify it.
// Native only: the merge-join loop is driven by `tokio::select!`, whose macro
// is not built for the wasm tokio. The browser preview cannot run a compare.
#[cfg(not(target_arch = "wasm32"))]
#[allow(clippy::too_many_arguments)]
async fn run_streaming_compare(
    left_services: sqlly_services::Services,
    left_profile: ConnectionProfile,
    left_sql: String,
    right_services: sqlly_services::Services,
    right_profile: ConnectionProfile,
    right_sql: String,
    key_names: Vec<String>,
    task_tx: tokio::sync::oneshot::Sender<(String, String)>,
) -> Result<(Vec<ResultColumn>, Vec<usize>, DataDiff), String> {
    let mut left_handle = left_services
        .queries
        .execute(&left_profile, &left_sql, 256)
        .await
        .map_err(|error| error.to_string())?;
    let mut right_handle = right_services
        .queries
        .execute(&right_profile, &right_sql, 256)
        .await
        .map_err(|error| error.to_string())?;
    let _ = task_tx.send((left_handle.task_id.clone(), right_handle.task_id.clone()));
    let left_task = left_handle.task_id.clone();
    let right_task = right_handle.task_id.clone();

    let mut joiner: Option<MergeJoiner> = None;
    let mut left_columns = Vec::new();
    let mut right_columns = Vec::new();
    let mut left_done = false;
    let mut right_done = false;

    let finish = |joiner: MergeJoiner,
                  left_columns: Vec<ResultColumn>,
                  key_names: &[String]|
     -> Result<(Vec<ResultColumn>, Vec<usize>, DataDiff), String> {
        let key_indexes = key_indexes(&left_columns, key_names).map_err(compare_error_message)?;
        Ok((left_columns, key_indexes, joiner.into_diff()))
    };

    loop {
        if let Some(active) = joiner.as_mut()
            && !active.drain()
        {
            let _ = left_services.queries.cancel(&left_task).await;
            let _ = right_services.queries.cancel(&right_task).await;
            let joiner = joiner.take().expect("joiner present");
            return finish(joiner, left_columns, &key_names);
        }

        if left_done && right_done {
            break;
        }

        // A side whose columns have arrived is not read again until the
        // other side's have too: the joiner is built from both, and rows (or
        // a Completed) consumed before it exists would be lost — the compare
        // then waited forever for a side it had already finished. Unread
        // rows wait in that side's bounded channel (backpressure).
        let left_ready = joiner.is_some() || left_columns.is_empty();
        let right_ready = joiner.is_some() || right_columns.is_empty();
        tokio::select! {
            left_event = left_handle.events.recv(), if !left_done && left_ready => {
                match left_event {
                    Some(QueryEvent::ResultSet { index: 0, columns }) => {
                        left_columns = columns;
                        if !right_columns.is_empty() && joiner.is_none() {
                            joiner = Some(MergeJoiner::new(&left_columns, &right_columns, &key_names).map_err(compare_error_message)?);
                        }
                    }
                    Some(QueryEvent::Rows { index: 0, rows }) => {
                        if let Some(joiner) = joiner.as_mut() {
                            joiner.push_left(rows).map_err(compare_error_message)?;
                        }
                    }
                    Some(QueryEvent::Error { message }) => {
                        let _ = left_services.queries.cancel(&left_task).await;
                        let _ = right_services.queries.cancel(&right_task).await;
                        return Err(message);
                    }
                    Some(QueryEvent::Completed { .. }) | Some(QueryEvent::Cancelled) | None => {
                        if let Some(joiner) = joiner.as_mut() {
                            joiner.left_finished();
                        }
                        left_done = true;
                    }
                    _ => {}
                }
            }
            right_event = right_handle.events.recv(), if !right_done && right_ready => {
                match right_event {
                    Some(QueryEvent::ResultSet { index: 0, columns }) => {
                        right_columns = columns;
                        if !left_columns.is_empty() && joiner.is_none() {
                            joiner = Some(MergeJoiner::new(&left_columns, &right_columns, &key_names).map_err(compare_error_message)?);
                        }
                    }
                    Some(QueryEvent::Rows { index: 0, rows }) => {
                        if let Some(joiner) = joiner.as_mut() {
                            joiner.push_right(rows).map_err(compare_error_message)?;
                        }
                    }
                    Some(QueryEvent::Error { message }) => {
                        let _ = left_services.queries.cancel(&left_task).await;
                        let _ = right_services.queries.cancel(&right_task).await;
                        return Err(message);
                    }
                    Some(QueryEvent::Completed { .. }) | Some(QueryEvent::Cancelled) | None => {
                        if let Some(joiner) = joiner.as_mut() {
                            joiner.right_finished();
                        }
                        right_done = true;
                    }
                    _ => {}
                }
            }
            else => break,
        }
    }

    if let Some(mut joiner) = joiner {
        while joiner.drain() {}
        finish(joiner, left_columns, &key_names)
    } else {
        Err(crate::localization::text(
            "results.data_compare.missing_column_metadata",
        ))
    }
}

impl gpui::Focusable for DataComparePane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DataComparePane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        if self.selects_dirty {
            self.selects_dirty = false;
            let profile_items = self.profiles.iter().map(profile_label).collect::<Vec<_>>();
            for (side, state) in [(DataSide::Left, &self.left), (DataSide::Right, &self.right)] {
                let profile_index = state
                    .profile
                    .as_ref()
                    .and_then(|selected| {
                        self.profiles
                            .iter()
                            .position(|profile| profile.id == selected.id)
                    })
                    .map(gpui_component::IndexPath::new);
                let database_index = state
                    .db
                    .as_ref()
                    .and_then(|database| {
                        state
                            .databases
                            .iter()
                            .position(|candidate| candidate == database)
                    })
                    .map(gpui_component::IndexPath::new);
                state.profile_select.update(cx, |select, cx| {
                    select.set_items(profile_items.clone(), window, cx);
                    if let Some(index) = profile_index {
                        select.set_selected_index(Some(index), window, cx);
                    }
                });
                state.database_select.update(cx, |select, cx| {
                    select.set_items(state.databases.clone(), window, cx);
                    if let Some(index) = database_index {
                        select.set_selected_index(Some(index), window, cx);
                    }
                });
                let _ = side;
            }
            self.direction_select.update(cx, |select, cx| {
                select.set_items(direction_items(), window, cx);
                let index = DIRECTION_CHOICES
                    .iter()
                    .position(|direction| *direction == self.direction)
                    .unwrap_or(0);
                select.set_selected_index(Some(gpui_component::IndexPath::new(index)), window, cx);
            });
        }

        let busy = matches!(
            self.phase,
            ComparePhase::LoadingKeys | ComparePhase::Comparing
        );
        let summary = self.diff.as_ref().map(|diff| {
            let truncated = self
                .truncated
                .as_ref()
                .map(|note| {
                    crate::localization::format(
                        "results.data_compare.truncated_suffix",
                        &[("note", note)],
                    )
                })
                .unwrap_or_default();
            crate::localization::format(
                "results.data_compare.diff_summary",
                &[
                    ("left_only", &diff.removed.len().to_string()),
                    ("right_only", &diff.added.len().to_string()),
                    ("changed", &diff.changed.len().to_string()),
                    ("unchanged", &diff.unchanged_count.to_string()),
                    ("truncated_suffix", &truncated),
                ],
            )
        });

        let content = if let Some(grid) = self.grid.clone() {
            div().size_full().child(grid)
        } else if let Some(error) = self.error.clone() {
            div().p(px(s(12.0))).text_color(Theme::error()).child(error)
        } else {
            div()
                .p(px(s(12.0)))
                .text_color(Theme::text_dim())
                .child(crate::localization::text(
                    "results.data_compare.empty_instructions",
                ))
        };

        let script_enabled = self.diff.is_some()
            && match self.direction {
                SyncDirection::LeftToRight => !self.right.use_query,
                SyncDirection::RightToLeft => !self.left.use_query,
            };

        // Bare body: this dialog embeds the diff grid, so it lays out its own
        // sections instead of the chrome's scrollable padded body.
        let controls = div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .px(px(s(14.0)))
            .pt(px(s(12.0)))
            .children(self.render_side_column(DataSide::Left, "Left", window, cx))
            .children(self.render_side_column(DataSide::Right, "Right", window, cx))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(div().flex_1().child(Input::new(&self.key_input)))
                    .child(
                        div()
                            .w(px(s(180.0)))
                            .child(Select::new(&self.direction_select)),
                    ),
            )
            .child(self.render_class_filters(cx));

        let focus_handle = self.focus_handle.clone();
        Dialog::new("data-compare-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text(
                "results.data_compare.dialog_title",
            ))
            .key_context("DataComparePane")
            .track_focus(&focus_handle)
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(controls)
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .mx(px(s(14.0)))
                    .mt(px(s(8.0)))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(content),
            )
            .child(
                div()
                    .px(px(s(14.0)))
                    .py(px(s(6.0)))
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(summary.unwrap_or_else(|| {
                        crate::localization::text("results.data_compare.no_comparison")
                    })),
            )
            .leading_button(
                Button::new("dc-script")
                    .primary()
                    .label(crate::localization::text(
                        "results.data_compare.script_sync_button",
                    ))
                    .disabled(!script_enabled)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.script_to_editor(cx);
                    })),
            )
            .button(
                Button::new("dc-compare")
                    .primary()
                    .label(crate::localization::text(
                        "results.data_compare.compare_button",
                    ))
                    .disabled(busy || self.left.databases_loading || self.right.databases_loading)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.compare(cx);
                    })),
            )
            .build(self, window, cx)
    }
}

impl DataComparePane {
    fn render_side_column(
        &self,
        side: DataSide,
        label: &'static str,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Vec<gpui::AnyElement> {
        let s = Theme::scaled;
        let state = self.side_ref(side);
        let use_query = state.use_query;
        let object_input = Textarea::new(&state.object_input);
        let (side_label, query_label) = match side {
            DataSide::Left => (
                crate::localization::text("results.data_compare.left_side_label"),
                crate::localization::text("results.data_compare.left_use_query"),
            ),
            DataSide::Right => (
                crate::localization::text("results.data_compare.right_side_label"),
                crate::localization::text("results.data_compare.right_use_query"),
            ),
        };
        vec![
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(52.0)))
                        .text_color(Theme::text_dim())
                        .child(side_label),
                )
                .child(
                    div()
                        .w(px(s(300.0)))
                        .child(Select::new(&state.profile_select).w_full()),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Select::new(&state.database_select).w_full()),
                )
                .child(
                    Checkbox::new(format!("dc-query-{label}"))
                        .label(query_label)
                        .checked(use_query)
                        .on_click(cx.listener(move |this, _checked: &bool, window, cx| {
                            let state = this.side_mut(side);
                            state.use_query = !state.use_query;
                            let placeholder = if state.use_query {
                                "SELECT ..."
                            } else {
                                "schema.table"
                            };
                            state.object_input.update(cx, |input, cx| {
                                input.set_placeholder(placeholder, window, cx);
                            });
                            this.invalidate_comparison();
                            cx.notify();
                        })),
                )
                .into_any_element(),
            div()
                .flex()
                .flex_row()
                .pl(px(Theme::scaled(60.0)))
                .child(div().flex_1().child(object_input))
                .into_any_element(),
        ]
    }

    fn render_class_filters(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mk = |id: &'static str, label: String, filter: Option<&'static str>| {
            Button::new(id)
                .small()
                .label(label)
                .selected(self.class_filter == filter)
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.set_class_filter(filter, cx);
                }))
        };
        div()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(mk(
                "dc-filter-all",
                crate::localization::text("results.data_compare.filter_all"),
                None,
            ))
            .child(mk(
                "dc-filter-left",
                crate::localization::text("results.data_compare.filter_only_left"),
                Some("Removed"),
            ))
            .child(mk(
                "dc-filter-right",
                crate::localization::text("results.data_compare.filter_only_right"),
                Some("Added"),
            ))
            .child(mk(
                "dc-filter-changed",
                crate::localization::text("results.data_compare.filter_changed"),
                Some("Changed"),
            ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::metadata::PrimaryKeyRow;

    #[test]
    fn direction_dispatches_on_row() {
        assert_eq!(direction_from_row(Some(0)), SyncDirection::LeftToRight);
        assert_eq!(direction_from_row(Some(1)), SyncDirection::RightToLeft);
        assert_eq!(direction_from_row(None), SyncDirection::LeftToRight);
        assert_eq!(direction_items().len(), DIRECTION_CHOICES.len());
    }

    #[test]
    fn source_sql_for_table_sql_server() {
        let sql = source_sql_for_table(DatabaseType::SqlServer, "dbo.t", &["id".into()]);
        assert_eq!(sql, "SELECT * FROM [dbo].[t] ORDER BY [id]");
    }

    #[test]
    fn source_sql_for_table_postgres() {
        let sql = source_sql_for_table(DatabaseType::Postgres, "public.t", &["id".into()]);
        assert_eq!(sql, "SELECT * FROM \"public\".\"t\" ORDER BY \"id\"");
    }

    #[test]
    fn source_sql_for_query_strips_trailing_semicolon() {
        let sql = source_sql_for_query("SELECT 1;", DatabaseType::Postgres, &["id".into()]);
        assert_eq!(sql, "SELECT * FROM (SELECT 1) AS sqlly_cmp ORDER BY \"id\"");
    }

    /// Oracle rejects `AS` before a table alias; the derived table keeps
    /// its alias without it, and tables quote with double quotes.
    #[test]
    fn source_sql_for_oracle_omits_as_before_the_alias() {
        let sql = source_sql_for_query(
            "SELECT id FROM sqlly_customers;",
            DatabaseType::Oracle,
            &["ID".into()],
        );
        assert_eq!(
            sql,
            "SELECT * FROM (SELECT id FROM sqlly_customers) sqlly_cmp ORDER BY \"ID\""
        );
        let sql = source_sql_for_table(DatabaseType::Oracle, "SQLLY.T", &["ID".into()]);
        assert_eq!(sql, "SELECT * FROM \"SQLLY\".\"T\" ORDER BY \"ID\"");
    }

    #[test]
    fn parse_key_input_trims_and_splits() {
        assert_eq!(
            parse_key_input("a, B ,c"),
            vec!["a".to_string(), "B".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn pk_columns_for_matches_case_insensitive() {
        let rows = vec![
            PrimaryKeyRow {
                schema: "dbo".into(),
                table: "T".into(),
                column: "b".into(),
                key_ordinal: 2,
            },
            PrimaryKeyRow {
                schema: "dbo".into(),
                table: "T".into(),
                column: "a".into(),
                key_ordinal: 1,
            },
        ];
        assert_eq!(pk_columns_for(&rows, "DBO", "t"), vec!["a", "b"]);
    }

    #[test]
    fn unqualified_tables_find_their_key_without_a_default_schema() {
        let pk = |schema: &str, table: &str, column: &str| PrimaryKeyRow {
            schema: schema.into(),
            table: table.into(),
            column: column.into(),
            key_ordinal: 1,
        };
        // SQLite has no default schema but its catalog reports `main`.
        let sqlite = vec![pk("main", "t", "id")];
        assert_eq!(
            pk_columns_for_table_input(&sqlite, DatabaseType::Sqlite, "t"),
            vec!["id"]
        );
        // The same name in two MySQL databases is ambiguous: no guess.
        let mysql = vec![pk("shop", "t", "id"), pk("archive", "t", "code")];
        assert!(pk_columns_for_table_input(&mysql, DatabaseType::MySql, "t").is_empty());
        assert_eq!(
            pk_columns_for_table_input(&mysql, DatabaseType::MySql, "archive.t"),
            vec!["code"]
        );
        // With a default schema, an unqualified name means that schema only.
        let tds = vec![pk("sales", "t", "id")];
        assert!(pk_columns_for_table_input(&tds, DatabaseType::SqlServer, "t").is_empty());
    }

    #[test]
    fn to_detailed_diff_maps_sides() {
        let columns = vec![ResultColumn {
            name: "id".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let diff = DataDiff {
            only_left: vec![vec![CellValue::I64(1)]],
            only_right: vec![vec![CellValue::I64(2)]],
            ..DataDiff::default()
        };
        let detailed = to_detailed_diff(diff, &columns, &[0]);
        assert_eq!(detailed.removed.len(), 1);
        assert_eq!(detailed.added.len(), 1);
    }

    // -- windowed compare tests ------------------------------------------------

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    /// A SQLite file with one keyed table, seeded with `rows`.
    fn sqlite_side(dir: &std::path::Path, file: &str, rows: &[(i64, &str)]) -> ConnectionProfile {
        let path = dir.join(file);
        let conn = rusqlite::Connection::open(&path).expect("open sqlite");
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT NOT NULL);")
            .expect("create table");
        for (id, name) in rows {
            conn.execute(
                "INSERT INTO t (id, name) VALUES (?1, ?2)",
                rusqlite::params![id, name],
            )
            .expect("insert");
        }
        drop(conn);
        let mut profile = ConnectionProfile::sql_login(
            file,
            path.to_string_lossy().to_string(),
            "",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::Sqlite;
        profile
    }

    /// Open the pane over two SQLite files (left: 1 a, 2 b, 3 c; right:
    /// 1 a, 2 B, 4 d), type `t` on both sides and `keys` into the key box,
    /// press Compare, and wait for the outcome. The services are the real
    /// native ones, so the primary-key lookup and both streaming queries run
    /// their tokio producers exactly as in the app.
    fn run_compare(
        cx: &mut gpui::TestAppContext,
        keys: &str,
    ) -> (ComparePhase, Option<String>, Option<DetailedDiff>) {
        crate::ensure_test_tokio_runtime();
        ensure_components_initialized(cx);
        cx.dispatcher.allow_parking();
        let dir = tempfile::tempdir().expect("tempdir");
        let left = sqlite_side(dir.path(), "left.db", &[(1, "a"), (2, "b"), (3, "c")]);
        let right = sqlite_side(dir.path(), "right.db", &[(1, "a"), (2, "B"), (4, "d")]);
        let services = sqlly_services::Services::new();
        let context = ToolDialogContext::new(left.clone(), services.clone());
        let router = ServiceRouter::new(services);
        let profiles = vec![left, right];
        let (view, cx) = cx.add_window_view(|window, cx| {
            DataComparePane::with_profiles(context, router, profiles, window, cx)
        });
        cx.run_until_parked();
        let keys = keys.to_string();
        view.update_in(cx, |pane, window, cx| {
            assert!(pane.left.db.is_some() && pane.right.db.is_some());
            for input in [
                pane.left.object_input.clone(),
                pane.right.object_input.clone(),
            ] {
                input.update(cx, |input, cx| input.set_value("t", window, cx));
            }
            pane.key_input
                .update(cx, |input, cx| input.set_value(keys.clone(), window, cx));
            pane.compare(cx);
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        loop {
            cx.run_until_parked();
            let settled = view.read_with(cx, |pane, _| {
                matches!(pane.phase, ComparePhase::Done | ComparePhase::Idle)
            });
            if settled || std::time::Instant::now() > deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        view.read_with(cx, |pane, _| {
            (pane.phase, pane.error.clone(), pane.diff.clone())
        })
    }

    fn assert_expected_diff(outcome: (ComparePhase, Option<String>, Option<DetailedDiff>)) {
        let (phase, error, diff) = outcome;
        assert_eq!(
            phase,
            ComparePhase::Done,
            "compare did not finish: {error:?}"
        );
        let diff = diff.expect("a diff after Done");
        assert_eq!(
            diff.removed,
            vec![vec![CellValue::I64(3), CellValue::String("c".into())]]
        );
        assert_eq!(
            diff.added,
            vec![vec![CellValue::I64(4), CellValue::String("d".into())]]
        );
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].key, vec![CellValue::I64(2)]);
        assert_eq!(diff.unchanged_count, 1);
    }

    /// Regression: with the key box blank, Compare looks up the primary key
    /// first. That lookup ran on a bare gpui background thread, where the
    /// query service's `tokio::spawn` panicked and took SQLLY down.
    #[gpui::test]
    fn compare_with_primary_key_lookup_runs_on_the_app_runtime(cx: &mut gpui::TestAppContext) {
        assert_expected_diff(run_compare(cx, ""));
    }

    /// Regression: with explicit keys, the compare future was built but the
    /// task awaited its task ids before ever polling it, so the ids never
    /// arrived and the pane sat in "Comparing" forever.
    #[gpui::test]
    fn compare_with_explicit_keys_finishes(cx: &mut gpui::TestAppContext) {
        assert_expected_diff(run_compare(cx, "id"));
    }
}
