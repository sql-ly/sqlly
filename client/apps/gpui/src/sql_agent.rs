//! SQL Agent browser — jobs, history, schedules, operators, and alerts from
//! `msdb`, in real grids, with SCRIPT-FIRST job control.
//!
//! Job actions (start, stop, enable, disable) never execute from the pane:
//! each puts the reviewed `sp_start_job` / `sp_stop_job` / `sp_update_job`
//! statement into an editor tab, per the script-first contract — running it
//! (and holding the msdb role it needs) stays the user's explicit act.
//! Capability is detected up front: Azure SQL Database has no Agent, and
//! the pane says so instead of showing empty grids.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{InputEvent, InputState, TextareaState};

use crate::field_input::FieldInput;
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;
use sqlly_services::{
    AdminToolsService, AlertDefinition, JobDefinition, JobStepDefinition, OperatorDefinition,
    ScheduleDefinition, describe_schedule, next_run_after,
};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AgentTab {
    Jobs,
    History,
    Schedules,
    Operators,
    Alerts,
}

impl AgentTab {
    pub fn all() -> [AgentTab; 5] {
        [
            AgentTab::Jobs,
            AgentTab::History,
            AgentTab::Schedules,
            AgentTab::Operators,
            AgentTab::Alerts,
        ]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            AgentTab::Jobs => "explorer.sql_agent.tab_jobs",
            AgentTab::History => "explorer.sql_agent.tab_history",
            AgentTab::Schedules => "explorer.sql_agent.tab_schedules",
            AgentTab::Operators => "explorer.sql_agent.tab_operators",
            AgentTab::Alerts => "explorer.sql_agent.tab_alerts",
        })
    }

    pub fn query(self, admin: &dyn AdminToolsService) -> String {
        match self {
            AgentTab::Jobs => admin.agent_list_jobs_query(),
            AgentTab::History => admin.agent_history_query(None),
            AgentTab::Schedules => admin.agent_list_schedules_query(),
            AgentTab::Operators => admin.agent_list_operators_query(),
            AgentTab::Alerts => admin.agent_list_alerts_query(),
        }
    }
}

/// What the capability probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Capability {
    Checking,
    Available,
    /// msdb.dbo.sysjobs does not exist (Azure SQL Database) or the probe
    /// failed; the string is the honest explanation.
    Unavailable(String),
}

impl EventEmitter<ToolDialogEvent> for SqlAgentPane {}

pub struct SqlAgentPane {
    context: ToolDialogContext,
    capability: Capability,
    tab: AgentTab,
    grid: Option<Entity<SqllyDataTable>>,
    /// The loaded rows behind the grid, kept so the Jobs tab's script
    /// actions can read the selected job's name and enabled flag.
    rows: Vec<Vec<CellValue>>,
    row_count: usize,
    error: Option<String>,
    loading: bool,
    refresh_generation: u64,
    editor: Option<AgentEditor>,
    editor_inputs: [FieldInput; 8],
    editor_values: [String; 8],
    editor_enabled: bool,
    _editor_subscriptions: Vec<gpui::Subscription>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EditorMode {
    Create,
    Edit,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentEditor {
    tab: AgentTab,
    mode: EditorMode,
    original_name: Option<String>,
    record_id: Option<i32>,
}

impl AgentEditor {
    fn title(&self) -> String {
        let mode = crate::localization::text(match self.mode {
            EditorMode::Create => "explorer.sql_agent.mode_create",
            EditorMode::Edit => "explorer.sql_agent.mode_edit",
        });
        let subject = crate::localization::text(match self.tab {
            AgentTab::Jobs => "explorer.sql_agent.editor_job",
            AgentTab::Schedules => "explorer.sql_agent.editor_schedule",
            AgentTab::Alerts => "explorer.sql_agent.editor_alert",
            AgentTab::Operators => "explorer.sql_agent.editor_operator",
            AgentTab::History => "explorer.sql_agent.editor_item",
        });
        crate::localization::format(
            "explorer.sql_agent.editor_title",
            &[("mode", &mode), ("subject", &subject)],
        )
    }
}

const JOB_FIELDS: [&str; 8] = [
    "explorer.sql_agent.field_job_name",
    "explorer.sql_agent.field_description",
    "explorer.sql_agent.field_owner_login",
    "explorer.sql_agent.field_step_id",
    "explorer.sql_agent.field_step_name",
    "explorer.sql_agent.field_subsystem",
    "explorer.sql_agent.field_step_database",
    "explorer.sql_agent.field_step_command",
];
const SCHEDULE_FIELDS: [&str; 5] = [
    "explorer.sql_agent.field_schedule_name",
    "explorer.sql_agent.field_frequency_type",
    "explorer.sql_agent.field_frequency_interval",
    "explorer.sql_agent.field_start_time",
    "explorer.sql_agent.field_attach_job",
];
const ALERT_FIELDS: [&str; 5] = [
    "explorer.sql_agent.field_alert_name",
    "explorer.sql_agent.field_message_id",
    "explorer.sql_agent.field_severity",
    "explorer.sql_agent.field_delay_seconds",
    "explorer.sql_agent.field_notify_operator",
];
const OPERATOR_FIELDS: [&str; 2] = [
    "explorer.sql_agent.field_operator_name",
    "explorer.sql_agent.field_email",
];

/// What a Jobs-tab script action produces. Script-first: each variant maps
/// to reviewed SQL, never to direct execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobScriptAction {
    Start,
    Stop,
    Enable,
    Disable,
}

impl JobScriptAction {
    fn label(self) -> String {
        crate::localization::text(match self {
            JobScriptAction::Start => "explorer.sql_agent.script_start",
            JobScriptAction::Stop => "explorer.sql_agent.script_stop",
            JobScriptAction::Enable => "explorer.sql_agent.script_enable",
            JobScriptAction::Disable => "explorer.sql_agent.script_disable",
        })
    }

    fn sql_for(self, admin: &dyn AdminToolsService, job_name: &str) -> String {
        let statement = match self {
            JobScriptAction::Start => admin.agent_start_job_sql(job_name),
            JobScriptAction::Stop => admin.agent_stop_job_sql(job_name),
            JobScriptAction::Enable => admin.agent_set_job_enabled_sql(job_name, true),
            JobScriptAction::Disable => admin.agent_set_job_enabled_sql(job_name, false),
        };
        format!("-- Review, then run. Requires an msdb SQLAgent role (or sysadmin).\n{statement}")
    }
}

/// The (name, enabled) pair from one Jobs-tab row, tolerant of the driver's
/// bool-vs-integer representation of `enabled`. Column order comes from
/// Append "Recurrence" and "Next run" columns to the Schedules grid, computed
/// locally from the sysschedules frequency encoding — no server round trip.
/// Row shape (list_schedules_query): schedule_id, name, enabled, freq_type,
/// freq_interval, active_start_date, active_end_date, active_start_time.
fn append_schedule_readability(set: &mut crate::tool_dialog::ToolQuerySet) {
    set.columns.push(sqlly_core::ResultColumn {
        name: "recurrence".into(),
        data_type: "nvarchar".into(),
        nullable: true,
    });
    set.columns.push(sqlly_core::ResultColumn {
        name: "next_run".into(),
        data_type: "nvarchar".into(),
        nullable: true,
    });
    let now = chrono::Local::now().naive_local();
    for row in &mut set.rows {
        let freq_type = cell_i32(row.get(3));
        let freq_interval = cell_i32(row.get(4)).unwrap_or(0);
        let start_time = cell_i32(row.get(7)).unwrap_or(0);
        match freq_type {
            Some(freq_type) => {
                row.push(CellValue::String(describe_schedule(
                    freq_type,
                    freq_interval,
                    start_time,
                )));
                let next = next_run_after(freq_type, freq_interval, start_time, now);
                row.push(match next {
                    Some(when) => CellValue::String(when.format("%Y-%m-%d %H:%M:%S").to_string()),
                    None => CellValue::Null,
                });
            }
            None => {
                row.push(CellValue::Null);
                row.push(CellValue::Null);
            }
        }
    }
}

/// Append a human "condition" column to the Alerts grid via the typed alert
/// parser. Row shape (list_alerts_query): id, name, enabled, message_id,
/// severity, delay_between_responses.
fn append_alert_condition(
    admin: &dyn AdminToolsService,
    set: &mut crate::tool_dialog::ToolQuerySet,
) {
    set.columns.push(sqlly_core::ResultColumn {
        name: "condition".into(),
        data_type: "nvarchar".into(),
        nullable: true,
    });
    for row in &mut set.rows {
        let name = match row.get(1) {
            Some(CellValue::String(s)) => Some(s.clone()),
            _ => None,
        };
        let parsed = admin.agent_parse_alert_row(
            cell_i32(row.first()).map(i64::from),
            name.as_deref(),
            cell_i32(row.get(2)).map(i64::from),
            cell_i32(row.get(3)).map(i64::from),
            cell_i32(row.get(4)).map(i64::from),
            cell_i32(row.get(5)).map(i64::from),
        );
        row.push(match parsed {
            Some(alert) => CellValue::String(alert.condition),
            None => CellValue::Null,
        });
    }
}

/// `JobBrowser::list_jobs_query`: job_id, name, enabled, ….
fn job_from_row(row: &[CellValue]) -> Option<(String, bool)> {
    let name = match row.get(1)? {
        CellValue::String(s) if !s.trim().is_empty() => s.clone(),
        _ => return None,
    };
    let enabled = match row.get(2) {
        Some(CellValue::Bool(b)) => *b,
        Some(CellValue::I64(n)) => *n != 0,
        Some(CellValue::String(s)) => s == "1" || s.eq_ignore_ascii_case("true"),
        _ => false,
    };
    Some((name, enabled))
}

fn cell_string(value: Option<&CellValue>) -> String {
    match value {
        Some(CellValue::String(value)) => value.clone(),
        Some(CellValue::I64(value)) => value.to_string(),
        Some(CellValue::F64(value)) => value.to_string(),
        Some(CellValue::Bool(value)) => i32::from(*value).to_string(),
        _ => String::new(),
    }
}

fn cell_i32(value: Option<&CellValue>) -> Option<i32> {
    match value? {
        CellValue::I64(value) => i32::try_from(*value).ok(),
        CellValue::F64(value) if value.is_finite() => Some(*value as i32),
        CellValue::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

fn cell_bool(value: Option<&CellValue>) -> bool {
    matches!(value, Some(CellValue::Bool(true)) | Some(CellValue::I64(1)))
        || matches!(value, Some(CellValue::String(value)) if value == "1" || value.eq_ignore_ascii_case("true"))
}

fn parse_i32_field(value: &str, label: &str) -> Result<i32, String> {
    value.trim().parse::<i32>().map_err(|_| {
        crate::localization::format(
            "explorer.sql_agent.whole_number_required",
            &[("label", &crate::localization::text(label))],
        )
    })
}

fn valid_agent_time(value: i32) -> bool {
    (0..=23).contains(&(value / 10_000))
        && (0..=59).contains(&((value / 100) % 100))
        && (0..=59).contains(&(value % 100))
}

fn editor_script(
    admin: &dyn AdminToolsService,
    editor: &AgentEditor,
    values: &[String; 8],
    enabled: bool,
) -> Result<String, String> {
    let body = match editor.tab {
        AgentTab::Jobs => {
            let name = values[0].trim();
            if name.is_empty() {
                return Err(crate::localization::text(
                    "explorer.sql_agent.job_name_required",
                ));
            }
            let job = JobDefinition {
                name: name.into(),
                enabled,
                description: values[1].trim().into(),
                owner_login: (!values[2].trim().is_empty()).then(|| values[2].trim().into()),
            };
            let has_step = !values[4].trim().is_empty() || !values[7].trim().is_empty();
            if has_step && (values[4].trim().is_empty() || values[7].trim().is_empty()) {
                return Err(crate::localization::text(
                    "explorer.sql_agent.step_name_command_required",
                ));
            }
            let step = has_step.then(|| JobStepDefinition {
                step_name: values[4].trim().into(),
                subsystem: if values[5].trim().is_empty() {
                    "TSQL".into()
                } else {
                    values[5].trim().into()
                },
                command: values[7].clone(),
                database_name: (!values[6].trim().is_empty()).then(|| values[6].trim().into()),
            });
            match editor.mode {
                EditorMode::Create => admin.agent_create_job_full_sql(&job, step.as_slice()),
                EditorMode::Edit => {
                    let original = editor.original_name.as_deref().unwrap_or(name);
                    let mut sql = admin.agent_update_job_sql(original, &job);
                    if let Some(step) = step {
                        let step_id = if values[3].trim().is_empty() {
                            None
                        } else {
                            Some(
                                parse_i32_field(&values[3], "explorer.sql_agent.field_step_id")?
                                    .max(1),
                            )
                        };
                        sql.push('\n');
                        sql.push_str(&admin.agent_upsert_job_step_sql(name, step_id, &step));
                    }
                    sql
                }
            }
        }
        AgentTab::Schedules => {
            let name = values[0].trim();
            if name.is_empty() {
                return Err(crate::localization::text(
                    "explorer.sql_agent.schedule_name_required",
                ));
            }
            let freq_type = parse_i32_field(&values[1], "explorer.sql_agent.field_frequency_type")?;
            if ![1, 4, 8, 16, 32, 64, 128].contains(&freq_type) {
                return Err(crate::localization::text(
                    "explorer.sql_agent.frequency_type_invalid",
                ));
            }
            let freq_interval =
                parse_i32_field(&values[2], "explorer.sql_agent.field_frequency_interval")?.max(1);
            let active_start_time =
                parse_i32_field(&values[3], "explorer.sql_agent.field_start_time")?;
            if !valid_agent_time(active_start_time) {
                return Err(crate::localization::text(
                    "explorer.sql_agent.start_time_invalid",
                ));
            }
            let definition = ScheduleDefinition {
                name: name.into(),
                enabled,
                freq_type,
                freq_interval,
                active_start_time,
            };
            let mut sql = match editor.mode {
                EditorMode::Create => admin.agent_create_schedule_definition_sql(&definition),
                EditorMode::Edit => admin.agent_update_schedule_sql(
                    editor.record_id.ok_or_else(|| {
                        crate::localization::text("explorer.sql_agent.select_schedule_edit")
                    })?,
                    &definition,
                ),
            };
            if !values[4].trim().is_empty() {
                sql.push('\n');
                sql.push_str(&admin.agent_attach_schedule_sql(values[4].trim(), name));
                sql.push(';');
            }
            sql
        }
        AgentTab::Alerts => {
            let name = values[0].trim();
            if name.is_empty() {
                return Err(crate::localization::text(
                    "explorer.sql_agent.alert_name_required",
                ));
            }
            let message_id =
                parse_i32_field(&values[1], "explorer.sql_agent.field_message_id")?.max(0);
            let severity = parse_i32_field(&values[2], "explorer.sql_agent.field_severity")?;
            if !(0..=25).contains(&severity) {
                return Err(crate::localization::text(
                    "explorer.sql_agent.severity_invalid",
                ));
            }
            if (message_id > 0) == (severity > 0) {
                return Err(crate::localization::text(
                    "explorer.sql_agent.message_or_severity",
                ));
            }
            let alert = AlertDefinition {
                name: name.into(),
                enabled,
                message_id,
                severity,
                delay_between_responses: parse_i32_field(
                    &values[3],
                    "explorer.sql_agent.field_delay_seconds",
                )?
                .max(0),
            };
            let mut sql = match editor.mode {
                EditorMode::Create => admin.agent_create_alert_sql(&alert),
                EditorMode::Edit => admin.agent_update_alert_sql(
                    editor.original_name.as_deref().unwrap_or(name),
                    &alert,
                ),
            };
            if !values[4].trim().is_empty() {
                sql.push('\n');
                sql.push_str(&admin.agent_add_notification_sql(name, values[4].trim()));
            }
            sql
        }
        AgentTab::Operators => {
            let name = values[0].trim();
            if name.is_empty() {
                return Err(crate::localization::text(
                    "explorer.sql_agent.operator_name_required",
                ));
            }
            let operator = OperatorDefinition {
                name: name.into(),
                enabled,
                email_address: (!values[1].trim().is_empty()).then(|| values[1].trim().into()),
            };
            match editor.mode {
                EditorMode::Create => admin.agent_create_operator_sql(&operator),
                EditorMode::Edit => admin.agent_update_operator_sql(
                    editor.original_name.as_deref().unwrap_or(name),
                    &operator,
                ),
            }
        }
        AgentTab::History => {
            return Err(crate::localization::text("explorer.sql_agent.no_editor"));
        }
    };
    Ok(format!(
        "-- Review this SQL Agent change before running it. Requires an appropriate msdb SQLAgent role (or sysadmin).\n{body}"
    ))
}

/// The reviewed deletion script for one selected row of a tab. Pure so the
/// per-tab wiring is testable; the SQL always carries a warning header and,
/// per the script-first contract, never runs from the pane.
fn delete_script(
    admin: &dyn AdminToolsService,
    tab: AgentTab,
    row: &[CellValue],
) -> Result<String, String> {
    let body = match tab {
        AgentTab::Jobs => {
            let (name, _) = job_from_row(row)
                .ok_or_else(|| crate::localization::text("explorer.sql_agent.select_job"))?;
            admin.agent_delete_job_sql(&name)
        }
        AgentTab::Schedules => {
            let id = cell_i32(row.first())
                .ok_or_else(|| crate::localization::text("explorer.sql_agent.select_schedule"))?;
            admin.agent_delete_schedule_sql(id)
        }
        AgentTab::Alerts => {
            let name = cell_string(row.get(1));
            if name.trim().is_empty() {
                return Err(crate::localization::text("explorer.sql_agent.select_alert"));
            }
            admin.agent_delete_alert_sql(name.trim())
        }
        AgentTab::Operators => {
            let name = cell_string(row.get(1));
            if name.trim().is_empty() {
                return Err(crate::localization::text(
                    "explorer.sql_agent.select_operator",
                ));
            }
            admin.agent_delete_operator_sql(name.trim())
        }
        AgentTab::History => {
            return Err(crate::localization::text(
                "explorer.sql_agent.history_delete_unsupported",
            ));
        }
    };
    Ok(format!(
        "-- Review before running: this permanently deletes the selected SQL Agent object.\n-- Requires an appropriate msdb SQLAgent role (or sysadmin).\n{body}"
    ))
}

impl SqlAgentPane {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let editor_inputs: [FieldInput; 8] = std::array::from_fn(|index| {
            if index == 7 {
                FieldInput::Multi(cx.new(|cx| TextareaState::new(window, cx).soft_wrap(true)))
            } else {
                FieldInput::Single(cx.new(|cx| InputState::new(window, cx)))
            }
        });
        let mut editor_subscriptions = Vec::new();
        for (index, input) in editor_inputs.iter().enumerate() {
            editor_subscriptions.push(input.subscribe(
                window,
                cx,
                move |this: &mut Self, event, _window, cx| {
                    if let InputEvent::Change = event {
                        let value = this.editor_inputs[index].value(cx);
                        this.editor_values[index] = value;
                        cx.notify();
                    }
                },
            ));
        }
        let mut this = Self {
            context,
            capability: Capability::Checking,
            tab: AgentTab::Jobs,
            grid: None,
            rows: Vec::new(),
            row_count: 0,
            error: None,
            loading: true,
            refresh_generation: 0,
            editor: None,
            editor_inputs,
            editor_values: std::array::from_fn(|_| String::new()),
            editor_enabled: true,
            _editor_subscriptions: editor_subscriptions,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(900.0), Theme::scaled(540.0)),
        };
        this.check_capability(cx);
        this
    }

    fn check_capability(&mut self, cx: &mut gpui::Context<Self>) {
        self.capability = Capability::Checking;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        crate::tool_dialog::run_query(
            &self.context,
            self.context.services.admin_tools.agent_capability_query(),
            cx,
            move |this, outcome, cx| {
                if this.refresh_generation != generation {
                    return;
                }
                if let Some(err) = outcome.error {
                    this.capability = Capability::Unavailable(err);
                    this.loading = false;
                } else {
                    let available = outcome
                        .sets
                        .first()
                        .and_then(|s| s.rows.first())
                        .and_then(|r| r.first())
                        .map(|c| matches!(c, CellValue::I64(1) | CellValue::Bool(true)))
                        .unwrap_or(false);
                    if available {
                        this.capability = Capability::Available;
                        this.load_tab(cx);
                    } else {
                        this.capability = Capability::Unavailable(crate::localization::text(
                            "explorer.sql_agent.unavailable",
                        ));
                        this.loading = false;
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn switch_tab(&mut self, tab: AgentTab, cx: &mut gpui::Context<Self>) {
        if self.tab != tab {
            self.tab = tab;
            self.editor = None;
            self.grid = None;
            self.rows = Vec::new();
            self.row_count = 0;
            self.error = None;
            if self.capability == Capability::Available {
                self.load_tab(cx);
            }
            cx.notify();
        }
    }

    fn load_tab(&mut self, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.refresh_generation += 1;
        let generation = self.refresh_generation;
        let sql = self.tab.query(&*self.context.services.admin_tools);
        crate::tool_dialog::run_query(&self.context, sql, cx, move |this, outcome, cx| {
            if this.refresh_generation != generation {
                return;
            }
            this.loading = false;
            if let Some(err) = outcome.error {
                this.error = Some(err);
                this.grid = None;
                this.rows = Vec::new();
                this.row_count = 0;
            } else {
                this.error = None;
                let mut set = outcome.sets.into_iter().next().unwrap_or_default();
                // Enrich raw msdb rows with locally computed, human-readable
                // columns. Appended at the END so the editors' fixed value
                // indices stay valid.
                match this.tab {
                    AgentTab::Schedules => append_schedule_readability(&mut set),
                    AgentTab::Alerts => {
                        append_alert_condition(&*this.context.services.admin_tools, &mut set)
                    }
                    _ => {}
                }
                this.row_count = set.rows.len();
                this.grid = crate::tool_dialog::build_result_grid(&set, cx);
                this.rows = set.rows;
            }
            cx.notify();
        });
        cx.notify();
    }

    /// The Jobs tab's selected job (name, enabled), if a row is selected.
    fn selected_job(&self, cx: &gpui::App) -> Option<(String, bool)> {
        if self.tab != AgentTab::Jobs {
            return None;
        }
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        job_from_row(self.rows.get(row)?)
    }

    fn selected_data_row(&self, cx: &gpui::App) -> Option<&[CellValue]> {
        let grid = self.grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        self.rows.get(row).map(Vec::as_slice)
    }

    fn open_editor(&mut self, mode: EditorMode, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if !matches!(
            self.tab,
            AgentTab::Jobs | AgentTab::Schedules | AgentTab::Alerts | AgentTab::Operators
        ) {
            return;
        }
        let selected = (mode == EditorMode::Edit)
            .then(|| self.selected_data_row(cx))
            .flatten();
        if mode == EditorMode::Edit && selected.is_none() {
            cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::format(
                    "explorer.sql_agent.select_row",
                    &[("tab", &self.tab.label().to_lowercase())],
                ),
            });
            return;
        }
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        let mut enabled = true;
        let mut original_name = None;
        let mut record_id = None;
        match (self.tab, selected) {
            (AgentTab::Jobs, Some(row)) => {
                values[0] = cell_string(row.get(1));
                values[1] = cell_string(row.get(3));
                values[2] = cell_string(row.get(5));
                values[5] = "TSQL".into();
                values[6] = self.context.profile.database.clone().unwrap_or_default();
                enabled = cell_bool(row.get(2));
                original_name = Some(values[0].clone());
            }
            (AgentTab::Jobs, None) => {
                values[1] = "Created by SQLLY".into();
                values[4] = "Step 1".into();
                values[5] = "TSQL".into();
                values[6] = self.context.profile.database.clone().unwrap_or_default();
            }
            (AgentTab::Schedules, Some(row)) => {
                record_id = cell_i32(row.first());
                values[0] = cell_string(row.get(1));
                values[1] = cell_string(row.get(3));
                values[2] = cell_string(row.get(4));
                values[3] = cell_string(row.get(7));
                enabled = cell_bool(row.get(2));
                original_name = Some(values[0].clone());
            }
            (AgentTab::Schedules, None) => {
                values[1] = "4".into();
                values[2] = "1".into();
                values[3] = "020000".into();
            }
            (AgentTab::Alerts, Some(row)) => {
                record_id = cell_i32(row.first());
                values[0] = cell_string(row.get(1));
                values[1] = cell_string(row.get(3));
                values[2] = cell_string(row.get(4));
                values[3] = cell_string(row.get(5));
                enabled = cell_bool(row.get(2));
                original_name = Some(values[0].clone());
            }
            (AgentTab::Alerts, None) => {
                values[1] = "0".into();
                values[2] = "16".into();
                values[3] = "0".into();
            }
            (AgentTab::Operators, Some(row)) => {
                record_id = cell_i32(row.first());
                values[0] = cell_string(row.get(1));
                values[1] = cell_string(row.get(3));
                enabled = cell_bool(row.get(2));
                original_name = Some(values[0].clone());
            }
            (AgentTab::Operators, None) => {}
            _ => return,
        }
        self.editor = Some(AgentEditor {
            tab: self.tab,
            mode,
            original_name,
            record_id,
        });
        self.editor_enabled = enabled;
        self.editor_values = values.clone();
        for (input, value) in self.editor_inputs.iter().zip(values) {
            input.set_value(value, window, cx);
        }
        cx.notify();
    }

    fn script_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(editor) = self.editor.as_ref() else {
            return;
        };
        match editor_script(
            &*self.context.services.admin_tools,
            editor,
            &self.editor_values,
            self.editor_enabled,
        ) {
            Ok(sql) => {
                let title = editor.title();
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::format(
                        "explorer.sql_agent.editor_scripted",
                        &[("title", &title)],
                    ),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(message) => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Warning,
                message,
            }),
        }
    }

    fn render_editor(&self, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        let s = Theme::scaled;
        let Some(editor) = self.editor.as_ref() else {
            return div().into_any_element();
        };
        let fields: &[&str] = match editor.tab {
            AgentTab::Jobs => &JOB_FIELDS,
            AgentTab::Schedules => &SCHEDULE_FIELDS,
            AgentTab::Alerts => &ALERT_FIELDS,
            AgentTab::Operators => &OPERATOR_FIELDS,
            AgentTab::History => &[],
        };
        let mut form = div()
            .id("agent-editor-form")
            .size_full()
            .overflow_scroll()
            .p(px(s(10.0)))
            .flex()
            .flex_col()
            .gap(px(s(7.0)))
            .child(
                div()
                    .text_color(Theme::text())
                    .text_size(px(Theme::font_size_ratio(1.02)))
                    .child(editor.title()),
            );
        for (index, label) in fields.iter().enumerate() {
            form = form.child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .w(px(s(165.0)))
                            .flex_shrink_0()
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.84)))
                            .child(crate::localization::text(label)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .when(index == 7, |element| element.h(px(s(96.0))))
                            .child(self.editor_inputs[index].element()),
                    ),
            );
        }
        let this = cx.entity().downgrade();
        form = form.child(
            div().pl(px(s(173.0))).child(
                Checkbox::new("agent-editor-enabled")
                    .checked(self.editor_enabled)
                    .label(crate::localization::text(
                        "explorer.sql_agent.enabled_checkbox",
                    ))
                    .on_click(move |checked: &bool, _window, app| {
                        if let Some(this) = this.upgrade() {
                            this.update(app, |this, cx| {
                                this.editor_enabled = *checked;
                                cx.notify();
                            });
                        }
                    }),
            ),
        );
        let preview = editor_script(
            &*self.context.services.admin_tools,
            editor,
            &self.editor_values,
            self.editor_enabled,
        );
        form.child(
            div()
                .mt(px(s(4.0)))
                .p(px(s(8.0)))
                .min_h(px(s(72.0)))
                .bg(Theme::sidebar_bg())
                .border_1()
                .border_color(Theme::border())
                .rounded(px(s(4.0)))
                .text_size(px(Theme::font_size_ratio(0.82)))
                .whitespace_normal()
                .child(match preview {
                    Ok(sql) => div().text_color(Theme::kind_string()).child(sql),
                    Err(reason) => div().text_color(Theme::warning()).child(reason),
                }),
        )
        .into_any_element()
    }

    /// Script a job action into an editor tab (script-first — nothing runs
    /// from the pane) and close so the user lands on the reviewed SQL.
    fn script_job(&mut self, action: JobScriptAction, cx: &mut gpui::Context<Self>) {
        match self.selected_job(cx) {
            Some((name, _)) => {
                cx.emit(ToolDialogEvent::ScriptToEditor {
                    sql: action.sql_for(&*self.context.services.admin_tools, &name),
                });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::format(
                        "explorer.sql_agent.job_action_scripted",
                        &[("action", &action.label()), ("name", &name)],
                    ),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            None => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::text("explorer.sql_agent.select_job"),
            }),
        }
    }

    /// Script deletion of the selected row into an editor tab. Same
    /// script-first contract as job control: the pane never deletes anything
    /// itself.
    fn script_delete(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(row) = self.selected_data_row(cx).map(<[CellValue]>::to_vec) else {
            cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Info,
                message: crate::localization::format(
                    "explorer.sql_agent.select_row",
                    &[("tab", &self.tab.label().to_lowercase())],
                ),
            });
            return;
        };
        match delete_script(&*self.context.services.admin_tools, self.tab, &row) {
            Ok(sql) => {
                cx.emit(ToolDialogEvent::ScriptToEditor { sql });
                cx.emit(ToolDialogEvent::Status {
                    level: crate::app::StatusLevel::Info,
                    message: crate::localization::text("explorer.sql_agent.deletion_scripted"),
                });
                cx.emit(ToolDialogEvent::Close);
            }
            Err(message) => cx.emit(ToolDialogEvent::Status {
                level: crate::app::StatusLevel::Warning,
                message,
            }),
        }
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for SqlAgentPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SqlAgentPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let active = self.tab;
        let available = self.capability == Capability::Available;

        let tabs = div()
            .flex()
            .flex_row()
            .gap(px(s(4.0)))
            .children(AgentTab::all().map(|tab| {
                let is_active = tab == active;
                div()
                    .px(px(s(10.0)))
                    .py(px(s(4.0)))
                    .rounded(px(s(4.0)))
                    .cursor_pointer()
                    .bg(if is_active {
                        Theme::selection()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if is_active {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(tab.label())
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.switch_tab(tab, cx)),
                    )
            }));

        let content: gpui::AnyElement = if self.editor.is_some() {
            self.render_editor(cx)
        } else {
            match &self.capability {
                Capability::Checking => div()
                    .p(px(s(12.0)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text("explorer.sql_agent.checking"))
                    .into_any_element(),
                Capability::Unavailable(reason) => div()
                    .p(px(s(12.0)))
                    .text_color(Theme::text())
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .whitespace_normal()
                    .child(reason.clone())
                    .into_any_element(),
                Capability::Available => {
                    if let Some(err) = &self.error {
                        div()
                            .p(px(s(12.0)))
                            .text_color(Theme::error())
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .whitespace_normal()
                            .child(crate::localization::format(
                                "explorer.sql_agent.load_failed",
                                &[("tab", &self.tab.label()), ("error", err)],
                            ))
                            .into_any_element()
                    } else if let Some(grid) = &self.grid {
                        div().size_full().child(grid.clone()).into_any_element()
                    } else if self.loading {
                        div()
                            .p(px(s(12.0)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("explorer.sql_agent.loading"))
                            .into_any_element()
                    } else {
                        div()
                            .p(px(s(12.0)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::text("explorer.sql_agent.no_rows"))
                            .into_any_element()
                    }
                }
            }
        };

        // Footer actions are phase-dependent: the list phase offers
        // create/edit/script-delete (plus Jobs-tab script-first job control),
        // the inline editor phase offers cancel + the scripting action.
        let mut buttons: Vec<gpui::AnyElement> = Vec::new();
        if self.editor.is_some() {
            buttons.push(
                Button::new("agent-cancel-editor")
                    .label(crate::localization::text(
                        "explorer.sql_agent.cancel_editor_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.editor = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
            buttons.push(
                Button::new("agent-script-editor")
                    .label(crate::localization::text(
                        "explorer.sql_agent.open_reviewed_sql_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.script_editor(cx)))
                    .into_any_element(),
            );
        } else if available {
            if matches!(
                self.tab,
                AgentTab::Jobs | AgentTab::Schedules | AgentTab::Alerts | AgentTab::Operators
            ) {
                buttons.push(
                    Button::new("agent-create")
                        .label(crate::localization::text(
                            "explorer.sql_agent.create_button",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_editor(EditorMode::Create, window, cx)
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("agent-edit")
                        .label(crate::localization::text(
                            "explorer.sql_agent.edit_selected_button",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_editor(EditorMode::Edit, window, cx)
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("agent-script-delete")
                        .label(crate::localization::text(
                            "explorer.sql_agent.script_delete_button",
                        ))
                        .disabled(self.selected_data_row(cx).is_none())
                        .on_click(cx.listener(|this, _, _window, cx| this.script_delete(cx)))
                        .into_any_element(),
                );
            }
            if self.tab == AgentTab::Jobs {
                // Script-first job control: visible on the Jobs tab, enabled
                // once a row is selected.
                let job = self.selected_job(cx);
                let enabled_flag = job.as_ref().map(|(_, e)| *e);
                let has_selection = job.is_some();
                for action in [
                    JobScriptAction::Start,
                    JobScriptAction::Stop,
                    // Offer the toggle that applies to the selected job's
                    // current state.
                    if enabled_flag == Some(true) {
                        JobScriptAction::Disable
                    } else {
                        JobScriptAction::Enable
                    },
                ] {
                    buttons.push(
                        Button::new(action.label())
                            .label(action.label())
                            .disabled(!has_selection)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.script_job(action, cx);
                            }))
                            .into_any_element(),
                    );
                }
            }
        }

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("sql-agent-pane", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.sql_agent.title",
                &[("connection", &self.context.profile.name)],
            ))
            .easy_close(true)
            .key_context("SqlAgentPane")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .bare_body()
            .child(div().px(px(s(14.0))).pt(px(s(10.0))).child(tabs))
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
                    .py(px(s(8.0)))
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "explorer.sql_agent.footer",
                        &[("count", &self.row_count.to_string())],
                    )),
            )
            .leading_button(
                Button::new("agent-refresh")
                    .label(crate::localization::text(
                        "explorer.sql_agent.refresh_button",
                    ))
                    .disabled(self.loading || !available || self.editor.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| this.load_tab(cx))),
            );
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::DefaultAdminToolsService;

    #[test]
    fn every_tab_has_a_query_against_msdb() {
        let admin = DefaultAdminToolsService;
        for tab in AgentTab::all() {
            let sql = tab.query(&admin);
            assert!(
                sql.contains("msdb.dbo."),
                "{} query does not target msdb: {sql}",
                tab.label()
            );
        }
    }

    #[test]
    fn capability_probe_checks_sysjobs() {
        let admin = DefaultAdminToolsService;
        assert!(admin.agent_capability_query().contains("msdb.dbo.sysjobs"));
    }

    #[test]
    fn job_script_actions_are_guarded_and_escaped() {
        let admin = DefaultAdminToolsService;
        let sql = JobScriptAction::Start.sql_for(&admin, "Nightly ETL");
        assert!(sql.starts_with("-- Review, then run."), "{sql}");
        assert!(
            sql.contains("sp_start_job @job_name = N'Nightly ETL'"),
            "{sql}"
        );
        let sql = JobScriptAction::Stop.sql_for(&admin, "O'Brien Job");
        assert!(sql.contains("N'O''Brien Job'"), "{sql}");
        let sql = JobScriptAction::Enable.sql_for(&admin, "J");
        assert!(
            sql.contains("sp_update_job @job_name = N'J', @enabled = 1;"),
            "{sql}"
        );
        let sql = JobScriptAction::Disable.sql_for(&admin, "J");
        assert!(sql.contains("@enabled = 0;"), "{sql}");
    }

    #[test]
    fn job_from_row_reads_name_and_enabled_variants() {
        let row = vec![
            CellValue::String("id".into()),
            CellValue::String("Nightly ETL".into()),
            CellValue::Bool(true),
        ];
        assert_eq!(job_from_row(&row), Some(("Nightly ETL".into(), true)));
        let row = vec![
            CellValue::String("id".into()),
            CellValue::String("J".into()),
            CellValue::I64(0),
        ];
        assert_eq!(job_from_row(&row), Some(("J".into(), false)));
        // No name → no action target.
        let row = vec![CellValue::Null, CellValue::Null];
        assert_eq!(job_from_row(&row), None);
    }

    #[test]
    fn operator_editor_scripts_create_and_edit() {
        let admin = DefaultAdminToolsService;
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        values[0] = "On-call DBA".into();
        values[1] = "oncall@example.com".into();
        let editor = AgentEditor {
            tab: AgentTab::Operators,
            mode: EditorMode::Create,
            original_name: None,
            record_id: None,
        };
        let sql = editor_script(&admin, &editor, &values, true).unwrap();
        assert!(
            sql.contains("sp_add_operator @name = N'On-call DBA'"),
            "{sql}"
        );
        assert!(
            sql.contains("@email_address = N'oncall@example.com'"),
            "{sql}"
        );

        let editor = AgentEditor {
            tab: AgentTab::Operators,
            mode: EditorMode::Edit,
            original_name: Some("Old Name".into()),
            record_id: Some(3),
        };
        let sql = editor_script(&admin, &editor, &values, false).unwrap();
        assert!(
            sql.contains("sp_update_operator @name = N'Old Name'"),
            "{sql}"
        );
        assert!(sql.contains("@enabled = 0"), "{sql}");

        values[0] = String::new();
        assert!(editor_script(&admin, &editor, &values, true).is_err());
    }

    #[test]
    fn alert_editor_appends_operator_notification_when_named() {
        let admin = DefaultAdminToolsService;
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        values[0] = "deadlocks".into();
        values[1] = "1205".into();
        values[2] = "0".into();
        values[3] = "60".into();
        values[4] = "On-call DBA".into();
        let editor = AgentEditor {
            tab: AgentTab::Alerts,
            mode: EditorMode::Create,
            original_name: None,
            record_id: None,
        };
        let sql = editor_script(&admin, &editor, &values, true).unwrap();
        assert!(sql.contains("sp_add_alert"), "{sql}");
        assert!(
            sql.contains(
                "sp_add_notification @alert_name = N'deadlocks', @operator_name = N'On-call DBA'"
            ),
            "{sql}"
        );

        values[4] = String::new();
        let sql = editor_script(&admin, &editor, &values, true).unwrap();
        assert!(!sql.contains("sp_add_notification"), "{sql}");
    }

    #[test]
    fn delete_scripts_cover_every_editable_tab() {
        let admin = DefaultAdminToolsService;
        let job_row = vec![
            CellValue::String("id".into()),
            CellValue::String("Nightly ETL".into()),
            CellValue::Bool(true),
        ];
        let sql = delete_script(&admin, AgentTab::Jobs, &job_row).unwrap();
        assert!(sql.starts_with("-- Review before running"), "{sql}");
        assert!(
            sql.contains("sp_delete_job @job_name = N'Nightly ETL'"),
            "{sql}"
        );

        let schedule_row = vec![CellValue::I64(7), CellValue::String("daily".into())];
        let sql = delete_script(&admin, AgentTab::Schedules, &schedule_row).unwrap();
        assert!(sql.contains("sp_delete_schedule @schedule_id = 7"), "{sql}");

        let alert_row = vec![CellValue::I64(1), CellValue::String("deadlocks".into())];
        let sql = delete_script(&admin, AgentTab::Alerts, &alert_row).unwrap();
        assert!(
            sql.contains("sp_delete_alert @name = N'deadlocks'"),
            "{sql}"
        );

        let operator_row = vec![CellValue::I64(2), CellValue::String("On-call DBA".into())];
        let sql = delete_script(&admin, AgentTab::Operators, &operator_row).unwrap();
        assert!(
            sql.contains("sp_delete_operator @name = N'On-call DBA'"),
            "{sql}"
        );

        assert!(delete_script(&admin, AgentTab::History, &job_row).is_err());
        assert!(delete_script(&admin, AgentTab::Alerts, &[CellValue::Null]).is_err());
    }

    #[test]
    fn job_editor_scripts_create_edit_and_optional_steps() {
        let admin = DefaultAdminToolsService;
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        values[0] = "Nightly O'Brien".into();
        values[1] = "Load warehouse".into();
        values[4] = "Load".into();
        values[5] = "TSQL".into();
        values[6] = "Warehouse".into();
        values[7] = "EXEC dbo.LoadWarehouse".into();
        let create = editor_script(
            &admin,
            &AgentEditor {
                tab: AgentTab::Jobs,
                mode: EditorMode::Create,
                original_name: None,
                record_id: None,
            },
            &values,
            true,
        )
        .unwrap();
        assert!(create.starts_with("-- Review this SQL Agent change"));
        assert!(create.contains("sp_add_job"));
        assert!(create.contains("sp_add_jobstep"));
        assert!(create.contains("sp_add_jobserver"));
        values[3] = "2".into();
        let edit = editor_script(
            &admin,
            &AgentEditor {
                tab: AgentTab::Jobs,
                mode: EditorMode::Edit,
                original_name: Some("Old Job".into()),
                record_id: None,
            },
            &values,
            false,
        )
        .unwrap();
        assert!(edit.contains("sp_update_job"));
        assert!(edit.contains("sp_update_jobstep"));
        assert!(edit.contains("@step_id = 2"));
    }

    #[test]
    fn schedule_editor_validates_time_and_scripts_attach() {
        let admin = DefaultAdminToolsService;
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        values[0] = "Weekdays".into();
        values[1] = "8".into();
        values[2] = "62".into();
        values[3] = "023000".into();
        values[4] = "Nightly".into();
        let script = editor_script(
            &admin,
            &AgentEditor {
                tab: AgentTab::Schedules,
                mode: EditorMode::Edit,
                original_name: Some("Old".into()),
                record_id: Some(7),
            },
            &values,
            true,
        )
        .unwrap();
        assert!(script.contains("sp_update_schedule @schedule_id = 7"));
        assert!(script.contains("sp_attach_schedule"));
        values[3] = "256199".into();
        assert!(
            editor_script(
                &admin,
                &AgentEditor {
                    tab: AgentTab::Schedules,
                    mode: EditorMode::Create,
                    original_name: None,
                    record_id: None,
                },
                &values,
                true,
            )
            .unwrap_err()
            .contains("HHMMSS")
        );
    }

    #[test]
    fn alert_editor_requires_one_condition_and_scripts_updates() {
        let admin = DefaultAdminToolsService;
        let mut values: [String; 8] = std::array::from_fn(|_| String::new());
        values[0] = "Deadlocks".into();
        values[1] = "1205".into();
        values[2] = "0".into();
        values[3] = "60".into();
        let script = editor_script(
            &admin,
            &AgentEditor {
                tab: AgentTab::Alerts,
                mode: EditorMode::Edit,
                original_name: Some("Old alert".into()),
                record_id: Some(3),
            },
            &values,
            true,
        )
        .unwrap();
        assert!(script.contains("sp_update_alert"));
        assert!(script.contains("@message_id = 1205"));
        values[2] = "16".into();
        assert!(
            editor_script(
                &admin,
                &AgentEditor {
                    tab: AgentTab::Alerts,
                    mode: EditorMode::Create,
                    original_name: None,
                    record_id: None,
                },
                &values,
                true,
            )
            .is_err()
        );
    }

    /// The pane's grid, drawn in the real pane, gets room for its rows (a
    /// grid in an auto-height wrapper paints zero pixels tall).
    #[gpui::test]
    fn sql_agent_grid_gets_room_to_paint_its_rows(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
        let context =
            crate::tool_dialog::unreachable_tool_context(sqlly_core::DatabaseType::SqlServer);
        let (view, cx) = cx.add_window_view(|window, cx| SqlAgentPane::new(context, window, cx));
        cx.simulate_resize(gpui::size(gpui::px(1440.0), gpui::px(900.0)));
        cx.run_until_parked();
        view.update(cx, |pane, cx| {
            pane.capability = Capability::Available;
            pane.error = None;
            pane.loading = false;
            pane.grid =
                crate::tool_dialog::build_result_grid(&crate::tool_dialog::sample_tool_set(20), cx);
            cx.notify();
        });
        cx.run_until_parked();
        view.read_with(cx, |pane, cx| {
            let grid = pane.grid.as_ref().expect("grid");
            crate::tool_dialog::assert_grid_shows_rows(grid, 3, "SQL Agent", cx);
        });
    }
}
