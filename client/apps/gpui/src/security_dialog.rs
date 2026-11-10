//! SQL Server security management dialog: logins, database users, role
//! membership, and database access: the gpui counterpart to SSMS's Login
//! Properties / User Properties pages.
//!
//! The dialog owns its queries: it reads through `Services::queries` against
//! the connection that was active when it opened, and every change is a batch
//! of T-SQL statements the user reviews in a live preview before choosing
//! Apply. The pure model (row parsing, baseline/form diffing, validation) is
//! separated from the view so it is testable without a window.

// Browser build: swap tokio's channels/spawn for the reactor-free shim so
// the `tokio::` paths below compile unchanged.
#[cfg(target_arch = "wasm32")]
use crate::wasm_tokio as tokio;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use gpui::{
    AnyElement, App, Div, Entity, EventEmitter, FocusHandle, Focusable, Hsla, IntoElement,
    ParentElement, Render, Stateful, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::{Disableable as _, Sizable as _};
use sqlly_core::{CellValue, ConnectionProfile, DatabaseType};
use sqlly_services::{AdminToolsService, QueryEvent, Services, tool_dialects::SecurityDialect};

use crate::dialog::{Dialog, DialogFrame};
use crate::security_model::{
    PasswordStrength, PlannedStatement, cell_bool, cell_opt_text, cell_text,
    clickhouse as ch_model, generate_password, mask_password, mysql as my_model, name_taken,
    one_line, password_strength, postgres as pg_model,
};
use crate::theme::{Theme, themed_row};
use sqlly_services::admin::security_sql::clickhouse as ch_sql;

fn localized_format<T: std::fmt::Display>(key: &str, arguments: &[(&str, T)]) -> String {
    crate::localization::format(key, arguments)
}

const DIALOG_WIDTH: f32 = 1040.0;
const DIALOG_HEIGHT: f32 = 740.0;
const DIALOG_MIN_WIDTH: f32 = 760.0;
const DIALOG_MIN_HEIGHT: f32 = 540.0;
const LIST_WIDTH: f32 = 270.0;
const CHECKLIST_MAX_HEIGHT: f32 = 168.0;
const SQL_ROWS: usize = 7;
const SQL_HEIGHT: f32 = 120.0;

// ---------------------------------------------------------------------------
// Pure model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SecurityTab {
    Logins,
    Users,
    Roles,
    Accounts,
    MyRoles,
    /// ClickHouse users (read-only: list, describe, script).
    ChUsers,
    /// ClickHouse roles (read-only: list, describe, script).
    ChRoles,
}

pub fn tabs_for(dialect: Option<SecurityDialect>, roles_supported: bool) -> Vec<SecurityTab> {
    match dialect {
        Some(SecurityDialect::SqlServer) | None => {
            vec![SecurityTab::Logins, SecurityTab::Users]
        }
        Some(SecurityDialect::Postgres) => vec![SecurityTab::Roles],
        Some(SecurityDialect::MySql) => {
            if roles_supported {
                vec![SecurityTab::Accounts, SecurityTab::MyRoles]
            } else {
                vec![SecurityTab::Accounts]
            }
        }
        Some(SecurityDialect::MariaDb) => vec![SecurityTab::Accounts],
        Some(SecurityDialect::ClickHouse) => vec![SecurityTab::ChUsers, SecurityTab::ChRoles],
        // Oracle users and roles ride the Roles tab: a login role is a user,
        // a non-login role a role, and "connect" is CREATE SESSION.
        Some(SecurityDialect::Oracle) => vec![SecurityTab::Roles],
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LoginAuth {
    Sql,
    Windows,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoginEntry {
    pub name: String,
    pub kind: String,
    pub is_disabled: bool,
}

/// How a new SQL Server database user authenticates.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum UserAuth {
    /// Mapped to a login that already exists on the server.
    #[default]
    ExistingLogin,
    /// Create a SQL login with a password and map the user to it, in one go.
    NewLogin,
    /// Contained database user with its own password (no server login).
    Contained,
    /// `WITHOUT LOGIN`: cannot connect, used for impersonation/ownership.
    NoLogin,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UserEntry {
    pub name: String,
    pub kind: String,
    /// `authentication_type_desc`: INSTANCE, DATABASE (contained), NONE,
    /// WINDOWS, EXTERNAL.
    pub auth_type: String,
    pub login: Option<String>,
    pub default_schema: Option<String>,
}

/// Where a password typed on the Users tab lands for an existing user.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub enum UserPasswordTarget {
    /// Windows/external/without-login users have no password here.
    #[default]
    None,
    /// Contained user: `ALTER USER … WITH PASSWORD`.
    Contained,
    /// Mapped to this SQL login: `ALTER LOGIN … WITH PASSWORD`.
    Login(String),
}

impl UserPasswordTarget {
    /// Resolve from the user row and the server's logins (only SQL logins
    /// carry a password).
    pub fn for_user(user: &UserEntry, logins: &[LoginEntry]) -> Self {
        if user.auth_type.eq_ignore_ascii_case("DATABASE") {
            return Self::Contained;
        }
        match &user.login {
            Some(login)
                if logins
                    .iter()
                    .any(|l| l.name.eq_ignore_ascii_case(login) && l.kind == "SQL_LOGIN") =>
            {
                Self::Login(login.clone())
            }
            _ => Self::None,
        }
    }
}

/// Server-side state of the login being edited, loaded on selection.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LoginBaseline {
    pub is_disabled: bool,
    pub server_roles: BTreeSet<String>,
    /// database name -> mapped user name
    pub db_users: BTreeMap<String, String>,
}

/// Server-side state of the database user being edited.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UserBaseline {
    pub default_schema: Option<String>,
    pub roles: BTreeSet<String>,
    pub password_target: UserPasswordTarget,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LoginForm {
    pub name: String,
    pub auth: LoginAuth,
    pub password: String,
    pub password_confirm: String,
    pub enabled: bool,
    pub server_roles: BTreeSet<String>,
    pub db_access: BTreeSet<String>,
}

impl Default for LoginForm {
    fn default() -> Self {
        Self {
            name: String::new(),
            auth: LoginAuth::Sql,
            password: String::new(),
            password_confirm: String::new(),
            enabled: true,
            server_roles: BTreeSet::new(),
            db_access: BTreeSet::new(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct UserForm {
    pub name: String,
    pub auth: UserAuth,
    pub login: String,
    pub password: String,
    pub password_confirm: String,
    pub default_schema: String,
    pub roles: BTreeSet<String>,
}

/// Rows from `SecurityCatalog::server_logins_query`:
/// name, type_desc, is_disabled, create_date, modify_date.
pub fn parse_logins(rows: &[Vec<CellValue>]) -> Vec<LoginEntry> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(LoginEntry {
                name,
                kind: cell_text(row.get(1)?),
                is_disabled: row.get(2).map(cell_bool).unwrap_or(false),
            })
        })
        .collect()
}

/// Rows from `SecurityCatalog::database_users_with_login_query`:
/// name, type_desc, authentication_type_desc, default_schema_name, login_name.
pub fn parse_users(rows: &[Vec<CellValue>]) -> Vec<UserEntry> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(UserEntry {
                name,
                kind: cell_text(row.get(1)?),
                auth_type: row.get(2).map(cell_text).unwrap_or_default(),
                default_schema: row.get(3).and_then(cell_opt_text),
                login: row.get(4).and_then(cell_opt_text),
            })
        })
        .collect()
}

/// Single-column name results (databases, server roles, memberships).
pub fn parse_names(rows: &[Vec<CellValue>]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| {
            let name = cell_text(row.first()?).trim().to_string();
            if name.is_empty() { None } else { Some(name) }
        })
        .collect()
}

/// Rows from `SecurityCatalog::login_db_user_mapping_query`:
/// database_name, user_name.
pub fn parse_db_user_mapping(rows: &[Vec<CellValue>]) -> BTreeMap<String, String> {
    rows.iter()
        .filter_map(|row| {
            let db = cell_text(row.first()?).trim().to_string();
            let user = cell_text(row.get(1)?).trim().to_string();
            if db.is_empty() || user.is_empty() {
                None
            } else {
                Some((db, user))
            }
        })
        .collect()
}

/// SQL Server caps identifiers at 128 chars; passwords at 128 too.
const MAX_IDENTIFIER_LEN: usize = 128;

/// SQL Server error text often spans several lines (severity/state chains);
/// the status line is single-line, so collapse whitespace runs into one.
pub fn validate_login(
    is_new: bool,
    existing_names: &BTreeSet<String>,
    form: &LoginForm,
) -> Vec<String> {
    let mut errors = Vec::new();
    let name = form.name.trim();
    if name.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.login_name_required",
        ));
    } else if name.chars().count() > MAX_IDENTIFIER_LEN {
        errors.push(crate::security_model::statement_description(
            "explorer.security.login_name_too_long",
            &[("max", &MAX_IDENTIFIER_LEN.to_string())],
        ));
    } else if is_new && name_taken(existing_names, name) {
        errors.push(crate::security_model::statement_description(
            "explorer.security.login_exists",
            &[("name", name)],
        ));
    } else if is_new && form.auth == LoginAuth::Windows && !name.contains('\\') {
        errors.push(crate::localization::text(
            "explorer.security.windows_login_format",
        ));
    }
    if form.auth == LoginAuth::Sql {
        if is_new && form.password.is_empty() {
            errors.push(crate::localization::text(
                "explorer.security.sql_password_required",
            ));
        }
        if !form.password.is_empty() && form.password != form.password_confirm {
            errors.push(crate::localization::text(
                "explorer.security.passwords_do_not_match",
            ));
        }
        if form.password.chars().count() > MAX_IDENTIFIER_LEN {
            errors.push(crate::security_model::statement_description(
                "explorer.security.password_too_long",
                &[("max", &MAX_IDENTIFIER_LEN.to_string())],
            ));
        }
    }
    errors
}

#[cfg(test)]
pub fn validate_user(
    is_new: bool,
    existing_names: &BTreeSet<String>,
    known_logins: &[String],
    form: &UserForm,
) -> Vec<String> {
    validate_user_with_target(is_new, existing_names, known_logins, form, None)
}

/// [`validate_user`] plus, for an existing user, whether a typed password
/// has somewhere to go.
pub fn validate_user_with_target(
    is_new: bool,
    existing_names: &BTreeSet<String>,
    known_logins: &[String],
    form: &UserForm,
    target: Option<&UserPasswordTarget>,
) -> Vec<String> {
    let mut errors = validate_user_fields(is_new, existing_names, known_logins, form);
    if !is_new && !form.password.is_empty() && target == Some(&UserPasswordTarget::None) {
        errors.push(crate::localization::text(
            "explorer.security.user_password_not_applicable",
        ));
    }
    errors
}

fn validate_user_fields(
    is_new: bool,
    existing_names: &BTreeSet<String>,
    known_logins: &[String],
    form: &UserForm,
) -> Vec<String> {
    let mut errors = Vec::new();
    let name = form.name.trim();
    if name.is_empty() {
        errors.push(crate::localization::text(
            "explorer.security.user_name_required",
        ));
    } else if name.chars().count() > MAX_IDENTIFIER_LEN {
        errors.push(crate::security_model::statement_description(
            "explorer.security.user_name_too_long",
            &[("max", &MAX_IDENTIFIER_LEN.to_string())],
        ));
    } else if is_new && name_taken(existing_names, name) {
        errors.push(crate::security_model::statement_description(
            "explorer.security.user_exists",
            &[("name", name)],
        ));
    }
    if is_new {
        match form.auth {
            UserAuth::ExistingLogin => {
                let login = form.login.trim();
                if login.is_empty() {
                    errors.push(crate::localization::text(
                        "explorer.security.user_login_required",
                    ));
                } else if !known_logins.iter().any(|l| l.eq_ignore_ascii_case(login)) {
                    errors.push(crate::security_model::statement_description(
                        "explorer.security.user_login_missing",
                        &[("login", login)],
                    ));
                }
            }
            UserAuth::NewLogin => {
                if !name.is_empty() && known_logins.iter().any(|l| l.eq_ignore_ascii_case(name)) {
                    errors.push(crate::security_model::statement_description(
                        "explorer.security.user_new_login_exists",
                        &[("name", name)],
                    ));
                }
                if form.password.is_empty() {
                    errors.push(crate::localization::text(
                        "explorer.security.user_new_password_required",
                    ));
                }
            }
            UserAuth::Contained => {
                if form.password.is_empty() {
                    errors.push(crate::localization::text(
                        "explorer.security.contained_password_required",
                    ));
                }
            }
            UserAuth::NoLogin => {}
        }
    }
    if !form.password.is_empty() {
        if form.password != form.password_confirm {
            errors.push(crate::localization::text(
                "explorer.security.passwords_do_not_match",
            ));
        }
        if form.password.chars().count() > MAX_IDENTIFIER_LEN {
            errors.push(crate::security_model::statement_description(
                "explorer.security.password_too_long",
                &[("max", &MAX_IDENTIFIER_LEN.to_string())],
            ));
        }
    }
    let schema = form.default_schema.trim();
    if schema.chars().count() > MAX_IDENTIFIER_LEN {
        errors.push(crate::security_model::statement_description(
            "explorer.security.schema_name_too_long",
            &[("max", &MAX_IDENTIFIER_LEN.to_string())],
        ));
    }
    errors
}

/// Diff a login form against its loaded baseline. `None` baseline = new login.
/// Statement order is dependency order: the login exists before it can join
/// roles or be mapped into databases.
pub fn diff_login(
    admin: &dyn AdminToolsService,
    baseline: Option<&LoginBaseline>,
    form: &LoginForm,
) -> Vec<PlannedStatement> {
    let name = form.name.trim();
    if name.is_empty() {
        return Vec::new();
    }
    let mut statements = Vec::new();
    match baseline {
        None => {
            let create = match form.auth {
                LoginAuth::Sql => admin.security_create_login_sql(name, &form.password),
                LoginAuth::Windows => admin.security_create_windows_login_sql(name),
            };
            statements.push(PlannedStatement::server(
                create,
                format!("create login {name}"),
            ));
            if !form.enabled {
                statements.push(PlannedStatement::server(
                    admin.security_alter_login_enabled_sql(name, false),
                    format!("disable login {name}"),
                ));
            }
            for role in &form.server_roles {
                statements.push(PlannedStatement::server(
                    admin.security_alter_server_role_sql(role, name, true),
                    format!("add {name} to server role {role}"),
                ));
            }
            for db in &form.db_access {
                statements.push(PlannedStatement::in_db(
                    db,
                    admin.security_create_user_sql(name, Some(name), Some("dbo")),
                    format!("create user {name} in {db}"),
                ));
            }
        }
        Some(base) => {
            if !form.password.is_empty() {
                statements.push(PlannedStatement::server(
                    admin.security_alter_login_password_sql(name, &form.password),
                    format!("reset the password for {name}"),
                ));
            }
            if form.enabled == base.is_disabled {
                statements.push(PlannedStatement::server(
                    admin.security_alter_login_enabled_sql(name, form.enabled),
                    format!(
                        "{} login {name}",
                        if form.enabled { "enable" } else { "disable" }
                    ),
                ));
            }
            for role in form.server_roles.difference(&base.server_roles) {
                statements.push(PlannedStatement::server(
                    admin.security_alter_server_role_sql(role, name, true),
                    format!("add {name} to server role {role}"),
                ));
            }
            for role in base.server_roles.difference(&form.server_roles) {
                statements.push(PlannedStatement::server(
                    admin.security_alter_server_role_sql(role, name, false),
                    format!("remove {name} from server role {role}"),
                ));
            }
            let mapped: BTreeSet<String> = base.db_users.keys().cloned().collect();
            for db in form.db_access.difference(&mapped) {
                statements.push(PlannedStatement::in_db(
                    db,
                    admin.security_create_user_sql(name, Some(name), Some("dbo")),
                    format!("create user {name} in {db}"),
                ));
            }
            for db in mapped.difference(&form.db_access) {
                if let Some(user) = base.db_users.get(db) {
                    statements.push(PlannedStatement::in_db(
                        db,
                        admin.security_drop_user_sql(user),
                        format!("drop user {user} from {db}"),
                    ));
                }
            }
        }
    }
    statements
}

/// Diff a user form against its loaded baseline. All statements run in the
/// database the Users tab is scoped to.
pub fn diff_user(
    admin: &dyn AdminToolsService,
    baseline: Option<&UserBaseline>,
    database: &str,
    form: &UserForm,
) -> Vec<PlannedStatement> {
    let name = form.name.trim();
    if name.is_empty() {
        return Vec::new();
    }
    let mut statements = Vec::new();
    match baseline {
        None => {
            let schema = match form.default_schema.trim() {
                "" => None,
                schema => Some(schema),
            };
            match form.auth {
                UserAuth::ExistingLogin => statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_create_user_sql(name, Some(form.login.trim()), schema),
                    format!(
                        "create user {name} in {database} for login {}",
                        form.login.trim()
                    ),
                )),
                UserAuth::NewLogin => {
                    statements.push(PlannedStatement::server(
                        admin.security_create_login_sql(name, &form.password),
                        format!("create login {name}"),
                    ));
                    statements.push(PlannedStatement::in_db(
                        database,
                        admin.security_create_user_sql(name, Some(name), schema),
                        format!("create user {name} in {database} for login {name}"),
                    ));
                }
                UserAuth::Contained => statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_create_contained_user_sql(name, &form.password, schema),
                    format!("create contained user {name} in {database}"),
                )),
                UserAuth::NoLogin => statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_create_user_sql(name, None, schema),
                    format!("create user {name} in {database} without login"),
                )),
            }
            for role in &form.roles {
                statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_alter_role_sql(role, name, true),
                    format!("add {name} to role {role}"),
                ));
            }
        }
        Some(base) => {
            if !form.password.is_empty() {
                match &base.password_target {
                    UserPasswordTarget::Contained => statements.push(PlannedStatement::in_db(
                        database,
                        admin.security_alter_user_password_sql(name, &form.password),
                        format!("reset the password for {name}"),
                    )),
                    UserPasswordTarget::Login(login) => statements.push(PlannedStatement::server(
                        admin.security_alter_login_password_sql(login, &form.password),
                        format!("reset the password of login {login}"),
                    )),
                    UserPasswordTarget::None => {}
                }
            }
            let schema = form.default_schema.trim();
            if !schema.is_empty() && base.default_schema.as_deref() != Some(schema) {
                statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_alter_user_default_schema_sql(name, schema),
                    format!("set default schema of {name} to {schema}"),
                ));
            }
            for role in form.roles.difference(&base.roles) {
                statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_alter_role_sql(role, name, true),
                    format!("add {name} to role {role}"),
                ));
            }
            for role in base.roles.difference(&form.roles) {
                statements.push(PlannedStatement::in_db(
                    database,
                    admin.security_alter_role_sql(role, name, false),
                    format!("remove {name} from role {role}"),
                ));
            }
        }
    }
    statements
}

// ---------------------------------------------------------------------------
// Async query plumbing
// ---------------------------------------------------------------------------

/// What Apply would run, and the problems that block it.
struct SecurityPlan {
    statements: Vec<PlannedStatement>,
    errors: Vec<String>,
}

/// Accumulated result of one batch: rows per result set, or the first error.
#[derive(Debug, Default)]
struct QueryOutcome {
    sets: Vec<Vec<Vec<CellValue>>>,
    error: Option<String>,
}

impl QueryOutcome {
    fn set(&self, index: usize) -> &[Vec<CellValue>] {
        self.sets.get(index).map(Vec::as_slice).unwrap_or(&[])
    }
}

async fn execute_collect(
    services: &Services,
    profile: &ConnectionProfile,
    sql: &str,
) -> QueryOutcome {
    let mut outcome = QueryOutcome::default();
    let mut handle = match services.queries.execute(profile, sql, 256).await {
        Ok(handle) => handle,
        Err(e) => {
            outcome.error = Some(crate::localization::message(e.message()));
            return outcome;
        }
    };
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::ResultSet { index, .. } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize(index + 1, Vec::new());
                }
            }
            QueryEvent::Rows { index, rows } => {
                if outcome.sets.len() <= index {
                    outcome.sets.resize(index + 1, Vec::new());
                }
                outcome.sets[index].extend(rows);
            }
            QueryEvent::Error { message } => {
                outcome.error = Some(message);
            }
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    outcome
}

/// What a finished query feeds. Carries the principal/db it was issued for so
/// stale results (the user moved on while the query was in flight) are
/// discarded instead of clobbering the form they now see.
#[derive(Clone, Debug)]
enum QueryKind {
    /// databases + server roles + logins (one batch, three result sets).
    Catalog,
    /// server-role memberships + database mappings for one login.
    LoginDetail(String),
    /// users + database roles + full role membership for one database.
    Users(String),
    PgCatalog,
    PgRoleDetail(String),
    MyUsers,
    MyUsersCompat,
    MyGrants(String, String),
    /// users + roles + grants + role grants + settings profiles.
    ChCatalog,
    /// `SHOW CREATE USER/ROLE` + `SHOW GRANTS FOR` one principal (name, is_role).
    ChDetail(String, bool),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LoadState {
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StatusKind {
    Idle,
    Busy,
    Success,
    Error,
}

impl StatusKind {
    fn color(self) -> Hsla {
        match self {
            Self::Idle | Self::Busy => Theme::text_dim(),
            Self::Success => Theme::success(),
            Self::Error => Theme::error(),
        }
    }
}

#[derive(Clone)]
pub enum SecurityDialogEvent {
    Dismissed,
    /// A reviewed script for a principal, to open in a query tab.
    ScriptToEditor {
        sql: String,
    },
}

impl EventEmitter<SecurityDialogEvent> for SecurityDialog {}

// ---------------------------------------------------------------------------
// View
// ---------------------------------------------------------------------------

pub struct SecurityDialog {
    services: Services,
    profile: ConnectionProfile,
    dialect: Option<SecurityDialect>,
    production_read_only: bool,
    tab: SecurityTab,

    databases: Vec<String>,
    server_roles: Vec<String>,
    catalog_state: LoadState,
    catalog_error: String,

    logins: Vec<LoginEntry>,
    selected_login: Option<usize>,
    login_baseline: Option<LoginBaseline>,
    login_detail_state: LoadState,
    editing_new_login: bool,
    login_auth: LoginAuth,
    login_enabled: bool,
    login_server_roles: BTreeSet<String>,
    login_db_access: BTreeSet<String>,

    users_db: Option<String>,
    users: Vec<UserEntry>,
    db_roles: Vec<String>,
    /// role_name -> member names, for the current users database.
    role_members: BTreeMap<String, BTreeSet<String>>,
    users_state: LoadState,
    users_error: String,
    selected_user: Option<usize>,
    user_baseline: Option<UserBaseline>,
    editing_new_user: bool,
    user_login: String,
    user_roles: BTreeSet<String>,
    user_auth: UserAuth,

    pg_roles: Vec<pg_model::PgRoleEntry>,
    pg_all_roles: Vec<String>,
    pg_role_members: BTreeMap<String, BTreeSet<String>>,
    pg_connect_pairs: Vec<(String, String)>,
    pg_schema_pairs: Vec<(String, String)>,
    pg_tables: Vec<(String, String)>,
    selected_pg_role: Option<usize>,
    pg_role_baseline: Option<pg_model::PgRoleBaseline>,
    editing_new_pg_role: bool,
    pg_role_opts: pg_model::RoleOptions,
    pg_member_of: BTreeSet<String>,
    pg_connect_dbs: BTreeSet<String>,
    pg_schema_usage: BTreeSet<String>,
    pg_table_grants: BTreeMap<(String, String), BTreeSet<pg_model::TablePrivilege>>,
    pending_select_pg_role: Option<String>,
    /// Oracle only: the `DBA_*` views were not granted, so the catalog came
    /// from the `USER_*` / `ALL_*` fallbacks and shows the session user's
    /// own privileges only.
    oracle_compat: bool,

    my_users: Vec<my_model::MyUserEntry>,
    my_role_names: BTreeSet<String>,
    my_databases: Vec<String>,
    my_global_privileges: Vec<String>,
    my_database_privileges: Vec<String>,
    selected_my_user: Option<usize>,
    my_user_baseline: Option<my_model::MyUserBaseline>,
    editing_new_my_user: bool,
    my_user_locked: bool,
    my_global: BTreeSet<String>,
    my_database_privs: BTreeMap<String, BTreeSet<String>>,
    my_table_privs: BTreeMap<(String, String), BTreeSet<String>>,
    my_assigned_roles: BTreeSet<String>,
    my_roles_supported: bool,
    my_selected_database: Option<String>,
    pending_select_my_user: Option<(String, String)>,

    ch_users: Vec<ch_model::ChUserEntry>,
    ch_roles: Vec<ch_model::ChRoleEntry>,
    ch_grants: Vec<ch_model::ChGrant>,
    ch_role_grants: Vec<ch_model::ChRoleGrant>,
    /// Settings profiles as (name, who they apply to).
    ch_profiles: Vec<(String, String)>,
    selected_ch_user: Option<usize>,
    selected_ch_role: Option<usize>,
    /// The selected principal's CREATE + GRANT script: the server's own
    /// `SHOW CREATE` / `SHOW GRANTS` text, or the catalog-built script when
    /// those are not permitted. `(principal, script)`.
    ch_detail: Option<(String, String)>,

    login_name_input: Entity<InputState>,
    login_password_input: Entity<InputState>,
    login_password_confirm_input: Entity<InputState>,
    user_name_input: Entity<InputState>,
    user_schema_input: Entity<InputState>,
    user_login_select: Entity<SelectState<Vec<String>>>,
    users_db_select: Entity<SelectState<Vec<String>>>,
    pg_role_name_input: Entity<InputState>,
    pg_role_password_input: Entity<InputState>,
    pg_role_password_confirm_input: Entity<InputState>,
    my_user_input: Entity<InputState>,
    my_host_input: Entity<InputState>,
    my_password_input: Entity<InputState>,
    my_password_confirm_input: Entity<InputState>,
    user_password_input: Entity<InputState>,
    user_password_confirm_input: Entity<InputState>,
    my_db_select: Entity<SelectState<Vec<String>>>,
    /// Narrows the principal list on every tab (cleared on tab switch).
    list_filter_input: Entity<InputState>,
    /// The generated SQL, read-only but selectable; synced from the plan on
    /// every render (only when the text changed, so a selection survives).
    sql_output: Entity<TextareaState>,
    sql_output_text: String,
    /// Passwords are masked in the on-screen SQL unless this is on; Copy All
    /// always copies the runnable SQL.
    show_sql_passwords: bool,
    /// Drop armed for the selected principal: the SQL pane shows the DROP
    /// statements and the primary button turns into the confirmation.
    pending_drop: bool,

    applying: bool,
    status: String,
    status_kind: StatusKind,
    /// Selection is tracked by index, but reloads replace the list and indexes
    /// can shift (a principal added/removed elsewhere reorders everything).
    /// Before a reload we remember the selected principal's *name* here and
    /// re-resolve its index when fresh data lands.
    pending_select_login: Option<String>,
    pending_select_user: Option<String>,
    /// Select widgets need `&mut Window` to receive items, so catalog loads
    /// mark this and render performs the sync (it has the window).
    selects_dirty: bool,
    /// Set when a password was applied successfully; render clears the masked
    /// fields (they too need a Window to mutate).
    clear_passwords: bool,
    focus_handle: FocusHandle,
    focus_requested: bool,
    frame: DialogFrame,
    /// Held, never read: dropping a `Subscription` unsubscribes it, so the
    /// input/select sync lives exactly as long as the dialog does.
    _subscriptions: Vec<Subscription>,
}

impl gpui::Focusable for SecurityDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SecurityDialog {
    pub fn new(
        services: Services,
        profile: ConnectionProfile,
        production_read_only: bool,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let dialect = sqlly_services::tool_dialects::security_dialect(profile.database_type);
        let my_roles_supported = matches!(dialect, Some(SecurityDialect::MySql));
        let default_tab = tabs_for(dialect, my_roles_supported)
            .into_iter()
            .next()
            .unwrap_or(SecurityTab::Logins);
        let login_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.security.login_name_placeholder",
            ))
        });
        let login_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.login_password_placeholder",
                ))
                .masked(true)
        });
        let login_password_confirm_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.login_password_confirm_placeholder",
                ))
                .masked(true)
        });
        let user_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.security.user_name_placeholder",
            ))
        });
        let user_schema_input = cx.new(|cx| InputState::new(window, cx).default_value("dbo"));
        let user_login_select =
            cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let users_db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let pg_role_name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.security.pg_role_name_placeholder",
            ))
        });
        let pg_role_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.pg_password_placeholder",
                ))
                .masked(true)
        });
        let pg_role_password_confirm_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.pg_password_confirm_placeholder",
                ))
                .masked(true)
        });
        let my_user_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.security.mysql_user_placeholder",
            ))
        });
        let my_host_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.mysql_host_placeholder",
                ))
                .default_value("%")
        });
        let my_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.mysql_password_placeholder",
                ))
                .masked(true)
        });
        let my_password_confirm_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.mysql_password_confirm_placeholder",
                ))
                .masked(true)
        });

        let user_password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.user_password_placeholder",
                ))
                .masked(true)
        });
        let user_password_confirm_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.user_password_confirm_placeholder",
                ))
                .masked(true)
        });
        let my_db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let list_filter_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "explorer.security.list_filter_placeholder",
            ))
        });
        let sql_output = cx.new(|cx| {
            TextareaState::new(window, cx).rows(SQL_ROWS).placeholder(
                crate::localization::input_placeholder(
                    window,
                    cx,
                    "explorer.security.sql_output_placeholder",
                ),
            )
        });

        let my_global_privileges = my_model::GLOBAL_PRIVILEGES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let my_database_privileges = my_model::DATABASE_PRIVILEGES
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        let mut subscriptions = vec![
            cx.subscribe_in(
                &user_login_select,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(login) = value {
                        this.user_login = login.to_string();
                        cx.notify();
                    }
                },
            ),
            cx.subscribe_in(
                &users_db_select,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    let db = value.as_ref().map(|v| v.to_string());
                    if db.is_some() && db != this.users_db {
                        this.users_db = db;
                        this.load_users(cx);
                    }
                },
            ),
            cx.subscribe_in(
                &my_db_select,
                window,
                |this, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(db) = value {
                        this.my_selected_database = Some(db.to_string());
                        cx.notify();
                    }
                },
            ),
        ];
        // Every text field feeds the live plan: the SQL pane, the inline
        // problems and the Apply button all re-derive from the inputs, so any
        // keystroke must re-render the dialog (a child input only repaints
        // itself otherwise, which left Apply stuck disabled).
        for input in [
            &login_name_input,
            &login_password_input,
            &login_password_confirm_input,
            &user_name_input,
            &user_schema_input,
            &user_password_input,
            &user_password_confirm_input,
            &pg_role_name_input,
            &pg_role_password_input,
            &pg_role_password_confirm_input,
            &my_user_input,
            &my_host_input,
            &my_password_input,
            &my_password_confirm_input,
            &list_filter_input,
        ] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                |this, _, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.refresh_preview_status(cx);
                    }
                },
            ));
        }

        let mut this = Self {
            services,
            profile,
            dialect,
            production_read_only,
            tab: default_tab,
            databases: Vec::new(),
            server_roles: Vec::new(),
            catalog_state: LoadState::Idle,
            catalog_error: String::new(),
            logins: Vec::new(),
            selected_login: None,
            login_baseline: None,
            login_detail_state: LoadState::Idle,
            editing_new_login: false,
            login_auth: LoginAuth::Sql,
            login_enabled: true,
            login_server_roles: BTreeSet::new(),
            login_db_access: BTreeSet::new(),
            users_db: None,
            users: Vec::new(),
            db_roles: Vec::new(),
            role_members: BTreeMap::new(),
            users_state: LoadState::Idle,
            users_error: String::new(),
            selected_user: None,
            user_baseline: None,
            editing_new_user: false,
            user_login: String::new(),
            user_roles: BTreeSet::new(),
            user_auth: UserAuth::default(),
            pg_roles: Vec::new(),
            pg_all_roles: Vec::new(),
            pg_role_members: BTreeMap::new(),
            pg_connect_pairs: Vec::new(),
            pg_schema_pairs: Vec::new(),
            pg_tables: Vec::new(),
            selected_pg_role: None,
            pg_role_baseline: None,
            editing_new_pg_role: false,
            pg_role_opts: pg_model::RoleOptions::default(),
            pg_member_of: BTreeSet::new(),
            pg_connect_dbs: BTreeSet::new(),
            pg_schema_usage: BTreeSet::new(),
            pg_table_grants: BTreeMap::new(),
            pending_select_pg_role: None,
            oracle_compat: false,
            my_users: Vec::new(),
            my_role_names: BTreeSet::new(),
            my_databases: Vec::new(),
            my_global_privileges,
            my_database_privileges,
            selected_my_user: None,
            my_user_baseline: None,
            editing_new_my_user: false,
            my_user_locked: false,
            my_global: BTreeSet::new(),
            my_database_privs: BTreeMap::new(),
            my_table_privs: BTreeMap::new(),
            my_assigned_roles: BTreeSet::new(),
            my_roles_supported,
            ch_users: Vec::new(),
            ch_roles: Vec::new(),
            ch_grants: Vec::new(),
            ch_role_grants: Vec::new(),
            ch_profiles: Vec::new(),
            selected_ch_user: None,
            selected_ch_role: None,
            ch_detail: None,
            my_selected_database: None,
            pending_select_my_user: None,
            login_name_input,
            login_password_input,
            login_password_confirm_input,
            user_name_input,
            user_schema_input,
            user_login_select,
            users_db_select,
            pg_role_name_input,
            pg_role_password_input,
            pg_role_password_confirm_input,
            my_user_input,
            my_host_input,
            my_password_input,
            my_password_confirm_input,
            user_password_input,
            user_password_confirm_input,
            my_db_select,
            list_filter_input,
            sql_output,
            sql_output_text: String::new(),
            show_sql_passwords: false,
            pending_drop: false,
            applying: false,
            status: crate::localization::text("explorer.security.initial_status"),
            status_kind: StatusKind::Idle,
            pending_select_login: None,
            pending_select_user: None,
            selects_dirty: false,
            clear_passwords: false,
            focus_handle: cx.focus_handle(),
            focus_requested: false,
            // Fixed frame: the tabbed master+editor layout manages its own
            // columns and scroll regions, so the body must not auto-size.
            frame: DialogFrame::new(Theme::scaled(DIALOG_WIDTH), Theme::scaled(DIALOG_HEIGHT))
                .with_min_size(
                    Theme::scaled(DIALOG_MIN_WIDTH),
                    Theme::scaled(DIALOG_MIN_HEIGHT),
                ),
            _subscriptions: subscriptions,
        };
        this.load_catalog(cx);
        this
    }

    pub fn close(&mut self, cx: &mut gpui::Context<Self>) {
        // Closing mid-batch would strand the user without the outcome; the
        // batch is short and cannot be cancelled from here, so hold the
        // dialog until it finishes.
        if self.applying {
            self.set_status(
                StatusKind::Busy,
                crate::localization::text("explorer.security.applying_changes"),
            );
            cx.notify();
            return;
        }
        cx.emit(SecurityDialogEvent::Dismissed);
        cx.notify();
    }

    fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }

    /// Keep the idle status honest about the form under the cursor: the
    /// preview is derived from the diff, so as soon as anything changes the
    /// default hint goes back up (unless a terminal status is on screen).
    fn refresh_preview_status(&mut self, cx: &mut gpui::Context<Self>) {
        // Always re-render: the plan is derived from the form. Only the
        // status text is sticky, so an apply's outcome stays readable.
        if !matches!(self.status_kind, StatusKind::Success | StatusKind::Error) {
            self.set_status(
                StatusKind::Idle,
                crate::localization::text("explorer.security.preview_status"),
            );
        }
        cx.notify();
    }

    // ── Loading ─────────────────────────────────────────────────────────

    fn run_query(&mut self, kind: QueryKind, sql: String, cx: &mut gpui::Context<Self>) {
        let services = self.services.clone();
        let profile = self.profile.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<QueryOutcome>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let outcome = execute_collect(&services, &profile, &sql).await;
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
                        this.handle_query_result(kind, outcome, cx)
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn load_catalog(&mut self, cx: &mut gpui::Context<Self>) {
        self.catalog_state = LoadState::Loading;
        // A post-apply reload must not bury the apply's outcome.
        if !matches!(self.status_kind, StatusKind::Success | StatusKind::Error) {
            self.set_status(
                StatusKind::Busy,
                crate::localization::text("explorer.security.load.loading_principals"),
            );
        }
        let admin = &*self.services.admin_tools;
        let db = self.profile.database_type;
        let sql = match self.dialect {
            Some(SecurityDialect::Postgres | SecurityDialect::Oracle) => {
                let queries = [
                    "roles",
                    "role-members",
                    "db-connect",
                    "schema-usage",
                    "tables",
                ];
                // Oracle without DBA_* access answers the same five sets from
                // the USER_* / ALL_* views.
                let suffix = if self.oracle_compat { "-compat" } else { "" };
                queries
                    .iter()
                    .filter_map(|kind| admin.security_query_for(db, &format!("{kind}{suffix}")))
                    .collect::<Vec<_>>()
                    .join(";\n")
            }
            Some(SecurityDialect::MySql | SecurityDialect::MariaDb) => {
                let users = admin.security_query_for(db, "users").unwrap_or_default();
                let databases = admin
                    .security_query_for(db, "databases")
                    .unwrap_or_default();
                let roles = if self.my_roles_supported {
                    admin
                        .security_query_for(db, "roles")
                        .map(|q| format!("{q};"))
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                format!("{users};\n{databases};\n{roles}")
            }
            Some(SecurityDialect::ClickHouse) => {
                ["users", "roles", "grants", "role-grants", "profiles"]
                    .iter()
                    .filter_map(|kind| admin.security_query_for(db, kind))
                    .collect::<Vec<_>>()
                    .join(";\n")
            }
            _ => format!(
                "{};\n{};\n{}",
                admin.security_user_databases_query(),
                admin.security_server_roles_query(),
                admin.security_query("logins")
            ),
        };
        let kind = match self.dialect {
            Some(SecurityDialect::Postgres | SecurityDialect::Oracle) => QueryKind::PgCatalog,
            Some(SecurityDialect::MySql | SecurityDialect::MariaDb) => QueryKind::MyUsers,
            Some(SecurityDialect::ClickHouse) => QueryKind::ChCatalog,
            _ => QueryKind::Catalog,
        };
        self.run_query(kind, sql, cx);
        cx.notify();
    }

    fn load_login_detail(&mut self, login: &str, cx: &mut gpui::Context<Self>) {
        self.login_detail_state = LoadState::Loading;
        let admin = &*self.services.admin_tools;
        let sql = format!(
            "{};\n{}",
            admin.security_server_role_memberships_query(login),
            admin.security_login_db_user_mapping_query(login, &self.databases)
        );
        self.run_query(QueryKind::LoginDetail(login.to_string()), sql, cx);
        cx.notify();
    }

    fn load_users(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(db) = self.users_db.clone() else {
            self.users = Vec::new();
            self.db_roles = Vec::new();
            self.role_members = BTreeMap::new();
            self.users_state = LoadState::Idle;
            cx.notify();
            return;
        };
        self.users_state = LoadState::Loading;
        self.selected_user = None;
        self.user_baseline = None;
        self.editing_new_user = false;
        let admin = &*self.services.admin_tools;
        let inner = format!(
            "{};\n{};\n{}",
            admin.security_database_users_with_login_query(),
            admin.security_query("roles"),
            admin.security_query("role-members")
        );
        self.run_query(
            QueryKind::Users(db.clone()),
            admin.security_use_database_sql(&db, &inner),
            cx,
        );
        cx.notify();
    }

    fn handle_query_result(
        &mut self,
        kind: QueryKind,
        outcome: QueryOutcome,
        cx: &mut gpui::Context<Self>,
    ) {
        match kind {
            QueryKind::Catalog => {
                if let Some(error) = outcome.error {
                    self.catalog_state = LoadState::Failed;
                    self.catalog_error = one_line(&error);
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.principals_failed",
                            &[("error", &error)],
                        ),
                    );
                } else {
                    self.databases = parse_names(outcome.set(0));
                    self.server_roles = parse_names(outcome.set(1));
                    // Re-resolve the selection by name before swapping the
                    // list in; the pending name wins (set by post-apply
                    // reloads), otherwise keep whatever the user has selected.
                    let keep = self.pending_select_login.take().or_else(|| {
                        self.selected_login
                            .and_then(|i| self.logins.get(i))
                            .map(|l| l.name.clone())
                    });
                    self.logins = parse_logins(outcome.set(2));
                    let resolved = keep.and_then(|name| {
                        self.logins
                            .iter()
                            .position(|l| l.name == name)
                            .zip(Some(name))
                    });
                    match resolved {
                        Some((idx, name)) => {
                            self.selected_login = Some(idx);
                            if self.login_detail_state != LoadState::Loading {
                                self.load_login_detail(&name, cx);
                            }
                        }
                        None => {
                            if self.selected_login.is_some() {
                                self.set_status(
                                    StatusKind::Idle,
                                    crate::localization::text(
                                        "explorer.security.load.selected_login_missing",
                                    ),
                                );
                            }
                            self.selected_login = None;
                            self.login_baseline = None;
                            self.login_detail_state = LoadState::Idle;
                        }
                    }
                    self.catalog_state = LoadState::Ready;
                    self.catalog_error.clear();
                    if matches!(self.status_kind, StatusKind::Busy) {
                        self.set_status(
                            StatusKind::Idle,
                            crate::localization::text("explorer.security.login_catalog_ready"),
                        );
                    }

                    if self.user_login.is_empty() {
                        self.user_login = self
                            .logins
                            .first()
                            .map(|l| l.name.clone())
                            .unwrap_or_default();
                    }

                    // The Users tab defaults to the connection's database when
                    // it is one of the manageable databases.
                    let default_db = self
                        .profile
                        .database
                        .clone()
                        .filter(|db| self.databases.contains(db))
                        .or_else(|| self.databases.first().cloned());
                    if self.users_db.is_none() {
                        self.users_db = default_db;
                    }
                    self.selects_dirty = true;
                    self.ensure_users_loaded(cx);
                }
            }
            QueryKind::LoginDetail(name) => {
                // Stale guard: the selection moved while this was in flight.
                let current = self
                    .selected_login
                    .and_then(|i| self.logins.get(i))
                    .map(|l| l.name.clone());
                if current.as_deref() != Some(name.as_str()) {
                    return;
                }
                if let Some(error) = outcome.error {
                    self.login_detail_state = LoadState::Failed;
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.login_detail_failed",
                            &[("name", &name), ("error", &error)],
                        ),
                    );
                } else {
                    let server_roles: BTreeSet<String> =
                        parse_names(outcome.set(0)).into_iter().collect();
                    let db_users = parse_db_user_mapping(outcome.set(1));
                    let is_disabled = self
                        .selected_login
                        .and_then(|i| self.logins.get(i))
                        .map(|l| l.is_disabled)
                        .unwrap_or(false);
                    self.login_baseline = Some(LoginBaseline {
                        is_disabled,
                        server_roles: server_roles.clone(),
                        db_users: db_users.clone(),
                    });
                    self.login_enabled = !is_disabled;
                    self.login_server_roles = server_roles;
                    self.login_db_access = db_users.keys().cloned().collect();
                    self.login_detail_state = LoadState::Ready;
                }
            }
            QueryKind::Users(db) => {
                if self.users_db.as_deref() != Some(db.as_str()) {
                    return;
                }
                if let Some(error) = outcome.error {
                    self.users_state = LoadState::Failed;
                    self.users_error = one_line(&error);
                    let error = self.users_error.clone();
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.database_users_failed",
                            &[("database", &db), ("error", &error)],
                        ),
                    );
                } else {
                    let keep = self.pending_select_user.take().or_else(|| {
                        self.selected_user
                            .and_then(|i| self.users.get(i))
                            .map(|u| u.name.clone())
                    });
                    self.users = parse_users(outcome.set(0));
                    self.db_roles = parse_names(outcome.set(1));
                    self.role_members = BTreeMap::new();
                    for row in outcome.set(2) {
                        let role = row.first().map(cell_text).unwrap_or_default();
                        let member = row.get(1).map(cell_text).unwrap_or_default();
                        if !role.is_empty() && !member.is_empty() {
                            self.role_members.entry(role).or_default().insert(member);
                        }
                    }
                    // Re-resolve the selection by name (indexes shift across
                    // reloads) and refresh the baseline from server truth, so
                    // the next diff can't re-run statements a partial apply
                    // already committed. Form inputs stay as the user typed
                    // them; only the baseline moves.
                    self.selected_user =
                        keep.and_then(|name| self.users.iter().position(|u| u.name == name));
                    // The read-only "Mapped login" line reads user_login; take
                    // it from the server row, not whatever the picker held.
                    if let Some(user) = self.selected_user.and_then(|i| self.users.get(i)) {
                        self.user_login = user.login.clone().unwrap_or_default();
                    }
                    self.user_baseline =
                        self.selected_user
                            .and_then(|i| self.users.get(i))
                            .map(|user| {
                                let roles: BTreeSet<String> = self
                                    .role_members
                                    .iter()
                                    .filter(|(_, members)| members.contains(&user.name))
                                    .map(|(role, _)| role.clone())
                                    .collect();
                                UserBaseline {
                                    default_schema: user.default_schema.clone(),
                                    roles,
                                    password_target: UserPasswordTarget::for_user(
                                        user,
                                        &self.logins,
                                    ),
                                }
                            });
                    self.users_state = LoadState::Ready;
                    self.users_error.clear();
                }
            }
            QueryKind::PgCatalog => {
                if let Some(error) = outcome.error {
                    // Oracle: ORA-00942 / ORA-01031 on the DBA_* views means
                    // the login has no dictionary access — retry with the
                    // USER_* / ALL_* fallbacks, which show only its own
                    // privileges (announced in the status line).
                    if matches!(self.dialect, Some(SecurityDialect::Oracle))
                        && !self.oracle_compat
                        && (error.contains("ORA-00942") || error.contains("ORA-01031"))
                    {
                        self.oracle_compat = true;
                        self.load_catalog(cx);
                        return;
                    }
                    self.catalog_state = LoadState::Failed;
                    self.catalog_error = one_line(&error);
                    let error = self.catalog_error.clone();
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.roles_failed",
                            &[("error", &error)],
                        ),
                    );
                } else {
                    self.pg_all_roles = pg_model::parse_roles(outcome.set(0))
                        .into_iter()
                        .map(|r| r.name)
                        .collect();
                    let members = pg_model::parse_role_members(outcome.set(1));
                    self.pg_role_members = BTreeMap::new();
                    for (role, member, _) in members {
                        self.pg_role_members.entry(role).or_default().insert(member);
                    }
                    self.pg_connect_pairs = pg_model::parse_pairs(outcome.set(2));
                    self.pg_schema_pairs = pg_model::parse_pairs(outcome.set(3));
                    self.pg_tables = pg_model::parse_pairs(outcome.set(4));
                    let keep = self.pending_select_pg_role.take();
                    self.pg_roles = pg_model::parse_roles(outcome.set(0));
                    if let Some(name) = keep
                        && let Some(idx) = self.pg_roles.iter().position(|r| r.name == name)
                    {
                        self.selected_pg_role = Some(idx);
                        self.load_pg_role_detail(&name, cx);
                    }
                    self.databases = self
                        .pg_connect_pairs
                        .iter()
                        .map(|(db, _)| db.clone())
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    self.catalog_state = LoadState::Ready;
                    if self.oracle_compat {
                        self.set_status(
                            StatusKind::Idle,
                            crate::localization::text(
                                "explorer.security.load.oracle_compat_status",
                            ),
                        );
                    } else if matches!(self.status_kind, StatusKind::Busy) {
                        self.set_status(
                            StatusKind::Idle,
                            crate::localization::text("explorer.security.role_catalog_ready"),
                        );
                    }
                }
            }
            QueryKind::PgRoleDetail(name) => {
                let current = self
                    .selected_pg_role
                    .and_then(|i| self.pg_roles.get(i))
                    .map(|r| r.name.clone());
                if current.as_deref() != Some(name.as_str()) {
                    return;
                }
                if let Some(error) = outcome.error {
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.role_detail_failed",
                            &[("name", &name), ("error", &error)],
                        ),
                    );
                } else {
                    let table_grants = pg_model::parse_table_grants(outcome.set(0));
                    let member_of: BTreeSet<String> = self
                        .pg_role_members
                        .iter()
                        .filter(|(_, members)| members.contains(&name))
                        .map(|(role, _)| role.clone())
                        .collect();
                    let connect_dbs: BTreeSet<String> = self
                        .pg_connect_pairs
                        .iter()
                        .filter(|(_, role)| role == &name)
                        .map(|(db, _)| db.clone())
                        .collect();
                    let schema_usage: BTreeSet<String> = self
                        .pg_schema_pairs
                        .iter()
                        .filter(|(_, role)| role == &name)
                        .map(|(schema, _)| schema.clone())
                        .collect();
                    self.pg_role_baseline = self
                        .selected_pg_role
                        .and_then(|i| self.pg_roles.get(i))
                        .map(|r| pg_model::PgRoleBaseline {
                            name: r.name.clone(),
                            opts: r.opts(),
                            member_of,
                            connect_dbs,
                            schema_usage,
                            table_grants,
                        });
                    if let Some(base) = &self.pg_role_baseline {
                        self.pg_role_opts = base.opts;
                        self.pg_member_of = base.member_of.clone();
                        self.pg_connect_dbs = base.connect_dbs.clone();
                        self.pg_schema_usage = base.schema_usage.clone();
                        self.pg_table_grants = base.table_grants.clone();
                    }
                }
            }
            QueryKind::MyUsers => {
                if let Some(error) = outcome.error {
                    if matches!(self.dialect, Some(SecurityDialect::MariaDb)) {
                        let admin = &*self.services.admin_tools;
                        if let Some(sql) =
                            admin.security_query_for(self.profile.database_type, "users-compat")
                        {
                            self.run_query(QueryKind::MyUsersCompat, sql, cx);
                            return;
                        }
                    }
                    self.catalog_state = LoadState::Failed;
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.accounts_failed",
                            &[("error", &error)],
                        ),
                    );
                } else {
                    self.my_databases = parse_names(outcome.set(1));
                    if self.my_roles_supported {
                        self.my_role_names = parse_names(outcome.set(2)).into_iter().collect();
                    }
                    let keep = self.pending_select_my_user.take();
                    self.my_users = my_model::parse_users(outcome.set(0), &self.my_role_names);
                    if let Some((user, host)) = keep
                        && let Some(idx) = self
                            .my_users
                            .iter()
                            .position(|u| u.user == user && u.host == host)
                    {
                        self.selected_my_user = Some(idx);
                        self.load_my_grants(&user, &host, cx);
                    }
                    self.catalog_state = LoadState::Ready;
                    self.selects_dirty = true;
                    if matches!(self.status_kind, StatusKind::Busy) {
                        self.set_status(
                            StatusKind::Idle,
                            crate::localization::text("explorer.security.account_catalog_ready"),
                        );
                    }
                }
            }
            QueryKind::MyUsersCompat => {
                if let Some(error) = outcome.error {
                    self.catalog_state = LoadState::Failed;
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.accounts_failed",
                            &[("error", &error)],
                        ),
                    );
                } else {
                    self.my_users = my_model::parse_users(outcome.set(0), &self.my_role_names);
                    self.catalog_state = LoadState::Ready;
                }
            }
            QueryKind::ChCatalog => {
                if let Some(error) = outcome.error {
                    self.catalog_state = LoadState::Failed;
                    self.catalog_error = one_line(&error);
                    let error = self.catalog_error.clone();
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.users_roles_failed",
                            &[("error", &error)],
                        ),
                    );
                } else {
                    let keep_user = self
                        .selected_ch_user
                        .and_then(|i| self.ch_users.get(i))
                        .map(|u| u.name.clone());
                    let keep_role = self
                        .selected_ch_role
                        .and_then(|i| self.ch_roles.get(i))
                        .map(|r| r.name.clone());
                    self.ch_users = ch_model::parse_users(outcome.set(0));
                    self.ch_roles = ch_model::parse_roles(outcome.set(1));
                    self.ch_grants = ch_model::parse_grants(outcome.set(2));
                    self.ch_role_grants = ch_model::parse_role_grants(outcome.set(3));
                    self.ch_profiles = outcome
                        .set(4)
                        .iter()
                        .filter_map(|row| {
                            let name = cell_text(row.first()?);
                            if name.is_empty() {
                                return None;
                            }
                            let applies_to = if row.get(3).map(cell_bool).unwrap_or(false) {
                                "everyone".to_string()
                            } else {
                                row.get(4).map(cell_text).unwrap_or_default()
                            };
                            Some((name, applies_to))
                        })
                        .collect();
                    self.selected_ch_user =
                        keep_user.and_then(|n| self.ch_users.iter().position(|u| u.name == n));
                    self.selected_ch_role =
                        keep_role.and_then(|n| self.ch_roles.iter().position(|r| r.name == n));
                    self.ch_detail = None;
                    self.catalog_state = LoadState::Ready;
                    self.catalog_error.clear();
                    if matches!(self.status_kind, StatusKind::Busy) {
                        self.set_status(
                            StatusKind::Idle,
                            crate::localization::text(CH_READ_ONLY_STATUS),
                        );
                    }
                    if let Some((name, is_role)) = self.selected_ch_principal() {
                        self.load_ch_detail(&name, is_role, cx);
                    }
                }
            }
            QueryKind::ChDetail(name, is_role) => {
                if self.selected_ch_principal() != Some((name.clone(), is_role)) {
                    return;
                }
                let from_catalog = || {
                    ch_model::script_from_catalog(
                        &name,
                        is_role,
                        &self.ch_grants,
                        &self.ch_role_grants,
                    )
                };
                let script = match outcome.error {
                    Some(error) => {
                        let script = from_catalog();
                        let error = one_line(&error);
                        self.set_status(
                            StatusKind::Idle,
                            localized_format(
                                "explorer.security.load.ch_show_denied",
                                &[("error", &error)],
                            ),
                        );
                        script
                    }
                    None => {
                        let lines: Vec<String> = outcome
                            .set(0)
                            .iter()
                            .chain(outcome.set(1).iter())
                            .filter_map(|row| row.first().map(cell_text))
                            .filter(|line| !line.trim().is_empty())
                            .map(|line| format!("{};", line.trim_end_matches(';')))
                            .collect();
                        if lines.is_empty() {
                            from_catalog()
                        } else {
                            lines.join("\n")
                        }
                    }
                };
                self.ch_detail = Some((name, script));
            }
            QueryKind::MyGrants(user, host) => {
                let current = self
                    .selected_my_user
                    .and_then(|i| self.my_users.get(i))
                    .map(|u| (u.user.clone(), u.host.clone()));
                if current.as_ref() != Some(&(user.clone(), host.clone())) {
                    return;
                }
                if let Some(error) = outcome.error {
                    let error = one_line(&error);
                    self.set_status(
                        StatusKind::Error,
                        localized_format(
                            "explorer.security.load.grants_failed",
                            &[("user", &user), ("host", &host), ("error", &error)],
                        ),
                    );
                } else {
                    let lines: Vec<String> = outcome
                        .set(0)
                        .iter()
                        .filter_map(|row| row.first().map(cell_text))
                        .filter(|s| !s.is_empty())
                        .collect();
                    let grants = my_model::parse_show_grants(&lines);
                    let locked = self
                        .selected_my_user
                        .and_then(|i| self.my_users.get(i))
                        .map(|u| u.locked)
                        .unwrap_or(false);
                    let baseline = my_model::baseline_from_grants(&user, &host, locked, &grants);
                    self.my_user_baseline = Some(baseline.clone());
                    self.my_global = baseline.global;
                    self.my_database_privs = baseline.databases;
                    self.my_table_privs = baseline.tables;
                    self.my_assigned_roles = baseline.roles;
                    self.my_user_locked = locked;
                }
            }
        }
        cx.notify();
    }

    fn load_pg_role_detail(&mut self, role: &str, cx: &mut gpui::Context<Self>) {
        let admin = &*self.services.admin_tools;
        let sql = if self.oracle_compat {
            Some(sqlly_services::admin::security_sql::oracle::table_grants_query_compat(role))
        } else {
            admin.security_table_grants_query_for(self.profile.database_type, role)
        };
        if let Some(sql) = sql {
            self.run_query(QueryKind::PgRoleDetail(role.to_string()), sql, cx);
        }
    }

    /// Which engine's DDL the Roles tab scripts (PostgreSQL by default).
    fn role_script_dialect(&self) -> pg_model::RoleScriptDialect {
        match self.dialect {
            Some(SecurityDialect::Oracle) => pg_model::RoleScriptDialect::Oracle,
            _ => pg_model::RoleScriptDialect::Postgres,
        }
    }

    fn load_my_grants(&mut self, user: &str, host: &str, cx: &mut gpui::Context<Self>) {
        let admin = &*self.services.admin_tools;
        if let Some(sql) =
            admin.security_show_grants_sql_for(self.profile.database_type, user, host)
        {
            self.run_query(
                QueryKind::MyGrants(user.to_string(), host.to_string()),
                sql,
                cx,
            );
        }
    }

    fn database_type(&self) -> DatabaseType {
        self.profile.database_type
    }

    // ── ClickHouse (read-only) ──────────────────────────────────────────

    /// The selected ClickHouse principal on the active tab, as (name, is_role).
    fn selected_ch_principal(&self) -> Option<(String, bool)> {
        match self.tab {
            SecurityTab::ChUsers => self
                .selected_ch_user
                .and_then(|i| self.ch_users.get(i))
                .map(|u| (u.name.clone(), false)),
            SecurityTab::ChRoles => self
                .selected_ch_role
                .and_then(|i| self.ch_roles.get(i))
                .map(|r| (r.name.clone(), true)),
            _ => None,
        }
    }

    fn load_ch_detail(&mut self, name: &str, is_role: bool, cx: &mut gpui::Context<Self>) {
        let create = if is_role {
            ch_sql::show_create_role_sql(name)
        } else {
            ch_sql::show_create_user_sql(name)
        };
        let sql = format!("{create};\n{}", ch_sql::show_grants_sql(name));
        self.run_query(QueryKind::ChDetail(name.to_string(), is_role), sql, cx);
    }

    fn select_ch_principal(
        &mut self,
        idx: Option<usize>,
        is_role: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        if is_role {
            self.selected_ch_role = idx;
        } else {
            self.selected_ch_user = idx;
        }
        self.ch_detail = None;
        if let Some((name, is_role)) = self.selected_ch_principal() {
            self.load_ch_detail(&name, is_role, cx);
        }
        cx.notify();
    }

    fn script_ch_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let Some((name, script)) = self.ch_detail.clone() else {
            self.set_status(
                StatusKind::Idle,
                crate::localization::text("explorer.security.clickhouse.selection_required"),
            );
            cx.notify();
            return;
        };
        let sql = format!(
            "-- {name}: CREATE and GRANT statements from the ClickHouse catalog. Review before running.\n{script}\n"
        );
        cx.emit(SecurityDialogEvent::ScriptToEditor { sql });
    }

    // ── Selection and form ──────────────────────────────────────────────

    fn select_login(
        &mut self,
        idx: Option<usize>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.selected_login == idx && !self.editing_new_login {
            return;
        }
        if self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.selected_login = idx;
        self.editing_new_login = false;
        self.login_baseline = None;
        self.login_detail_state = LoadState::Idle;
        if let Some(login) = idx.and_then(|i| self.logins.get(i)).cloned() {
            self.login_name_input.update(cx, |state, cx| {
                state.set_value(login.name.clone(), window, cx);
            });
            self.clear_password_inputs(window, cx);
            self.login_auth = if login.kind == "SQL_LOGIN" {
                LoginAuth::Sql
            } else {
                LoginAuth::Windows
            };
            self.load_login_detail(&login.name, cx);
        }
        cx.notify();
    }

    fn begin_new_login(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.catalog_state != LoadState::Ready || self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_login = true;
        self.selected_login = None;
        self.login_baseline = None;
        self.login_detail_state = LoadState::Ready;
        self.login_auth = LoginAuth::Sql;
        self.login_enabled = true;
        self.login_server_roles = BTreeSet::new();
        self.login_db_access = BTreeSet::new();
        self.login_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.clear_password_inputs(window, cx);
        window.focus(&self.login_name_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn select_user(
        &mut self,
        idx: Option<usize>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.selected_user == idx && !self.editing_new_user {
            return;
        }
        if self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.selected_user = idx;
        self.editing_new_user = false;
        self.user_auth = UserAuth::default();
        self.clear_password_inputs(window, cx);
        if let Some(user) = idx.and_then(|i| self.users.get(i)).cloned() {
            self.user_name_input.update(cx, |state, cx| {
                state.set_value(user.name.clone(), window, cx);
            });
            self.user_schema_input.update(cx, |state, cx| {
                state.set_value(
                    user.default_schema.clone().unwrap_or_else(|| "dbo".into()),
                    window,
                    cx,
                );
            });
            self.user_login = user.login.clone().unwrap_or_default();
            let roles: BTreeSet<String> = self
                .role_members
                .iter()
                .filter(|(_, members)| members.contains(&user.name))
                .map(|(role, _)| role.clone())
                .collect();
            self.user_baseline = Some(UserBaseline {
                default_schema: user.default_schema.clone(),
                roles: roles.clone(),
                password_target: UserPasswordTarget::for_user(&user, &self.logins),
            });
            self.user_roles = roles;
        } else {
            self.user_baseline = None;
        }
        cx.notify();
    }

    fn begin_new_user(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.users_state != LoadState::Ready || self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_user = true;
        self.selected_user = None;
        self.user_baseline = None;
        self.user_roles = BTreeSet::new();
        // With no logins to map, the useful default is creating one.
        self.user_auth = if self.logins.is_empty() {
            UserAuth::NewLogin
        } else {
            UserAuth::ExistingLogin
        };
        self.clear_password_inputs(window, cx);
        self.user_login = self
            .logins
            .first()
            .map(|l| l.name.clone())
            .unwrap_or_default();
        self.selects_dirty = true;
        self.user_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.user_schema_input
            .update(cx, |state, cx| state.set_value("dbo", window, cx));
        window.focus(&self.user_name_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn begin_new_pg_role(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.catalog_state != LoadState::Ready || self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_pg_role = true;
        self.selected_pg_role = None;
        self.pg_role_baseline = None;
        self.pg_role_opts = pg_model::RoleOptions {
            login: true,
            inherit: true,
            ..pg_model::RoleOptions::default()
        };
        self.pg_member_of.clear();
        self.pg_connect_dbs.clear();
        self.pg_schema_usage.clear();
        self.pg_table_grants.clear();
        self.clear_password_inputs(window, cx);
        self.pg_role_name_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        window.focus(&self.pg_role_name_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn select_pg_role(
        &mut self,
        idx: Option<usize>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_pg_role = false;
        self.selected_pg_role = idx;
        // The baseline arrives with the detail load; until then there is
        // nothing to diff against (never the previous role's baseline).
        self.pg_role_baseline = None;
        self.clear_password_inputs(window, cx);
        let role = idx.and_then(|i| self.pg_roles.get(i)).cloned();
        if let Some(role) = role {
            self.pg_role_opts = role.opts();
            self.pg_role_name_input.update(cx, |state, cx| {
                state.set_value(role.name.clone(), window, cx);
            });
            self.load_pg_role_detail(&role.name, cx);
        }
        cx.notify();
    }

    fn begin_new_my_user(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.catalog_state != LoadState::Ready || self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_my_user = true;
        self.selected_my_user = None;
        self.my_user_baseline = None;
        self.my_user_locked = false;
        self.my_global.clear();
        self.my_database_privs.clear();
        self.my_table_privs.clear();
        self.my_assigned_roles.clear();
        self.clear_password_inputs(window, cx);
        self.my_user_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.my_host_input
            .update(cx, |state, cx| state.set_value("%", window, cx));
        window.focus(&self.my_user_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    fn select_my_user(
        &mut self,
        idx: Option<usize>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.applying {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.editing_new_my_user = false;
        self.selected_my_user = idx;
        self.my_user_baseline = None;
        self.clear_password_inputs(window, cx);
        if let Some(user) = idx.and_then(|i| self.my_users.get(i)).cloned() {
            self.my_user_input.update(cx, |state, cx| {
                state.set_value(user.user.clone(), window, cx);
            });
            self.my_host_input.update(cx, |state, cx| {
                state.set_value(user.host.clone(), window, cx);
            });
            self.my_user_locked = user.locked;
            self.load_my_grants(&user.user, &user.host, cx);
        }
        cx.notify();
    }

    /// Selecting a different principal drops unapplied edits. Say so rather
    /// than letting a half-typed password vanish silently.
    fn discard_notice(&mut self, cx: &mut gpui::Context<Self>) {
        if self.form_is_dirty(cx) {
            self.set_status(
                StatusKind::Idle,
                crate::localization::text("explorer.security.editor.changes_discarded"),
            );
        }
    }

    /// Passwords never follow the selection: a half-typed secret for one
    /// principal must not be applied to the next.
    fn clear_password_inputs(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        for input in [
            &self.login_password_input,
            &self.login_password_confirm_input,
            &self.user_password_input,
            &self.user_password_confirm_input,
            &self.pg_role_password_input,
            &self.pg_role_password_confirm_input,
            &self.my_password_input,
            &self.my_password_confirm_input,
        ] {
            input.update(cx, |state, cx| state.set_value("", window, cx));
        }
    }

    fn login_form(&self, cx: &gpui::Context<Self>) -> LoginForm {
        LoginForm {
            name: self.login_name_input.read(cx).value().to_string(),
            auth: self.login_auth,
            password: self.login_password_input.read(cx).value().to_string(),
            password_confirm: self
                .login_password_confirm_input
                .read(cx)
                .value()
                .to_string(),
            enabled: self.login_enabled,
            server_roles: self.login_server_roles.clone(),
            db_access: self.login_db_access.clone(),
        }
    }

    fn user_form(&self, cx: &gpui::Context<Self>) -> UserForm {
        UserForm {
            name: self.user_name_input.read(cx).value().to_string(),
            auth: self.user_auth,
            password: self.user_password_input.read(cx).value().to_string(),
            password_confirm: self
                .user_password_confirm_input
                .read(cx)
                .value()
                .to_string(),
            login: self.user_login.clone(),
            default_schema: self.user_schema_input.read(cx).value().to_string(),
            roles: self.user_roles.clone(),
        }
    }

    fn form_is_dirty(&self, cx: &gpui::Context<Self>) -> bool {
        match self.tab {
            // The ClickHouse view has no editable form.
            SecurityTab::ChUsers | SecurityTab::ChRoles => false,
            // A new principal counts as dirty once anything is typed, even
            // before it validates into statements.
            SecurityTab::Logins if self.editing_new_login => {
                let form = self.login_form(cx);
                !form.name.trim().is_empty()
                    || !form.password.is_empty()
                    || !form.server_roles.is_empty()
                    || !form.db_access.is_empty()
            }
            SecurityTab::Users if self.editing_new_user => {
                let form = self.user_form(cx);
                !form.name.trim().is_empty() || !form.password.is_empty()
            }
            SecurityTab::Roles if self.editing_new_pg_role => {
                !self.pg_role_form(cx).name.trim().is_empty()
            }
            SecurityTab::Accounts | SecurityTab::MyRoles if self.editing_new_my_user => {
                !self.my_user_form(cx).user.trim().is_empty()
            }
            _ => !self.form_plan(cx, false).statements.is_empty(),
        }
    }

    /// Whether the editor is showing a principal (existing or new).
    fn has_subject(&self) -> bool {
        match self.tab {
            SecurityTab::Logins => self.editing_new_login || self.selected_login.is_some(),
            SecurityTab::Users => self.editing_new_user || self.selected_user.is_some(),
            SecurityTab::Roles => self.editing_new_pg_role || self.selected_pg_role.is_some(),
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                self.editing_new_my_user || self.selected_my_user.is_some()
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => self.selected_ch_principal().is_some(),
        }
    }

    /// Whether the editor is on a new principal rather than an existing one.
    fn is_new_subject(&self) -> bool {
        match self.tab {
            SecurityTab::Logins => self.editing_new_login,
            SecurityTab::Users => self.editing_new_user,
            SecurityTab::Roles => self.editing_new_pg_role,
            SecurityTab::Accounts | SecurityTab::MyRoles => self.editing_new_my_user,
            SecurityTab::ChUsers | SecurityTab::ChRoles => false,
        }
    }

    /// What Apply would run right now, and what blocks it. With a drop armed
    /// that is the DROP batch; otherwise the form's diff. `masked` swaps every
    /// password for a stand-in (the on-screen copy); validation always sees
    /// the real form.
    fn plan(&self, cx: &gpui::Context<Self>, masked: bool) -> SecurityPlan {
        if self.pending_drop {
            return SecurityPlan {
                statements: self.drop_statements(),
                errors: Vec::new(),
            };
        }
        self.form_plan(cx, masked)
    }

    fn form_plan(&self, cx: &gpui::Context<Self>, masked: bool) -> SecurityPlan {
        let admin = &*self.services.admin_tools;
        match self.tab {
            SecurityTab::Logins => {
                let form = self.login_form(cx);
                let existing: BTreeSet<String> =
                    self.logins.iter().map(|l| l.name.clone()).collect();
                let errors = validate_login(self.editing_new_login, &existing, &form);
                let statements = if self.editing_new_login || self.login_baseline.is_some() {
                    let form = if masked {
                        LoginForm {
                            password: mask_password(&form.password),
                            password_confirm: mask_password(&form.password_confirm),
                            ..form
                        }
                    } else {
                        form
                    };
                    diff_login(admin, self.login_baseline.as_ref(), &form)
                } else {
                    Vec::new()
                };
                SecurityPlan { statements, errors }
            }
            SecurityTab::Users => {
                let form = self.user_form(cx);
                let existing: BTreeSet<String> =
                    self.users.iter().map(|u| u.name.clone()).collect();
                let known: Vec<String> = self.logins.iter().map(|l| l.name.clone()).collect();
                let errors = validate_user_with_target(
                    self.editing_new_user,
                    &existing,
                    &known,
                    &form,
                    self.user_baseline.as_ref().map(|b| &b.password_target),
                );
                let db = self.users_db.clone().unwrap_or_default();
                let statements = if self.editing_new_user || self.user_baseline.is_some() {
                    let form = if masked {
                        UserForm {
                            password: mask_password(&form.password),
                            password_confirm: mask_password(&form.password_confirm),
                            ..form
                        }
                    } else {
                        form
                    };
                    diff_user(admin, self.user_baseline.as_ref(), &db, &form)
                } else {
                    Vec::new()
                };
                SecurityPlan { statements, errors }
            }
            SecurityTab::Roles => {
                let form = self.pg_role_form(cx);
                let existing: BTreeSet<String> =
                    self.pg_roles.iter().map(|r| r.name.clone()).collect();
                let errors = pg_model::validate(self.editing_new_pg_role, &existing, &form);
                let statements = if self.editing_new_pg_role || self.pg_role_baseline.is_some() {
                    let form = if masked {
                        pg_model::PgRoleForm {
                            password: mask_password(&form.password),
                            confirm: mask_password(&form.confirm),
                            ..form
                        }
                    } else {
                        form
                    };
                    pg_model::diff_for(
                        self.role_script_dialect(),
                        self.pg_role_baseline.as_ref(),
                        &form,
                    )
                } else {
                    Vec::new()
                };
                SecurityPlan { statements, errors }
            }
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let form = self.my_user_form(cx);
                let existing: BTreeSet<(String, String)> = self
                    .my_users
                    .iter()
                    .map(|u| (u.user.clone(), u.host.clone()))
                    .collect();
                let errors = my_model::validate(self.editing_new_my_user, &existing, &form);
                let statements = if self.editing_new_my_user || self.my_user_baseline.is_some() {
                    let form = if masked {
                        my_model::MyUserForm {
                            password: mask_password(&form.password),
                            confirm: mask_password(&form.confirm),
                            ..form
                        }
                    } else {
                        form
                    };
                    my_model::diff(
                        self.my_user_baseline.as_ref(),
                        &form,
                        self.my_roles_supported,
                    )
                } else {
                    Vec::new()
                };
                SecurityPlan { statements, errors }
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => SecurityPlan {
                statements: Vec::new(),
                errors: vec![crate::localization::text(
                    "explorer.security.clickhouse.apply_unavailable",
                )],
            },
        }
    }

    /// The DROP batch for the selected principal. SQL Server logins take
    /// their mapped database users with them first, so no orphans are left.
    fn drop_statements(&self) -> Vec<PlannedStatement> {
        let admin = &*self.services.admin_tools;
        match self.tab {
            SecurityTab::Logins => {
                let Some(login) = self.selected_login.and_then(|i| self.logins.get(i)) else {
                    return Vec::new();
                };
                let mut statements = Vec::new();
                if let Some(base) = &self.login_baseline {
                    for (db, user) in &base.db_users {
                        statements.push(PlannedStatement::in_db(
                            db,
                            admin.security_drop_user_sql(user),
                            format!("drop user {user} from {db}"),
                        ));
                    }
                }
                statements.push(PlannedStatement::server(
                    admin.security_drop_login_sql(&login.name),
                    format!("drop login {}", login.name),
                ));
                statements
            }
            SecurityTab::Users => {
                let (Some(user), Some(db)) = (
                    self.selected_user.and_then(|i| self.users.get(i)),
                    self.users_db.as_deref(),
                ) else {
                    return Vec::new();
                };
                vec![PlannedStatement::in_db(
                    db,
                    admin.security_drop_user_sql(&user.name),
                    format!("drop user {} from {db}", user.name),
                )]
            }
            SecurityTab::Roles => {
                let Some(role) = self.selected_pg_role.and_then(|i| self.pg_roles.get(i)) else {
                    return Vec::new();
                };
                let sql = match self.role_script_dialect() {
                    pg_model::RoleScriptDialect::Postgres => {
                        sqlly_services::admin::security_sql::postgres::drop_role_sql(&role.name)
                    }
                    pg_model::RoleScriptDialect::Oracle if role.login => {
                        sqlly_services::admin::security_sql::oracle::drop_user_sql(
                            &role.name, false,
                        )
                    }
                    pg_model::RoleScriptDialect::Oracle => {
                        sqlly_services::admin::security_sql::oracle::drop_role_sql(&role.name)
                    }
                };
                vec![PlannedStatement::server(sql, format!("drop {}", role.name))]
            }
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let Some(user) = self.selected_my_user.and_then(|i| self.my_users.get(i)) else {
                    return Vec::new();
                };
                use sqlly_services::admin::security_sql::mysql;
                let sql = if user.is_role {
                    mysql::drop_role_sql(&user.user)
                } else {
                    mysql::drop_user_sql(&user.user, &user.host)
                };
                vec![PlannedStatement::server(
                    sql,
                    format!("drop {}@{}", user.user, user.host),
                )]
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => Vec::new(),
        }
    }

    fn pg_role_form(&self, cx: &gpui::Context<Self>) -> pg_model::PgRoleForm {
        pg_model::PgRoleForm {
            name: self.pg_role_name_input.read(cx).value().to_string(),
            password: self.pg_role_password_input.read(cx).value().to_string(),
            confirm: self
                .pg_role_password_confirm_input
                .read(cx)
                .value()
                .to_string(),
            opts: self.pg_role_opts,
            member_of: self.pg_member_of.clone(),
            connect_dbs: self.pg_connect_dbs.clone(),
            schema_usage: self.pg_schema_usage.clone(),
            table_grants: self.pg_table_grants.clone(),
        }
    }

    fn my_user_form(&self, cx: &gpui::Context<Self>) -> my_model::MyUserForm {
        my_model::MyUserForm {
            user: self.my_user_input.read(cx).value().to_string(),
            host: self.my_host_input.read(cx).value().to_string(),
            password: self.my_password_input.read(cx).value().to_string(),
            confirm: self.my_password_confirm_input.read(cx).value().to_string(),
            locked: self.my_user_locked,
            global: self.my_global.clone(),
            databases: self.my_database_privs.clone(),
            tables: self.my_table_privs.clone(),
            roles: self.my_assigned_roles.clone(),
        }
    }

    fn toggle_login_server_role(
        &mut self,
        role: &str,
        checked: bool,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.applying {
            return;
        }
        if checked {
            self.login_server_roles.insert(role.to_string());
        } else {
            self.login_server_roles.remove(role);
        }
        self.refresh_preview_status(cx);
    }

    fn toggle_login_db_access(&mut self, db: &str, checked: bool, cx: &mut gpui::Context<Self>) {
        if self.applying {
            return;
        }
        if checked {
            self.login_db_access.insert(db.to_string());
        } else {
            self.login_db_access.remove(db);
        }
        self.refresh_preview_status(cx);
    }

    fn toggle_user_role(&mut self, role: &str, checked: bool, cx: &mut gpui::Context<Self>) {
        if self.applying {
            return;
        }
        if checked {
            self.user_roles.insert(role.to_string());
        } else {
            self.user_roles.remove(role);
        }
        self.refresh_preview_status(cx);
    }

    // ── Apply ───────────────────────────────────────────────────────────

    fn apply(&mut self, cx: &mut gpui::Context<Self>) {
        if self.applying {
            return;
        }
        if self.production_read_only {
            self.set_status(
                StatusKind::Error,
                crate::localization::text("explorer.security.apply.production_read_only"),
            );
            cx.notify();
            return;
        }
        let SecurityPlan { statements, errors } = self.plan(cx, false);
        let dropping = self.pending_drop;
        if let Some(first) = errors.first() {
            self.set_status(StatusKind::Error, first.clone());
            cx.notify();
            return;
        }
        if statements.is_empty() {
            self.set_status(
                StatusKind::Idle,
                crate::localization::text("explorer.security.apply.no_changes"),
            );
            cx.notify();
            return;
        }

        let db = self.database_type();
        for statement in &statements {
            // Policy gate only: account DDL must not be wrapped in rollback even
            // when auto_wrap_rollback is set — discard the returned SQL.
            if let Err(err) = sqlly_intel::query::safety::apply_connection_policy_for(
                &self.profile,
                &statement.batch_text(&*self.services.admin_tools, db),
            ) {
                self.set_status(StatusKind::Error, err.to_string());
                cx.notify();
                return;
            }
        }

        self.applying = true;
        let total = statements.len();
        let count = total.to_string();
        self.set_status(
            StatusKind::Busy,
            localized_format("explorer.security.apply.in_progress", &[("count", &count)]),
        );
        cx.notify();

        let services = self.services.clone();
        let admin = self.services.admin_tools.clone();
        let profile = self.profile.clone();
        let db_type = self.database_type();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(usize, Option<String>)>(1);
        cx.background_spawn(async move {
            crate::tokio_handle()
                .spawn(async move {
                    let mut failed = None;
                    for (i, statement) in statements.iter().enumerate() {
                        let outcome = execute_collect(
                            &services,
                            &profile,
                            &statement.batch_text(&*admin, db_type),
                        )
                        .await;
                        if let Some(error) = outcome.error {
                            failed = Some((i, Some(error)));
                            break;
                        }
                    }
                    let result = failed.unwrap_or((statements.len(), None));
                    let _ = tx.send(result).await;
                })
                .await
                .ok();
        })
        .detach();

        let async_cx = cx.to_async();
        let weak_self = cx.weak_entity();
        let tab = self.tab;
        // Remember which principal the apply was for, so the post-apply reload
        // can re-select it by name (a brand-new principal has no selection yet).
        let applied_login = match self.tab {
            SecurityTab::Logins => {
                let name = self.login_name_input.read(cx).value().trim().to_string();
                (!name.is_empty()).then_some(name)
            }
            _ => None,
        };
        let applied_user = match self.tab {
            SecurityTab::Users => {
                let name = self.user_name_input.read(cx).value().trim().to_string();
                (!name.is_empty()).then_some(name)
            }
            _ => None,
        };
        let applied_pg_role = match self.tab {
            SecurityTab::Roles => {
                let name = self.pg_role_name_input.read(cx).value().trim().to_string();
                (!name.is_empty()).then_some(name)
            }
            _ => None,
        };
        let applied_my_user = match self.tab {
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let user = self.my_user_input.read(cx).value().trim().to_string();
                let host = self.my_host_input.read(cx).value().trim().to_string();
                if user.is_empty() {
                    None
                } else {
                    Some((user, host))
                }
            }
            _ => None,
        };
        cx.spawn(async move |_this, _cx| {
            if let Some((done, error)) = rx.recv().await {
                let mut cx = async_cx.clone();
                weak_self
                    .update(&mut cx, |this, cx| {
                        this.applying = false;
                        this.pending_drop = false;
                        match error {
                            None => {
                                let count = done.to_string();
                                this.set_status(
                                    StatusKind::Success,
                                    if dropping {
                                        localized_format(
                                            "explorer.security.drop.completed",
                                            &[("count", &count)],
                                        )
                                    } else {
                                        localized_format(
                                            "explorer.security.apply.completed",
                                            &[("count", &count)],
                                        )
                                    },
                                );
                                this.editing_new_login = false;
                                this.editing_new_user = false;
                                this.editing_new_pg_role = false;
                                this.editing_new_my_user = false;
                                // The password was just set on the server; it
                                // should not linger in the dialog's fields.
                                // InputStates need a Window to mutate, so this
                                // is applied at the next render.
                                this.clear_passwords = true;
                                if dropping {
                                    this.selected_login = None;
                                    this.login_baseline = None;
                                    this.selected_user = None;
                                    this.user_baseline = None;
                                    this.selected_pg_role = None;
                                    this.pg_role_baseline = None;
                                    this.selected_my_user = None;
                                    this.my_user_baseline = None;
                                }
                            }
                            Some(message) => {
                                let statement = (done + 1).to_string();
                                let total = total.to_string();
                                let error = one_line(&message);
                                let applied = done.to_string();
                                this.set_status(
                                    StatusKind::Error,
                                    localized_format(
                                        "explorer.security.apply.statement_failed",
                                        &[
                                            ("statement", &statement),
                                            ("total", &total),
                                            ("error", &error),
                                            ("applied", &applied),
                                        ],
                                    ),
                                );
                            }
                        }
                        // Reload from the server so the lists show truth, not
                        // what we hope happened. The reload handlers re-resolve
                        // the selection from these names.
                        match tab {
                            // A dropped principal has nothing to re-select.
                            _ if dropping => {}
                            SecurityTab::Logins => this.pending_select_login = applied_login,
                            SecurityTab::Users => this.pending_select_user = applied_user,
                            SecurityTab::Roles => this.pending_select_pg_role = applied_pg_role,
                            SecurityTab::Accounts | SecurityTab::MyRoles => {
                                this.pending_select_my_user = applied_my_user;
                            }
                            // Nothing is applied for ClickHouse (scripted only).
                            SecurityTab::ChUsers | SecurityTab::ChRoles => {}
                        }
                        this.load_catalog(cx);
                        if tab == SecurityTab::Users {
                            this.load_users(cx);
                        }
                        cx.notify();
                    })
                    .ok();
            }
        })
        .detach();
    }

    fn on_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        let m = &event.keystroke.modifiers;
        // ⌘↩ applies from anywhere in the dialog, text fields included.
        if m.platform && !m.control && !m.alt && event.keystroke.key == "enter" {
            self.apply(cx);
            cx.stop_propagation();
            return;
        }
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        // Escape is the dialog chrome's: its card-level handler routes to
        // `close`, which holds the dialog open while a batch is applying.
        match event.keystroke.key.as_str() {
            // Arrow navigation belongs to the list; when a text field owns the
            // keyboard, up/down are the field's, not the list's.
            "up" if self.focus_handle.is_focused(window) => self.move_selection(-1, window, cx),
            "down" if self.focus_handle.is_focused(window) => self.move_selection(1, window, cx),
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize, window: &mut Window, cx: &mut gpui::Context<Self>) {
        match self.tab {
            SecurityTab::Logins => {
                if self.logins.is_empty() {
                    return;
                }
                let last = self.logins.len() - 1;
                let next = match self.selected_login {
                    None if delta > 0 => 0,
                    None => last,
                    Some(i) => (i as isize + delta).clamp(0, last as isize) as usize,
                };
                self.select_login(Some(next), window, cx);
            }
            SecurityTab::Users => {
                if self.users.is_empty() {
                    return;
                }
                let last = self.users.len() - 1;
                let next = match self.selected_user {
                    None if delta > 0 => 0,
                    None => last,
                    Some(i) => (i as isize + delta).clamp(0, last as isize) as usize,
                };
                self.select_user(Some(next), window, cx);
            }
            SecurityTab::Roles => {
                if self.pg_roles.is_empty() {
                    return;
                }
                let last = self.pg_roles.len() - 1;
                let next = match self.selected_pg_role {
                    None if delta > 0 => 0,
                    None => last,
                    Some(i) => (i as isize + delta).clamp(0, last as isize) as usize,
                };
                self.select_pg_role(Some(next), window, cx);
            }
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let users: Vec<usize> = self
                    .my_users
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| u.is_role == (self.tab == SecurityTab::MyRoles))
                    .map(|(i, _)| i)
                    .collect();
                if users.is_empty() {
                    return;
                }
                let pos = self
                    .selected_my_user
                    .and_then(|sel| users.iter().position(|&i| i == sel))
                    .unwrap_or(0);
                let next_pos = (pos as isize + delta).clamp(0, users.len() as isize - 1) as usize;
                self.select_my_user(Some(users[next_pos]), window, cx);
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => {
                let is_role = self.tab == SecurityTab::ChRoles;
                let len = if is_role {
                    self.ch_roles.len()
                } else {
                    self.ch_users.len()
                };
                if len == 0 {
                    return;
                }
                let current = if is_role {
                    self.selected_ch_role
                } else {
                    self.selected_ch_user
                };
                let next = match current {
                    None if delta > 0 => 0,
                    None => len - 1,
                    Some(i) => (i as isize + delta).clamp(0, len as isize - 1) as usize,
                };
                self.select_ch_principal(Some(next), is_role, cx);
            }
        }
    }
}

fn selected_index(index: Option<usize>) -> Option<gpui_component::IndexPath> {
    index.map(gpui_component::IndexPath::new)
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/// One row of the principal list.
struct PrincipalRow {
    idx: usize,
    name: String,
    /// Small tags after the name (kind, disabled, …) as (text, colour).
    tags: Vec<(String, Hsla)>,
}

type RowSelect =
    Rc<dyn Fn(&mut SecurityDialog, usize, &mut Window, &mut gpui::Context<SecurityDialog>)>;
type NewAction = Rc<dyn Fn(&mut SecurityDialog, &mut Window, &mut gpui::Context<SecurityDialog>)>;
/// A role attribute checkbox: (element id, label, checked, setter).
type RoleFlag = (
    &'static str,
    String,
    bool,
    fn(&mut pg_model::RoleOptions, bool),
);
type ChecklistToggle =
    Rc<dyn Fn(&mut SecurityDialog, &str, bool, &mut gpui::Context<SecurityDialog>)>;

impl Render for SecurityDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        if !self.focus_requested {
            self.focus_requested = true;
            window.focus(&self.focus_handle, cx);
        }

        // Input and Select states need `&mut Window` to change, so work queued
        // by async handlers (which have none) lands here.
        if self.clear_passwords {
            self.clear_passwords = false;
            self.clear_password_inputs(window, cx);
        }
        if self.selects_dirty {
            self.selects_dirty = false;
            self.sync_selects(window, cx);
        }

        let clickhouse = matches!(self.dialect, Some(SecurityDialect::ClickHouse));
        let plan = self.plan(cx, !self.show_sql_passwords);
        let statement_count = plan.statements.len();
        let sql_text = if clickhouse {
            self.ch_detail
                .as_ref()
                .map(|(_, script)| script.clone())
                .unwrap_or_default()
        } else {
            preview_body(
                &plan.statements,
                &*self.services.admin_tools,
                self.database_type(),
            )
        };
        if sql_text != self.sql_output_text {
            self.sql_output_text = sql_text.clone();
            self.sql_output
                .update(cx, |state, cx| state.set_value(sql_text, window, cx));
        }

        let ready = match self.tab {
            SecurityTab::Users => self.users_state == LoadState::Ready,
            _ => self.catalog_state == LoadState::Ready,
        };
        let apply_enabled = !clickhouse
            && plan.errors.is_empty()
            && statement_count > 0
            && !self.applying
            && ready
            && !self.production_read_only;
        // Problems are only worth showing once the user has started on the
        // form: a blank "New login" should not open with a wall of errors.
        let show_problems =
            !clickhouse && !self.pending_drop && self.has_subject() && self.form_is_dirty(cx);
        let problems = if show_problems {
            plan.errors.clone()
        } else {
            Vec::new()
        };

        let header = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .justify_between()
            .gap(px(Theme::scaled(8.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(Theme::scaled(6.0)))
                    .text_color(Theme::text_dim())
                    .child(self.profile.server.clone())
                    .child("·")
                    .child(self.profile.database.clone().unwrap_or_else(|| {
                        crate::localization::text("explorer.security.header.no_database")
                    }))
                    .when(self.production_read_only, |d| {
                        d.child(tag(
                            crate::localization::text("explorer.security.header.read_only"),
                            Theme::warning(),
                        ))
                    }),
            )
            .child(self.render_tab_strip(cx));

        let body = div()
            .flex()
            .flex_row()
            .gap(px(Theme::scaled(12.0)))
            .flex_1()
            .min_h_0()
            .child(self.render_list_panel(window, cx))
            .child(self.render_editor_card(cx));

        let content = div()
            .id("security-dialog-content")
            .key_context("SecurityDialog")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _ev, window, cx| {
                    // Mouse-down bubbles up from the clicked element, so a
                    // text field has already taken focus by the time this
                    // runs. Only claim focus for clicks on empty space;
                    // refocusing unconditionally stole it back from every
                    // input, which is why nothing could be typed into them.
                    if !this.focus_handle.contains_focused(window, cx) {
                        window.focus(&this.focus_handle, cx);
                    }
                    cx.stop_propagation();
                }),
            )
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(12.0)))
            .p(px(Theme::scaled(16.0)))
            .child(header)
            // A served page manages the real server through its proxy, so the
            // "illustrative" banner only belongs to the standalone preview.
            .children(if crate::browser_preview::served() {
                None
            } else {
                crate::browser_preview::preview_banner(crate::localization::text(
                    "explorer.security.preview.banner",
                ))
            })
            .child(body)
            .child(self.render_sql_card(statement_count, problems, clickhouse, cx))
            .child(self.render_status_line());

        let primary = if clickhouse {
            Button::new("security-script")
                .primary()
                .label(crate::localization::text(
                    "explorer.security.actions.script_to_editor",
                ))
                .disabled(self.ch_detail.is_none())
                .on_click(cx.listener(|this, _, _, cx| this.script_ch_to_editor(cx)))
        } else if self.pending_drop {
            Button::new("security-apply")
                .danger()
                .label(if self.applying {
                    crate::localization::text("explorer.security.drop.in_progress")
                } else {
                    let subject = self.subject_label();
                    localized_format("explorer.security.drop.button", &[("subject", &subject)])
                })
                .disabled(!apply_enabled)
                .on_click(cx.listener(|this, _, _, cx| this.apply(cx)))
        } else {
            Button::new("security-apply")
                .primary()
                .label(if self.applying {
                    crate::localization::text("explorer.security.apply.button_in_progress")
                } else if statement_count > 0 {
                    let count = statement_count.to_string();
                    localized_format(
                        "explorer.security.apply.button_with_count",
                        &[("count", &count)],
                    )
                } else {
                    crate::localization::text("explorer.security.apply.button")
                })
                .tooltip(crate::localization::text("explorer.security.apply.tooltip"))
                .disabled(!apply_enabled)
                .on_click(cx.listener(|this, _, _, cx| this.apply(cx)))
        };

        let reload_button = Button::new("security-reload")
            .ghost()
            .label(crate::localization::text(
                "explorer.security.actions.reload",
            ))
            .tooltip(crate::localization::text(
                "explorer.security.actions.reload_tooltip",
            ))
            .on_click(cx.listener(|this, _, _, cx| this.reload(cx)));

        Dialog::new("security-dialog", |this: &mut Self| &mut this.frame)
            .title(localized_format(
                "explorer.security.window.title",
                &[("connection", &self.profile.name)],
            ))
            .bare_body()
            .on_close(|this, _window, cx| this.close(cx))
            .child(content)
            .leading_button(reload_button)
            .button(primary)
            .build(self, window, cx)
    }
}

impl SecurityDialog {
    fn sync_selects(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let login_names: Vec<String> = self.logins.iter().map(|l| l.name.clone()).collect();
        let login_idx = if self.user_login.is_empty() {
            (!login_names.is_empty()).then_some(0)
        } else {
            login_names.iter().position(|n| n == &self.user_login)
        };
        self.user_login_select.update(cx, |state, cx| {
            state.set_items(login_names, window, cx);
            state.set_selected_index(selected_index(login_idx), window, cx);
        });
        let db_idx = self
            .users_db
            .as_ref()
            .and_then(|db| self.databases.iter().position(|d| d == db));
        self.users_db_select.update(cx, |state, cx| {
            state.set_items(self.databases.clone(), window, cx);
            state.set_selected_index(selected_index(db_idx), window, cx);
        });
        if self.my_selected_database.is_none() {
            self.my_selected_database = self.my_databases.first().cloned();
        }
        let my_idx = self
            .my_selected_database
            .as_ref()
            .and_then(|db| self.my_databases.iter().position(|d| d == db));
        self.my_db_select.update(cx, |state, cx| {
            state.set_items(self.my_databases.clone(), window, cx);
            state.set_selected_index(selected_index(my_idx), window, cx);
        });
    }

    fn reload(&mut self, cx: &mut gpui::Context<Self>) {
        if self.applying || self.catalog_state == LoadState::Loading {
            return;
        }
        self.pending_drop = false;
        self.load_catalog(cx);
        if self.tab == SecurityTab::Users && self.users_state != LoadState::Loading {
            self.load_users(cx);
        }
    }

    fn switch_tab(&mut self, tab: SecurityTab, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.applying || self.tab == tab {
            return;
        }
        self.discard_notice(cx);
        self.pending_drop = false;
        self.tab = tab;
        self.list_filter_input
            .update(cx, |state, cx| state.set_value("", window, cx));
        self.ensure_users_loaded(cx);
        if let Some((name, is_role)) = self.selected_ch_principal()
            && self.ch_detail.as_ref().map(|(n, _)| n) != Some(&name)
        {
            self.load_ch_detail(&name, is_role, cx);
        }
        cx.notify();
    }

    /// The Users tab lists one database's users; load them the first time
    /// the tab is shown (the database is known once the catalog is in).
    fn ensure_users_loaded(&mut self, cx: &mut gpui::Context<Self>) {
        if self.tab == SecurityTab::Users
            && self.users_state == LoadState::Idle
            && self.users_db.is_some()
        {
            self.load_users(cx);
        }
    }

    /// Short name of what the editor shows, for button labels.
    fn subject_label(&self) -> String {
        match self.tab {
            SecurityTab::Logins => self
                .selected_login
                .and_then(|i| self.logins.get(i))
                .map(|l| format!("login {}", l.name)),
            SecurityTab::Users => self
                .selected_user
                .and_then(|i| self.users.get(i))
                .map(|u| format!("user {}", u.name)),
            SecurityTab::Roles => self
                .selected_pg_role
                .and_then(|i| self.pg_roles.get(i))
                .map(|r| r.name.clone()),
            SecurityTab::Accounts | SecurityTab::MyRoles => self
                .selected_my_user
                .and_then(|i| self.my_users.get(i))
                .map(|u| format!("'{}'@'{}'", u.user, u.host)),
            SecurityTab::ChUsers | SecurityTab::ChRoles => None,
        }
        .unwrap_or_default()
    }

    fn arm_drop(&mut self, cx: &mut gpui::Context<Self>) {
        if self.applying || self.is_new_subject() || !self.has_subject() {
            return;
        }
        self.pending_drop = true;
        self.set_status(
            StatusKind::Idle,
            crate::localization::text("explorer.security.drop.review_warning"),
        );
        cx.notify();
    }

    fn cancel_drop(&mut self, cx: &mut gpui::Context<Self>) {
        self.pending_drop = false;
        self.set_status(
            StatusKind::Idle,
            crate::localization::text("explorer.security.drop_cancelled"),
        );
        cx.notify();
    }

    /// Throw away unapplied edits by re-selecting the principal from its
    /// loaded state (or clearing a new one).
    fn revert(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.applying {
            return;
        }
        match self.tab {
            SecurityTab::Logins => {
                if self.editing_new_login {
                    self.begin_new_login(window, cx);
                } else if let Some(idx) = self.selected_login {
                    self.selected_login = None;
                    self.select_login(Some(idx), window, cx);
                }
            }
            SecurityTab::Users => {
                if self.editing_new_user {
                    self.begin_new_user(window, cx);
                } else if let Some(idx) = self.selected_user {
                    self.selected_user = None;
                    self.select_user(Some(idx), window, cx);
                }
            }
            SecurityTab::Roles => {
                if self.editing_new_pg_role {
                    self.begin_new_pg_role(window, cx);
                } else {
                    self.select_pg_role(self.selected_pg_role, window, cx);
                }
            }
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                if self.editing_new_my_user {
                    self.begin_new_my_user(window, cx);
                } else {
                    self.select_my_user(self.selected_my_user, window, cx);
                }
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => {}
        }
        self.set_status(
            StatusKind::Idle,
            crate::localization::text("explorer.security.editor.reverted"),
        );
        cx.notify();
    }

    fn copy_all_sql(&mut self, cx: &mut gpui::Context<Self>) {
        let text = if matches!(self.dialect, Some(SecurityDialect::ClickHouse)) {
            self.ch_detail
                .as_ref()
                .map(|(_, script)| script.clone())
                .unwrap_or_default()
        } else {
            // Always the runnable SQL, real passwords included: the on-screen
            // mask is for shoulder-surfers, not for what gets pasted.
            preview_body(
                &self.plan(cx, false).statements,
                &*self.services.admin_tools,
                self.database_type(),
            )
        };
        if text.is_empty() {
            self.set_status(
                StatusKind::Idle,
                crate::localization::text("explorer.security.sql.nothing_to_copy"),
            );
        } else {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            self.set_status(
                StatusKind::Success,
                crate::localization::text("explorer.security.sql.copied"),
            );
        }
        cx.notify();
    }

    fn generate_password_into(
        &mut self,
        password: Entity<InputState>,
        confirm: Entity<InputState>,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) {
        if self.applying {
            return;
        }
        let generated = generate_password();
        password.update(cx, |state, cx| {
            state.set_value(generated.clone(), window, cx)
        });
        confirm.update(cx, |state, cx| {
            state.set_value(generated.clone(), window, cx)
        });
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(generated));
        self.set_status(
            StatusKind::Success,
            crate::localization::text("explorer.security.password.generated"),
        );
        cx.notify();
    }

    fn filter_text(&self, cx: &gpui::Context<Self>) -> String {
        self.list_filter_input
            .read(cx)
            .value()
            .trim()
            .to_lowercase()
    }

    // ── Tabs ────────────────────────────────────────────────────────────

    fn render_tab_strip(&self, cx: &mut gpui::Context<Self>) -> AnyElement {
        let tabs = tabs_for(self.dialect, self.my_roles_supported);
        if tabs.len() < 2 {
            return div().into_any_element();
        }
        let options: Vec<(SecurityTab, String)> = tabs
            .into_iter()
            .map(|tab| {
                let (label, count) = match tab {
                    SecurityTab::Logins => (
                        crate::localization::text("explorer.security.tabs.logins"),
                        Some(self.logins.len()),
                    ),
                    SecurityTab::Users => (
                        crate::localization::text("explorer.security.tabs.users"),
                        Some(self.users.len()),
                    ),
                    SecurityTab::Roles => (
                        crate::localization::text("explorer.security.tabs.roles"),
                        Some(self.pg_roles.len()),
                    ),
                    SecurityTab::Accounts => (
                        crate::localization::text("explorer.security.tabs.accounts"),
                        Some(self.my_users.iter().filter(|u| !u.is_role).count()),
                    ),
                    SecurityTab::MyRoles => (
                        crate::localization::text("explorer.security.tabs.my_roles"),
                        Some(self.my_users.iter().filter(|u| u.is_role).count()),
                    ),
                    SecurityTab::ChUsers => (
                        crate::localization::text("explorer.security.tabs.ch_users"),
                        Some(self.ch_users.len()),
                    ),
                    SecurityTab::ChRoles => (
                        crate::localization::text("explorer.security.tabs.ch_roles"),
                        Some(self.ch_roles.len()),
                    ),
                };
                let text = match count {
                    Some(n) if n > 0 => {
                        let count = n.to_string();
                        localized_format(
                            "explorer.security.tabs.with_count",
                            &[("label", &label), ("count", &count)],
                        )
                    }
                    _ => label,
                };
                (tab, text)
            })
            .collect();
        self.segmented(
            "security-tab",
            options,
            self.tab,
            !self.applying,
            |this, tab, window, cx| this.switch_tab(tab, window, cx),
            cx,
        )
        .into_any_element()
    }

    /// A row of mutually exclusive choices, the selected one raised.
    fn segmented<T: Copy + PartialEq + 'static>(
        &self,
        id: &'static str,
        options: Vec<(T, String)>,
        selected: T,
        enabled: bool,
        on_pick: impl Fn(&mut Self, T, &mut Window, &mut gpui::Context<Self>) + 'static,
        cx: &mut gpui::Context<Self>,
    ) -> Div {
        let on_pick = Rc::new(on_pick);
        let mut strip = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap(px(Theme::scaled(2.0)))
            .p(px(Theme::scaled(2.0)))
            .rounded(px(Theme::scaled(6.0)))
            .bg(Theme::panel_bg())
            .border_1()
            .border_color(Theme::border());
        for (i, (value, label)) in options.into_iter().enumerate() {
            let is_selected = value == selected;
            let on_pick = on_pick.clone();
            strip = strip.child(
                div()
                    .id((id, i))
                    .px(px(Theme::scaled(10.0)))
                    .py(px(Theme::scaled(3.0)))
                    .rounded(px(Theme::scaled(4.0)))
                    .when(is_selected, |d| {
                        d.bg(Theme::bg())
                            .border_1()
                            .border_color(Theme::border())
                            .text_color(Theme::text())
                    })
                    .when(!is_selected, |d| {
                        d.border_1()
                            .border_color(gpui::transparent_black())
                            .text_color(Theme::text_dim())
                            .when(enabled, |d| {
                                d.cursor_pointer().hover(|s| s.bg(Theme::row_hover()))
                            })
                    })
                    .when(enabled && !is_selected, |d| {
                        d.on_click(cx.listener(move |this, _, window, cx| {
                            on_pick(this, value, window, cx);
                        }))
                    })
                    .child(label),
            );
        }
        strip
    }

    // ── List panel ──────────────────────────────────────────────────────

    fn render_list_panel(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> Div {
        let _ = window;
        let filter = self.filter_text(cx);
        let matches = |name: &str| filter.is_empty() || name.to_lowercase().contains(&filter);
        let catalog_ready = self.catalog_state == LoadState::Ready && !self.applying;

        let (title, new_label, new_enabled, on_new, content): (
            String,
            Option<String>,
            bool,
            Option<NewAction>,
            AnyElement,
        ) = match self.tab {
            SecurityTab::Logins => {
                let rows: Vec<PrincipalRow> = self
                    .logins
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| matches(&l.name))
                    .map(|(idx, login)| {
                        let mut tags = vec![if login.kind == "SQL_LOGIN" {
                            (
                                crate::localization::text("explorer.security.tags.sql"),
                                Theme::info(),
                            )
                        } else if login.kind.starts_with("WINDOWS") {
                            (
                                crate::localization::text("explorer.security.tags.windows"),
                                Theme::accent(),
                            )
                        } else {
                            (
                                crate::localization::text("explorer.security.tags.external"),
                                Theme::accent(),
                            )
                        }];
                        if login.is_disabled {
                            tags.push((
                                crate::localization::text("explorer.security.tags.disabled"),
                                Theme::warning(),
                            ));
                        }
                        PrincipalRow {
                            idx,
                            name: login.name.clone(),
                            tags,
                        }
                    })
                    .collect();
                let content = match self.catalog_state {
                    LoadState::Idle | LoadState::Loading => list_message(
                        &crate::localization::text("explorer.security.lists.logins_loading"),
                    ),
                    LoadState::Failed => list_message(&localized_format(
                        "explorer.security.lists.logins_failed",
                        &[("error", &self.catalog_error)],
                    )),
                    LoadState::Ready if self.logins.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.logins_empty"),
                    ),
                    LoadState::Ready if rows.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.logins_no_matches"),
                    ),
                    LoadState::Ready => self.principal_rows(
                        "security-login-row",
                        rows,
                        self.selected_login,
                        Rc::new(|this, idx, window, cx| this.select_login(Some(idx), window, cx)),
                        cx,
                    ),
                };
                (
                    crate::localization::text("explorer.security.lists.logins_title"),
                    Some(crate::localization::text(
                        "explorer.security.lists.new_login",
                    )),
                    catalog_ready,
                    Some(Rc::new(
                        |this: &mut Self, window: &mut Window, cx: &mut gpui::Context<Self>| {
                            this.begin_new_login(window, cx)
                        },
                    )
                        as Rc<
                            dyn Fn(&mut Self, &mut Window, &mut gpui::Context<Self>),
                        >),
                    content,
                )
            }
            SecurityTab::Users => {
                let rows: Vec<PrincipalRow> = self
                    .users
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| matches(&u.name))
                    .map(|(idx, user)| PrincipalRow {
                        idx,
                        name: user.name.clone(),
                        tags: vec![user_kind_tag(user)],
                    })
                    .collect();
                let content = match self.users_state {
                    LoadState::Idle
                        if self.catalog_state == LoadState::Ready && self.databases.is_empty() =>
                    {
                        list_message(&crate::localization::text(
                            "explorer.security.lists.no_user_databases",
                        ))
                    }
                    LoadState::Idle => list_message(&crate::localization::text(
                        "explorer.security.lists.choose_database",
                    )),
                    LoadState::Loading => list_message(&crate::localization::text(
                        "explorer.security.lists.users_loading",
                    )),
                    LoadState::Failed => list_message(&localized_format(
                        "explorer.security.lists.users_failed",
                        &[("error", &self.users_error)],
                    )),
                    LoadState::Ready if self.users.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.users_empty"),
                    ),
                    LoadState::Ready if rows.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.users_no_matches"),
                    ),
                    LoadState::Ready => self.principal_rows(
                        "security-user-row",
                        rows,
                        self.selected_user,
                        Rc::new(|this, idx, window, cx| this.select_user(Some(idx), window, cx)),
                        cx,
                    ),
                };
                (
                    crate::localization::text("explorer.security.lists.users_title"),
                    Some(crate::localization::text(
                        "explorer.security.lists.new_user",
                    )),
                    self.users_state == LoadState::Ready && !self.applying,
                    Some(Rc::new(
                        |this: &mut Self, window: &mut Window, cx: &mut gpui::Context<Self>| {
                            this.begin_new_user(window, cx)
                        },
                    )
                        as Rc<
                            dyn Fn(&mut Self, &mut Window, &mut gpui::Context<Self>),
                        >),
                    content,
                )
            }
            SecurityTab::Roles => {
                let oracle = matches!(self.dialect, Some(SecurityDialect::Oracle));
                let rows: Vec<PrincipalRow> = self
                    .pg_roles
                    .iter()
                    .enumerate()
                    .filter(|(_, r)| matches(&r.name))
                    .map(|(idx, role)| {
                        let mut tags = Vec::new();
                        if role.login {
                            tags.push((
                                if oracle {
                                    crate::localization::text("explorer.security.tags.oracle_user")
                                } else {
                                    crate::localization::text("explorer.security.tags.login")
                                },
                                Theme::info(),
                            ));
                        } else {
                            tags.push((
                                crate::localization::text("explorer.security.tags.role"),
                                Theme::text_dim(),
                            ));
                        }
                        if role.superuser {
                            tags.push((
                                if oracle {
                                    crate::localization::text("explorer.security.tags.dba")
                                } else {
                                    crate::localization::text("explorer.security.tags.superuser")
                                },
                                Theme::warning(),
                            ));
                        }
                        PrincipalRow {
                            idx,
                            name: role.name.clone(),
                            tags,
                        }
                    })
                    .collect();
                let content = match self.catalog_state {
                    LoadState::Idle | LoadState::Loading => list_message(
                        &crate::localization::text("explorer.security.lists.roles_loading"),
                    ),
                    LoadState::Failed => list_message(&localized_format(
                        "explorer.security.lists.roles_failed",
                        &[("error", &self.catalog_error)],
                    )),
                    LoadState::Ready if self.pg_roles.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.roles_empty"),
                    ),
                    LoadState::Ready if rows.is_empty() => list_message(
                        &crate::localization::text("explorer.security.lists.roles_no_matches"),
                    ),
                    LoadState::Ready => self.principal_rows(
                        "security-pg-role-row",
                        rows,
                        self.selected_pg_role,
                        Rc::new(|this, idx, window, cx| this.select_pg_role(Some(idx), window, cx)),
                        cx,
                    ),
                };
                (
                    if oracle {
                        crate::localization::text(
                            "explorer.security.lists.oracle_users_roles_title",
                        )
                    } else {
                        crate::localization::text("explorer.security.lists.roles_title")
                    },
                    Some(if oracle {
                        crate::localization::text("explorer.security.lists.new_user_or_role")
                    } else {
                        crate::localization::text("explorer.security.lists.new_role")
                    }),
                    catalog_ready,
                    Some(Rc::new(
                        |this: &mut Self, window: &mut Window, cx: &mut gpui::Context<Self>| {
                            this.begin_new_pg_role(window, cx)
                        },
                    )
                        as Rc<
                            dyn Fn(&mut Self, &mut Window, &mut gpui::Context<Self>),
                        >),
                    content,
                )
            }
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let show_roles = self.tab == SecurityTab::MyRoles;
                let rows: Vec<PrincipalRow> = self
                    .my_users
                    .iter()
                    .enumerate()
                    .filter(|(_, u)| u.is_role == show_roles)
                    .map(|(idx, user)| (idx, user, format!("'{}'@'{}'", user.user, user.host)))
                    .filter(|(_, _, label)| matches(label))
                    .map(|(idx, user, name)| PrincipalRow {
                        idx,
                        name,
                        tags: if user.locked {
                            vec![(
                                crate::localization::text("explorer.security.tags.locked"),
                                Theme::warning(),
                            )]
                        } else {
                            Vec::new()
                        },
                    })
                    .collect();
                let content = match self.catalog_state {
                    LoadState::Idle | LoadState::Loading => list_message(&if show_roles {
                        crate::localization::text("explorer.security.lists.my_roles_loading")
                    } else {
                        crate::localization::text("explorer.security.lists.accounts_loading")
                    }),
                    LoadState::Failed => list_message(&if show_roles {
                        crate::localization::text("explorer.security.lists.my_roles_failed")
                    } else {
                        crate::localization::text("explorer.security.lists.accounts_failed")
                    }),
                    LoadState::Ready if rows.is_empty() && filter.is_empty() => {
                        list_message(&if show_roles {
                            crate::localization::text("explorer.security.lists.my_roles_empty")
                        } else {
                            crate::localization::text("explorer.security.lists.accounts_empty")
                        })
                    }
                    LoadState::Ready if rows.is_empty() => list_message(&if show_roles {
                        crate::localization::text("explorer.security.lists.my_roles_no_matches")
                    } else {
                        crate::localization::text("explorer.security.lists.accounts_no_matches")
                    }),
                    LoadState::Ready => self.principal_rows(
                        "security-my-user-row",
                        rows,
                        self.selected_my_user,
                        Rc::new(|this, idx, window, cx| this.select_my_user(Some(idx), window, cx)),
                        cx,
                    ),
                };
                (
                    if show_roles {
                        crate::localization::text("explorer.security.lists.my_roles_title")
                    } else {
                        crate::localization::text("explorer.security.lists.accounts_title")
                    },
                    Some(if show_roles {
                        crate::localization::text("explorer.security.lists.new_my_role")
                    } else {
                        crate::localization::text("explorer.security.lists.new_account")
                    }),
                    catalog_ready,
                    Some(Rc::new(
                        |this: &mut Self, window: &mut Window, cx: &mut gpui::Context<Self>| {
                            this.begin_new_my_user(window, cx)
                        },
                    )
                        as Rc<
                            dyn Fn(&mut Self, &mut Window, &mut gpui::Context<Self>),
                        >),
                    content,
                )
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => {
                let is_role = self.tab == SecurityTab::ChRoles;
                let rows: Vec<PrincipalRow> = if is_role {
                    self.ch_roles
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| matches(&r.name))
                        .map(|(idx, r)| PrincipalRow {
                            idx,
                            name: r.name.clone(),
                            tags: Vec::new(),
                        })
                        .collect()
                } else {
                    self.ch_users
                        .iter()
                        .enumerate()
                        .filter(|(_, u)| matches(&u.name))
                        .map(|(idx, u)| PrincipalRow {
                            idx,
                            name: u.name.clone(),
                            tags: Vec::new(),
                        })
                        .collect()
                };
                let selected = if is_role {
                    self.selected_ch_role
                } else {
                    self.selected_ch_user
                };
                let content = match self.catalog_state {
                    LoadState::Idle | LoadState::Loading => list_message(
                        &crate::localization::text("explorer.security.lists.ch_loading"),
                    ),
                    LoadState::Failed => list_message(&localized_format(
                        "explorer.security.lists.ch_failed",
                        &[("error", &self.catalog_error)],
                    )),
                    LoadState::Ready if rows.is_empty() => list_message(&if is_role {
                        crate::localization::text("explorer.security.lists.ch_roles_empty")
                    } else {
                        crate::localization::text("explorer.security.lists.ch_users_empty")
                    }),
                    LoadState::Ready => self.principal_rows(
                        "security-ch-row",
                        rows,
                        selected,
                        Rc::new(move |this, idx, _window, cx| {
                            this.select_ch_principal(Some(idx), is_role, cx)
                        }),
                        cx,
                    ),
                };
                (
                    if is_role {
                        crate::localization::text("explorer.security.lists.ch_roles_title")
                    } else {
                        crate::localization::text("explorer.security.lists.ch_users_title")
                    },
                    None,
                    false,
                    None,
                    content,
                )
            }
        };

        let new_button = new_label.map(|label| {
            let on_new = on_new.clone();
            Button::new("security-new")
                .small()
                .outline()
                .label(localized_format(
                    "explorer.security.lists.new_button",
                    &[("label", &label)],
                ))
                .disabled(!new_enabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    if let Some(on_new) = &on_new {
                        on_new(this, window, cx);
                    }
                }))
        });

        let mut panel = div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(8.0)))
            .w(px(Theme::scaled(LIST_WIDTH)))
            .flex_none()
            .min_h_0()
            .p(px(Theme::scaled(10.0)))
            .rounded(px(Theme::scaled(8.0)))
            .bg(Theme::panel_bg())
            .border_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .gap(px(Theme::scaled(6.0)))
                    .child(section_title(&title))
                    .children(new_button),
            );
        if self.tab == SecurityTab::Users {
            panel = panel.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::scaled(3.0)))
                    .child(caption(crate::localization::text(
                        "explorer.security.lists.database_label",
                    )))
                    .child(Select::new(&self.users_db_select).small().w_full()),
            );
        }
        panel
            .child(
                Input::new(&self.list_filter_input)
                    .small()
                    .cleanable(true)
                    .prefix(div().text_color(Theme::text_dim()).child("⌕")),
            )
            .child(content)
    }

    fn principal_rows(
        &self,
        id: &'static str,
        rows: Vec<PrincipalRow>,
        selected: Option<usize>,
        on_select: RowSelect,
        cx: &mut gpui::Context<Self>,
    ) -> AnyElement {
        div()
            .id(("security-rows", id.len()))
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .gap(px(Theme::scaled(1.0)))
            .children(rows.into_iter().map(|row| {
                let is_selected = Some(row.idx) == selected;
                let on_select = on_select.clone();
                let idx = row.idx;
                themed_row(
                    div()
                        .id((id, idx))
                        .cursor_pointer()
                        .min_h(px(Theme::row_height()))
                        .px(px(Theme::scaled(8.0)))
                        .py(px(Theme::scaled(4.0)))
                        .rounded(px(Theme::scaled(5.0)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            window.focus(&this.focus_handle, cx);
                            on_select(this, idx, window, cx);
                        })),
                    is_selected,
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(Theme::scaled(6.0)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .whitespace_nowrap()
                                .text_color(Theme::text())
                                .child(row.name),
                        )
                        .children(row.tags.into_iter().map(|(text, color)| tag(text, color))),
                )
            }))
            .into_any_element()
    }

    // ── Editor card ─────────────────────────────────────────────────────

    fn render_editor_card(&mut self, cx: &mut gpui::Context<Self>) -> Stateful<Div> {
        let card = div()
            .id("security-editor")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .rounded(px(Theme::scaled(8.0)))
            .bg(Theme::bg())
            .border_1()
            .border_color(Theme::border());

        if !self.has_subject() {
            let hint = match self.tab {
                SecurityTab::Logins => {
                    crate::localization::text("explorer.security.editor.empty_login")
                }
                SecurityTab::Users => {
                    crate::localization::text("explorer.security.editor.empty_user")
                }
                SecurityTab::Roles => {
                    crate::localization::text("explorer.security.editor.empty_role")
                }
                SecurityTab::Accounts => {
                    crate::localization::text("explorer.security.editor.empty_account")
                }
                SecurityTab::MyRoles => {
                    crate::localization::text("explorer.security.editor.empty_my_role")
                }
                SecurityTab::ChUsers => {
                    crate::localization::text("explorer.security.editor.empty_ch_user")
                }
                SecurityTab::ChRoles => {
                    crate::localization::text("explorer.security.editor.empty_ch_role")
                }
            };
            return card.child(
                div()
                    .flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .p(px(Theme::scaled(24.0)))
                    .child(
                        div()
                            .max_w(px(Theme::scaled(420.0)))
                            .text_color(Theme::text_dim())
                            .text_center()
                            .whitespace_normal()
                            .child(hint),
                    ),
            );
        }

        let body: AnyElement = match self.tab {
            SecurityTab::Logins => self.render_login_editor(cx).into_any_element(),
            SecurityTab::Users => self.render_user_editor(cx).into_any_element(),
            SecurityTab::Roles => self.render_pg_role_editor(cx).into_any_element(),
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                self.render_my_account_editor(cx).into_any_element()
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => {
                self.render_ch_detail(cx).into_any_element()
            }
        };

        card.child(self.render_editor_header(cx)).child(
            div()
                .id("security-editor-body")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(Theme::scaled(16.0)))
                .py(px(Theme::scaled(12.0)))
                .child(body),
        )
    }

    fn render_editor_header(&self, cx: &mut gpui::Context<Self>) -> Div {
        let is_new = self.is_new_subject();
        let clickhouse = matches!(self.tab, SecurityTab::ChUsers | SecurityTab::ChRoles);
        let (title, tags): (String, Vec<(String, Hsla)>) = match self.tab {
            SecurityTab::Logins if is_new => (
                crate::localization::text("explorer.security.editor.new_login"),
                Vec::new(),
            ),
            SecurityTab::Logins => {
                let login = self.selected_login.and_then(|i| self.logins.get(i));
                (
                    login.map(|l| l.name.clone()).unwrap_or_default(),
                    login
                        .map(|l| {
                            let mut tags = vec![match self.login_auth {
                                LoginAuth::Sql => (
                                    crate::localization::text(
                                        "explorer.security.tags.sql_authentication",
                                    ),
                                    Theme::info(),
                                ),
                                LoginAuth::Windows => (
                                    crate::localization::text(
                                        "explorer.security.tags.windows_authentication",
                                    ),
                                    Theme::accent(),
                                ),
                            }];
                            if l.is_disabled {
                                tags.push((
                                    crate::localization::text("explorer.security.tags.disabled"),
                                    Theme::warning(),
                                ));
                            }
                            tags
                        })
                        .unwrap_or_default(),
                )
            }
            SecurityTab::Users if is_new => {
                let database = self.users_db.clone().unwrap_or_else(|| {
                    crate::localization::text("explorer.security.editor.this_database")
                });
                (
                    localized_format(
                        "explorer.security.editor.new_user_in_database",
                        &[("database", &database)],
                    ),
                    Vec::new(),
                )
            }
            SecurityTab::Users => {
                let user = self.selected_user.and_then(|i| self.users.get(i));
                (
                    user.map(|u| u.name.clone()).unwrap_or_default(),
                    user.map(|u| vec![user_kind_tag(u)]).unwrap_or_default(),
                )
            }
            SecurityTab::Roles if is_new => (
                if matches!(self.dialect, Some(SecurityDialect::Oracle)) {
                    crate::localization::text("explorer.security.editor.new_user_or_role")
                } else {
                    crate::localization::text("explorer.security.editor.new_role")
                },
                Vec::new(),
            ),
            SecurityTab::Roles => (
                self.selected_pg_role
                    .and_then(|i| self.pg_roles.get(i))
                    .map(|r| r.name.clone())
                    .unwrap_or_default(),
                Vec::new(),
            ),
            SecurityTab::Accounts | SecurityTab::MyRoles if is_new => (
                if self.tab == SecurityTab::MyRoles {
                    crate::localization::text("explorer.security.editor.new_my_role")
                } else {
                    crate::localization::text("explorer.security.editor.new_account")
                },
                Vec::new(),
            ),
            SecurityTab::Accounts | SecurityTab::MyRoles => {
                let user = self.selected_my_user.and_then(|i| self.my_users.get(i));
                (
                    user.map(|u| format!("'{}'@'{}'", u.user, u.host))
                        .unwrap_or_default(),
                    user.filter(|u| u.locked)
                        .map(|_| {
                            vec![(
                                crate::localization::text("explorer.security.tags.locked"),
                                Theme::warning(),
                            )]
                        })
                        .unwrap_or_default(),
                )
            }
            SecurityTab::ChUsers | SecurityTab::ChRoles => (
                self.selected_ch_principal()
                    .map(|(name, _)| name)
                    .unwrap_or_default(),
                vec![(
                    crate::localization::text("explorer.security.tags.read_only"),
                    Theme::text_dim(),
                )],
            ),
        };

        let dirty = !clickhouse && self.form_is_dirty(cx);
        let mut actions = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(6.0)));
        if dirty && !self.pending_drop {
            actions = actions
                .child(tag(
                    crate::localization::text("explorer.security.tags.unsaved"),
                    Theme::accent(),
                ))
                .child(
                    Button::new("security-revert")
                        .small()
                        .ghost()
                        .label(crate::localization::text("explorer.security.editor.revert"))
                        .tooltip(crate::localization::text(
                            "explorer.security.editor.revert_tooltip",
                        ))
                        .disabled(self.applying)
                        .on_click(cx.listener(|this, _, window, cx| this.revert(window, cx))),
                );
        }
        if !clickhouse && !is_new {
            actions = if self.pending_drop {
                actions.child(
                    Button::new("security-cancel-drop")
                        .small()
                        .ghost()
                        .label(crate::localization::text("explorer.security.drop.keep"))
                        .disabled(self.applying)
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_drop(cx))),
                )
            } else {
                actions.child(
                    Button::new("security-drop")
                        .small()
                        .danger()
                        .outline()
                        .label(crate::localization::text(
                            "explorer.security.drop.menu_button",
                        ))
                        .tooltip(crate::localization::text("explorer.security.drop.tooltip"))
                        .disabled(self.applying || self.production_read_only)
                        .on_click(cx.listener(|this, _, _, cx| this.arm_drop(cx))),
                )
            };
        }

        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .gap(px(Theme::scaled(8.0)))
            .px(px(Theme::scaled(16.0)))
            .py(px(Theme::scaled(10.0)))
            .border_b_1()
            .border_color(Theme::border())
            .when(self.pending_drop, |d| d.bg(Theme::error().opacity(0.08)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(Theme::scaled(8.0)))
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_size(px(Theme::font_size() * 1.15))
                            .text_color(Theme::text())
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(title),
                    )
                    .children(tags.into_iter().map(|(text, color)| tag(text, color)))
                    .when(self.pending_drop, |d| {
                        d.child(tag(
                            crate::localization::text("explorer.security.tags.will_be_dropped"),
                            Theme::error(),
                        ))
                    }),
            )
            .child(actions)
    }

    fn render_login_editor(&mut self, cx: &mut gpui::Context<Self>) -> Div {
        let is_new = self.editing_new_login;
        let detail_loading = !is_new && self.login_detail_state == LoadState::Loading;

        let mut column = div().flex().flex_col().gap(px(Theme::scaled(18.0)));
        if is_new {
            column = column.child(
                section(crate::localization::text("explorer.security.login.sign_in")).child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::scaled(10.0)))
                        .child(field(
                            crate::localization::text("explorer.security.login.name_label"),
                            Input::new(&self.login_name_input)
                                .font_family(Theme::editor_font_family())
                                .into_any_element(),
                        ))
                        .child(field(
                            crate::localization::text("explorer.security.login.authentication"),
                            self.segmented(
                                "security-auth",
                                vec![
                                    (
                                        LoginAuth::Sql,
                                        crate::localization::text(
                                            "explorer.security.login.sql_password",
                                        ),
                                    ),
                                    (
                                        LoginAuth::Windows,
                                        crate::localization::text(
                                            "explorer.security.login.windows",
                                        ),
                                    ),
                                ],
                                self.login_auth,
                                !self.applying,
                                |this, auth, _, cx| {
                                    this.login_auth = auth;
                                    this.refresh_preview_status(cx);
                                },
                                cx,
                            )
                            .into_any_element(),
                        ))
                        .when(self.login_auth == LoginAuth::Windows, |d| {
                            d.child(hint(&crate::localization::text(
                                "explorer.security.login.windows_hint",
                            )))
                        }),
                ),
            );
        }
        if self.login_auth == LoginAuth::Sql {
            column = column.child(self.render_password_section(
                "security-login-pw",
                self.login_password_input.clone(),
                self.login_password_confirm_input.clone(),
                is_new,
                None,
                cx,
            ));
        }
        column = column.child(
            section(crate::localization::text("explorer.security.login.status")).child(
                Checkbox::new("security-login-enabled")
                    .checked(self.login_enabled)
                    .label(crate::localization::text("explorer.security.login.enabled"))
                    .tab_stop(true)
                    .disabled(self.applying)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.login_enabled = *checked;
                        this.refresh_preview_status(cx);
                    })),
            ),
        );
        if detail_loading {
            column = column.child(hint(&crate::localization::text(
                "explorer.security.login.detail_loading",
            )));
        } else {
            let server_roles = self.server_roles.clone();
            let checked = self.login_server_roles.clone();
            column = column.child(self.render_checklist(
                crate::localization::text("explorer.security.login.server_roles"),
                0,
                &server_roles,
                &checked,
                crate::localization::text("explorer.security.login.no_server_roles"),
                None,
                Rc::new(|this, role, checked, cx| this.toggle_login_server_role(role, checked, cx)),
                cx,
            ));
            let databases = self.databases.clone();
            let checked = self.login_db_access.clone();
            column = column.child(self.render_checklist(
                crate::localization::text("explorer.security.login.database_access"),
                1,
                &databases,
                &checked,
                crate::localization::text("explorer.security.login.no_databases"),
                Some(crate::localization::text(
                    "explorer.security.login.database_access_hint",
                )),
                Rc::new(|this, db, checked, cx| this.toggle_login_db_access(db, checked, cx)),
                cx,
            ));
        }
        column
    }

    fn render_user_editor(&mut self, cx: &mut gpui::Context<Self>) -> Div {
        let is_new = self.editing_new_user;
        let mut column = div().flex().flex_col().gap(px(Theme::scaled(18.0)));

        if is_new {
            let mut sign_in = div()
                .flex()
                .flex_col()
                .gap(px(Theme::scaled(10.0)))
                .child(field(
                    crate::localization::text("explorer.security.user.name_label"),
                    Input::new(&self.user_name_input)
                        .font_family(Theme::editor_font_family())
                        .into_any_element(),
                ))
                .child(field(
                    crate::localization::text("explorer.security.user.sign_in_method"),
                    self.segmented(
                        "security-user-auth",
                        vec![
                            (
                                UserAuth::ExistingLogin,
                                crate::localization::text("explorer.security.user.existing_login"),
                            ),
                            (
                                UserAuth::NewLogin,
                                crate::localization::text("explorer.security.user.new_login"),
                            ),
                            (
                                UserAuth::Contained,
                                crate::localization::text("explorer.security.user.contained"),
                            ),
                            (
                                UserAuth::NoLogin,
                                crate::localization::text("explorer.security.user.without_login"),
                            ),
                        ],
                        self.user_auth,
                        !self.applying,
                        |this, auth, _, cx| {
                            this.user_auth = auth;
                            this.refresh_preview_status(cx);
                        },
                        cx,
                    )
                    .into_any_element(),
                ));
            sign_in = match self.user_auth {
                UserAuth::ExistingLogin => sign_in.child(field(
                    crate::localization::text("explorer.security.user.login_label"),
                    if self.logins.is_empty() {
                        hint(&crate::localization::text(
                            "explorer.security.user.no_logins_hint",
                        ))
                        .into_any_element()
                    } else {
                        Select::new(&self.user_login_select)
                            .placeholder(crate::localization::text(
                                "explorer.security.user.choose_login",
                            ))
                            .w_full()
                            .into_any_element()
                    },
                )),
                UserAuth::NewLogin => sign_in.child(hint(&crate::localization::text(
                    "explorer.security.user.new_login_hint",
                ))),
                UserAuth::Contained => sign_in.child(hint(&crate::localization::text(
                    "explorer.security.user.contained_hint",
                ))),
                UserAuth::NoLogin => sign_in.child(hint(&crate::localization::text(
                    "explorer.security.user.without_login_hint",
                ))),
            };
            column = column.child(
                section(crate::localization::text("explorer.security.user.sign_in")).child(sign_in),
            );
            if matches!(self.user_auth, UserAuth::NewLogin | UserAuth::Contained) {
                column = column.child(self.render_password_section(
                    "security-user-pw",
                    self.user_password_input.clone(),
                    self.user_password_confirm_input.clone(),
                    true,
                    None,
                    cx,
                ));
            }
        } else {
            let login_text = if self.user_login.is_empty() {
                crate::localization::text("explorer.security.user.no_mapped_login")
            } else {
                self.user_login.clone()
            };
            column = column.child(
                section(crate::localization::text(
                    "explorer.security.user.existing_sign_in",
                ))
                .child(field(
                    crate::localization::text("explorer.security.user.mapped_login"),
                    div()
                        .font_family(Theme::editor_font_family())
                        .text_color(Theme::text())
                        .child(login_text)
                        .into_any_element(),
                )),
            );
            match self
                .user_baseline
                .as_ref()
                .map(|b| b.password_target.clone())
            {
                Some(UserPasswordTarget::Contained) => {
                    column = column.child(self.render_password_section(
                        "security-user-pw",
                        self.user_password_input.clone(),
                        self.user_password_confirm_input.clone(),
                        false,
                        Some(crate::localization::text(
                            "explorer.security.user.contained_password_note",
                        )),
                        cx,
                    ));
                }
                Some(UserPasswordTarget::Login(login)) => {
                    column = column.child(self.render_password_section(
                        "security-user-pw",
                        self.user_password_input.clone(),
                        self.user_password_confirm_input.clone(),
                        false,
                        Some(localized_format(
                            "explorer.security.user.login_password_note",
                            &[("login", &login)],
                        )),
                        cx,
                    ));
                }
                _ => {
                    column = column.child(
                        section(crate::localization::text("explorer.security.user.password"))
                            .child(hint(&crate::localization::text(
                                "explorer.security.user.password_not_managed",
                            ))),
                    );
                }
            }
        }

        column = column.child(
            section(crate::localization::text("explorer.security.user.defaults")).child(field(
                crate::localization::text("explorer.security.user.default_schema"),
                Input::new(&self.user_schema_input)
                    .font_family(Theme::editor_font_family())
                    .into_any_element(),
            )),
        );
        let roles = self.db_roles.clone();
        let checked = self.user_roles.clone();
        column.child(self.render_checklist(
            crate::localization::text("explorer.security.user.database_roles"),
            2,
            &roles,
            &checked,
            crate::localization::text("explorer.security.user.no_database_roles"),
            None,
            Rc::new(|this, role, checked, cx| this.toggle_user_role(role, checked, cx)),
            cx,
        ))
    }

    fn render_pg_role_editor(&mut self, cx: &mut gpui::Context<Self>) -> Div {
        let is_new = self.editing_new_pg_role;
        // Oracle rides this form with its own vocabulary: a login role is a
        // user, "connect" is CREATE SESSION, and there is no schema usage.
        let oracle = matches!(self.dialect, Some(SecurityDialect::Oracle));
        let mut column = div().flex().flex_col().gap(px(Theme::scaled(18.0)));
        if is_new {
            column = column.child(
                section(crate::localization::text("explorer.security.role.name")).child(
                    Input::new(&self.pg_role_name_input).font_family(Theme::editor_font_family()),
                ),
            );
        }

        let opts = self.pg_role_opts;
        let mut flags: Vec<RoleFlag> = vec![
            (
                "security-pg-login",
                if oracle {
                    crate::localization::text("explorer.security.role.oracle_login")
                } else {
                    crate::localization::text("explorer.security.role.can_log_in")
                },
                opts.login,
                |o, v| o.login = v,
            ),
            (
                "security-pg-super",
                if oracle {
                    crate::localization::text("explorer.security.role.dba")
                } else {
                    crate::localization::text("explorer.security.role.superuser")
                },
                opts.superuser,
                |o, v| o.superuser = v,
            ),
        ];
        if !oracle {
            flags.push((
                "security-pg-createdb",
                crate::localization::text("explorer.security.role.create_databases"),
                opts.createdb,
                |o, v| o.createdb = v,
            ));
        }
        flags.push((
            "security-pg-createrole",
            if oracle {
                crate::localization::text("explorer.security.role.oracle_create_role")
            } else {
                crate::localization::text("explorer.security.role.create_roles")
            },
            opts.createrole,
            |o, v| o.createrole = v,
        ));
        if !oracle {
            flags.push((
                "security-pg-inherit",
                crate::localization::text("explorer.security.role.inherit_privileges"),
                opts.inherit,
                |o, v| o.inherit = v,
            ));
        }
        let mut flag_grid = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(Theme::scaled(16.0)))
            .gap_y(px(Theme::scaled(6.0)));
        for (id, label, checked, set) in flags {
            flag_grid = flag_grid.child(
                div().w(px(Theme::scaled(230.0))).child(
                    Checkbox::new(id)
                        .checked(checked)
                        .label(label)
                        .disabled(self.applying)
                        .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                            set(&mut this.pg_role_opts, *checked);
                            this.refresh_preview_status(cx);
                        })),
                ),
            );
        }
        column = column.child(
            section(crate::localization::text(
                "explorer.security.role.attributes",
            ))
            .child(flag_grid),
        );

        if opts.login {
            column = column.child(self.render_password_section(
                "security-pg-pw",
                self.pg_role_password_input.clone(),
                self.pg_role_password_confirm_input.clone(),
                is_new,
                None,
                cx,
            ));
        }

        let all_roles: Vec<String> = self
            .pg_all_roles
            .iter()
            .filter(|r| {
                Some(r.as_str())
                    != self
                        .selected_pg_role
                        .and_then(|i| self.pg_roles.get(i))
                        .map(|s| s.name.as_str())
            })
            .cloned()
            .collect();
        let member_of = self.pg_member_of.clone();
        column = column.child(self.render_checklist(
            if oracle {
                crate::localization::text("explorer.security.role.granted_roles")
            } else {
                crate::localization::text("explorer.security.role.member_of")
            },
            10,
            &all_roles,
            &member_of,
            crate::localization::text("explorer.security.role.no_roles"),
            None,
            Rc::new(|this, role, checked, cx| {
                if checked {
                    this.pg_member_of.insert(role.to_string());
                } else {
                    this.pg_member_of.remove(role);
                }
                this.refresh_preview_status(cx);
            }),
            cx,
        ));
        let databases = self.databases.clone();
        let connect = self.pg_connect_dbs.clone();
        column = column.child(self.render_checklist(
            if oracle {
                crate::localization::text("explorer.security.role.open_sessions")
            } else {
                crate::localization::text("explorer.security.role.can_connect")
            },
            11,
            &databases,
            &connect,
            crate::localization::text("explorer.security.role.no_databases"),
            None,
            Rc::new(|this, db, checked, cx| {
                if checked {
                    this.pg_connect_dbs.insert(db.to_string());
                } else {
                    this.pg_connect_dbs.remove(db);
                }
                this.refresh_preview_status(cx);
            }),
            cx,
        ));
        if !oracle {
            let schemas: Vec<String> = self
                .pg_schema_pairs
                .iter()
                .map(|(schema, _)| schema.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let usage = self.pg_schema_usage.clone();
            column = column.child(self.render_checklist(
                crate::localization::text("explorer.security.role.schema_usage"),
                12,
                &schemas,
                &usage,
                crate::localization::text("explorer.security.role.no_schemas"),
                None,
                Rc::new(|this, schema, checked, cx| {
                    if checked {
                        this.pg_schema_usage.insert(schema.to_string());
                    } else {
                        this.pg_schema_usage.remove(schema);
                    }
                    this.refresh_preview_status(cx);
                }),
                cx,
            ));
        }
        column
    }

    fn render_my_account_editor(&mut self, cx: &mut gpui::Context<Self>) -> Div {
        let is_new = self.editing_new_my_user;
        let mut column = div().flex().flex_col().gap(px(Theme::scaled(18.0)));
        if is_new {
            column = column.child(
                section(crate::localization::text(
                    "explorer.security.account.account",
                ))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(Theme::scaled(8.0)))
                        .child(
                            div().flex_1().child(field(
                                crate::localization::text("explorer.security.account.user"),
                                Input::new(&self.my_user_input)
                                    .font_family(Theme::editor_font_family())
                                    .into_any_element(),
                            )),
                        )
                        .child(
                            div().w(px(Theme::scaled(180.0))).child(field(
                                crate::localization::text("explorer.security.account.host"),
                                Input::new(&self.my_host_input)
                                    .font_family(Theme::editor_font_family())
                                    .into_any_element(),
                            )),
                        ),
                ),
            );
        }
        column = column
            .child(self.render_password_section(
                "security-my-pw",
                self.my_password_input.clone(),
                self.my_password_confirm_input.clone(),
                is_new,
                None,
                cx,
            ))
            .child(
                section(crate::localization::text(
                    "explorer.security.account.status",
                ))
                .child(
                    Checkbox::new("security-my-locked")
                        .checked(self.my_user_locked)
                        .label(crate::localization::text(
                            "explorer.security.account.locked",
                        ))
                        .disabled(self.applying)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.my_user_locked = *checked;
                            this.refresh_preview_status(cx);
                        })),
                ),
            );
        let globals = self.my_global_privileges.clone();
        let checked = self.my_global.clone();
        column = column.child(self.render_checklist(
            crate::localization::text("explorer.security.account.global_privileges"),
            20,
            &globals,
            &checked,
            crate::localization::text("explorer.security.account.no_global_privileges"),
            None,
            Rc::new(|this, priv_name, checked, cx| {
                if checked {
                    this.my_global.insert(priv_name.to_string());
                } else {
                    this.my_global.remove(priv_name);
                }
                this.refresh_preview_status(cx);
            }),
            cx,
        ));
        if self.tab == SecurityTab::Accounts && !self.my_databases.is_empty() {
            let db = self
                .my_selected_database
                .clone()
                .or_else(|| self.my_databases.first().cloned())
                .unwrap_or_default();
            let checked = self.my_database_privs.get(&db).cloned().unwrap_or_default();
            let granted_in: Vec<String> = self
                .my_database_privs
                .iter()
                .filter(|(_, privs)| !privs.is_empty())
                .map(|(db, _)| db.clone())
                .collect();
            let privileges = self.my_database_privileges.clone();
            column = column.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(Theme::scaled(6.0)))
                    .child(field(
                        crate::localization::text("explorer.security.account.database"),
                        Select::new(&self.my_db_select)
                            .small()
                            .w(px(Theme::scaled(260.0)))
                            .into_any_element(),
                    ))
                    .when(!granted_in.is_empty(), |d| {
                        let databases = granted_in.join(", ");
                        d.child(hint(&localized_format(
                            "explorer.security.account.has_privileges_in",
                            &[("databases", &databases)],
                        )))
                    })
                    .child(self.render_checklist(
                        crate::localization::text("explorer.security.account.database_privileges"),
                        21,
                        &privileges,
                        &checked,
                        crate::localization::text(
                            "explorer.security.account.no_database_privileges",
                        ),
                        None,
                        Rc::new(move |this, priv_name, is_checked, cx| {
                            let entry = this.my_database_privs.entry(db.clone()).or_default();
                            if is_checked {
                                entry.insert(priv_name.to_string());
                            } else {
                                entry.remove(priv_name);
                            }
                            this.refresh_preview_status(cx);
                        }),
                        cx,
                    )),
            );
        }
        if self.my_roles_supported && self.tab == SecurityTab::Accounts {
            let roles: Vec<String> = self.my_role_names.iter().cloned().collect();
            let assigned = self.my_assigned_roles.clone();
            column = column.child(self.render_checklist(
                crate::localization::text("explorer.security.account.roles"),
                22,
                &roles,
                &assigned,
                crate::localization::text("explorer.security.account.no_roles"),
                None,
                Rc::new(|this, role, checked, cx| {
                    if checked {
                        this.my_assigned_roles.insert(role.to_string());
                    } else {
                        this.my_assigned_roles.remove(role);
                    }
                    this.refresh_preview_status(cx);
                }),
                cx,
            ));
        }
        column
    }

    fn render_ch_detail(&mut self, _cx: &mut gpui::Context<Self>) -> Div {
        let is_role = self.tab == SecurityTab::ChRoles;
        let text = |value: String| {
            div()
                .whitespace_normal()
                .text_color(Theme::text())
                .child(value)
                .into_any_element()
        };
        let mut column = div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(12.0)))
            .child(hint(&crate::localization::text(CH_READ_ONLY_DETAIL)));
        let Some((name, _)) = self.selected_ch_principal() else {
            return column;
        };
        if is_role {
            if let Some(role) = self.selected_ch_role.and_then(|i| self.ch_roles.get(i)) {
                let holders: Vec<String> = self
                    .ch_role_grants
                    .iter()
                    .filter(|g| g.granted_role == role.name)
                    .map(|g| {
                        if g.user_name.is_empty() {
                            localized_format(
                                "explorer.security.clickhouse.role_holder",
                                &[("role", &g.role_name)],
                            )
                        } else {
                            g.user_name.clone()
                        }
                    })
                    .collect();
                column = column
                    .child(field(
                        crate::localization::text("explorer.security.clickhouse.role_defined_in"),
                        text(role.storage.clone()),
                    ))
                    .child(field(
                        crate::localization::text("explorer.security.clickhouse.granted_to"),
                        text(if holders.is_empty() {
                            crate::localization::text("explorer.security.clickhouse.nobody")
                        } else {
                            holders.join(", ")
                        }),
                    ));
            }
        } else if let Some(user) = self.selected_ch_user.and_then(|i| self.ch_users.get(i)) {
            let profiles: Vec<String> = self
                .ch_profiles
                .iter()
                .filter(|(_, applies_to)| {
                    applies_to == "everyone"
                        || applies_to.split(',').any(|who| who.trim() == user.name)
                })
                .map(|(profile, _)| profile.clone())
                .collect();
            column = column
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.authentication"),
                    text(user.auth_types.clone()),
                ))
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.allowed_hosts"),
                    text(user.hosts.clone()),
                ))
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.default_roles"),
                    text(user.default_roles.clone()),
                ))
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.default_database"),
                    text(if user.default_database.is_empty() {
                        crate::localization::text("explorer.security.clickhouse.no_database")
                    } else {
                        user.default_database.clone()
                    }),
                ))
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.settings_profiles"),
                    text(if profiles.is_empty() {
                        crate::localization::text("explorer.security.clickhouse.no_profiles")
                    } else {
                        profiles.join(", ")
                    }),
                ))
                .child(field(
                    crate::localization::text("explorer.security.clickhouse.user_defined_in"),
                    text(user.storage.clone()),
                ));
        }
        let grants = ch_model::grants_for(&self.ch_grants, &name, is_role);
        let roles = ch_model::roles_for(&self.ch_role_grants, &name, is_role);
        let grant_lines = if grants.is_empty() {
            crate::localization::text("explorer.security.clickhouse.no_privileges")
        } else {
            grants
                .iter()
                .map(|g| ch_sql::grant_statement_sql(&name, g))
                .collect::<Vec<_>>()
                .join("\n")
        };
        let role_lines = if roles.is_empty() {
            crate::localization::text("explorer.security.clickhouse.no_roles")
        } else {
            roles
                .iter()
                .map(|(role, admin)| {
                    if *admin {
                        localized_format(
                            "explorer.security.clickhouse.with_admin_option",
                            &[("role", role)],
                        )
                    } else {
                        role.clone()
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        column
            .child(field(
                crate::localization::text("explorer.security.clickhouse.privileges"),
                text(grant_lines),
            ))
            .child(field(
                crate::localization::text("explorer.security.clickhouse.roles"),
                text(role_lines),
            ))
    }

    // ── Shared pieces ───────────────────────────────────────────────────

    /// Password + confirmation side by side, a Generate button, and live
    /// feedback (strength, match) under them.
    fn render_password_section(
        &self,
        id: &'static str,
        password: Entity<InputState>,
        confirm: Entity<InputState>,
        is_new: bool,
        note: Option<String>,
        cx: &mut gpui::Context<Self>,
    ) -> Div {
        let pw = password.read(cx).value().to_string();
        let confirm_value = confirm.read(cx).value().to_string();
        let feedback: AnyElement = if pw.is_empty() {
            hint(&if is_new {
                crate::localization::text("explorer.security.password.enter_or_generate")
            } else {
                crate::localization::text("explorer.security.password.keep_current")
            })
            .into_any_element()
        } else {
            let strength = password_strength(&pw);
            let (strength_color, strength_label) = match strength {
                PasswordStrength::Weak => (
                    Theme::error(),
                    crate::localization::text("explorer.security.password.strength_weak"),
                ),
                PasswordStrength::Fair => (
                    Theme::warning(),
                    crate::localization::text("explorer.security.password.strength_fair"),
                ),
                PasswordStrength::Strong => (
                    Theme::success(),
                    crate::localization::text("explorer.security.password.strength_strong"),
                ),
            };
            let (match_text, match_color) = if confirm_value.is_empty() {
                (
                    crate::localization::text("explorer.security.password.confirmation_required"),
                    Theme::text_dim(),
                )
            } else if confirm_value == pw {
                (
                    crate::localization::text("explorer.security.password.match"),
                    Theme::success(),
                )
            } else {
                (
                    crate::localization::text("explorer.security.password.mismatch"),
                    Theme::error(),
                )
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(Theme::scaled(10.0)))
                .text_size(px(Theme::font_size() * 0.92))
                .child(strength_meter(strength, strength_color))
                .child(div().text_color(strength_color).child(strength_label))
                .child(div().text_color(Theme::text_dim()).child("·"))
                .child(div().text_color(match_color).child(match_text))
                .into_any_element()
        };
        let gen_password = password.clone();
        let gen_confirm = confirm.clone();
        section(if is_new {
            crate::localization::text("explorer.security.password.section_new")
        } else {
            crate::localization::text("explorer.security.password.section_change")
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_end()
                .gap(px(Theme::scaled(8.0)))
                .child(div().flex_1().min_w_0().child(field(
                    if is_new {
                        crate::localization::text("explorer.security.password.field_new")
                    } else {
                        crate::localization::text("explorer.security.password.field_change")
                    },
                    Input::new(&password).mask_toggle().into_any_element(),
                )))
                .child(div().flex_1().min_w_0().child(field(
                    crate::localization::text("explorer.security.password.confirm_label"),
                    Input::new(&confirm).mask_toggle().into_any_element(),
                )))
                .child(
                    Button::new(id)
                        .outline()
                        .label(crate::localization::text(
                            "explorer.security.password.generate",
                        ))
                        .tooltip(crate::localization::text(
                            "explorer.security.password.generate_tooltip",
                        ))
                        .disabled(self.applying)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.generate_password_into(
                                gen_password.clone(),
                                gen_confirm.clone(),
                                window,
                                cx,
                            );
                        })),
                ),
        )
        .child(feedback)
        .when_some(note, |d, note| d.child(hint(&note)))
    }

    /// A titled grid of checkboxes with a live "n of m" count and All/None.
    #[allow(clippy::too_many_arguments)]
    fn render_checklist(
        &self,
        title: String,
        id_key: usize,
        items: &[String],
        checked_items: &BTreeSet<String>,
        empty_message: String,
        note: Option<String>,
        on_toggle: ChecklistToggle,
        cx: &mut gpui::Context<Self>,
    ) -> Div {
        let checked_count = items.iter().filter(|i| checked_items.contains(*i)).count();
        let enabled = !self.applying;
        let bulk = |label: String, key: usize, value: bool| {
            let on_toggle = on_toggle.clone();
            let items = items.to_vec();
            div()
                .id(("security-bulk", id_key * 2 + key))
                .px(px(Theme::scaled(4.0)))
                .text_color(Theme::accent())
                .when(enabled, |d| {
                    d.cursor_pointer()
                        .hover(|s| s.underline())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            for item in &items {
                                on_toggle(this, item, value, cx);
                            }
                        }))
                })
                .child(label)
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(6.0)))
            .child(section_title(&title))
            .child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size() * 0.92))
                    .child({
                        let checked = checked_count.to_string();
                        let total = items.len().to_string();
                        localized_format(
                            "explorer.security.checklist.selected_count",
                            &[("checked", &checked), ("total", &total)],
                        )
                    }),
            )
            .child(div().flex_1())
            .when(items.len() > 1, |d| {
                d.child(bulk(
                    crate::localization::text("explorer.security.checklist.all"),
                    0,
                    true,
                ))
                .child(bulk(
                    crate::localization::text("explorer.security.checklist.none"),
                    1,
                    false,
                ))
            });

        let mut grid = div()
            .id(("security-checklist", id_key))
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_x(px(Theme::scaled(12.0)))
            .gap_y(px(Theme::scaled(4.0)))
            .max_h(px(Theme::scaled(CHECKLIST_MAX_HEIGHT)))
            .overflow_y_scroll()
            .p(px(Theme::scaled(8.0)))
            .rounded(px(Theme::scaled(6.0)))
            .bg(Theme::panel_bg())
            .border_1()
            .border_color(Theme::border());
        if items.is_empty() {
            grid = grid.child(div().text_color(Theme::text_dim()).child(empty_message));
        } else {
            for (idx, item) in items.iter().enumerate() {
                let checked = checked_items.contains(item);
                let item_name = item.clone();
                let on_toggle = on_toggle.clone();
                grid = grid.child(
                    div().w(px(Theme::scaled(200.0))).overflow_hidden().child(
                        Checkbox::new(format!("security-check-{id_key}-{idx}"))
                            .checked(checked)
                            .label(item.clone())
                            .tab_stop(false)
                            .disabled(!enabled)
                            .on_click(cx.listener(move |this, new_checked: &bool, _, cx| {
                                on_toggle(this, &item_name, *new_checked, cx);
                            })),
                    ),
                );
            }
        }
        div()
            .flex()
            .flex_col()
            .gap(px(Theme::scaled(6.0)))
            .child(header)
            .child(grid)
            .when_some(note, |d, note| d.child(hint(&note)))
    }

    // ── SQL pane and status ─────────────────────────────────────────────

    fn render_sql_card(
        &self,
        statement_count: usize,
        problems: Vec<String>,
        clickhouse: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Div {
        let title = if clickhouse {
            crate::localization::text("explorer.security.sql.clickhouse_title")
        } else if self.pending_drop {
            let count = statement_count.to_string();
            localized_format("explorer.security.sql.drop_title", &[("count", &count)])
        } else if statement_count == 0 {
            crate::localization::text("explorer.security.sql.title")
        } else {
            let count = statement_count.to_string();
            localized_format(
                "explorer.security.sql.title_with_count",
                &[("count", &count)],
            )
        };
        let has_text = !self.sql_output_text.is_empty();
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(Theme::scaled(10.0)))
            .child(section_title_owned(title))
            .child(div().flex_1())
            .when(!clickhouse, |d| {
                d.child(
                    Checkbox::new("security-show-sql-passwords")
                        .checked(self.show_sql_passwords)
                        .label(crate::localization::text(
                            "explorer.security.sql.show_passwords",
                        ))
                        .tab_stop(false)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.show_sql_passwords = *checked;
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("security-copy-sql")
                    .small()
                    .outline()
                    .label(crate::localization::text("explorer.security.sql.copy_all"))
                    .tooltip(if clickhouse {
                        crate::localization::text("explorer.security.sql.copy_script_tooltip")
                    } else {
                        crate::localization::text("explorer.security.sql.copy_sql_tooltip")
                    })
                    .disabled(!has_text)
                    .on_click(cx.listener(|this, _, _, cx| this.copy_all_sql(cx))),
            );

        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(Theme::scaled(6.0)))
            .child(header)
            .when(!problems.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(Theme::scaled(2.0)))
                        .px(px(Theme::scaled(10.0)))
                        .py(px(Theme::scaled(6.0)))
                        .rounded(px(Theme::scaled(6.0)))
                        .bg(Theme::warning().opacity(0.10))
                        .border_1()
                        .border_color(Theme::warning().opacity(0.35))
                        .children(problems.into_iter().map(|problem| {
                            div()
                                .flex()
                                .flex_row()
                                .gap(px(Theme::scaled(6.0)))
                                .child(div().text_color(Theme::warning()).child("⚠"))
                                .child(
                                    div()
                                        .whitespace_normal()
                                        .text_color(Theme::text())
                                        .child(problem),
                                )
                        })),
                )
            })
            .child(
                Textarea::new(&self.sql_output)
                    .readonly(true)
                    .h(px(Theme::scaled(SQL_HEIGHT)))
                    .font_family(Theme::editor_font_family())
                    .text_size(px(Theme::editor_font_size())),
            )
    }

    fn render_status_line(&self) -> Div {
        div()
            .flex()
            .flex_row()
            .items_start()
            .gap(px(Theme::scaled(6.0)))
            .min_h(px(Theme::row_height()))
            .child(
                div()
                    .mt(px(Theme::scaled(5.0)))
                    .size(px(Theme::scaled(7.0)))
                    .flex_none()
                    .rounded_full()
                    .bg(self.status_kind.color()),
            )
            .child(
                div()
                    .flex_1()
                    .whitespace_normal()
                    .text_color(self.status_kind.color())
                    .child(self.status.clone()),
            )
    }
}

const CH_READ_ONLY_STATUS: &str = "explorer.security.clickhouse.read_only_status";
const CH_READ_ONLY_DETAIL: &str = "explorer.security.clickhouse.read_only_detail";

/// The SQL pane text: each statement under a comment saying what it does.
fn preview_body(
    statements: &[PlannedStatement],
    admin: &dyn AdminToolsService,
    db: DatabaseType,
) -> String {
    statements
        .iter()
        .map(|s| {
            format!(
                "-- {}\n{};",
                s.summary,
                s.batch_text(admin, db).trim_end_matches(';')
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// How a SQL Server database user authenticates, as a list/header tag.
fn user_kind_tag(user: &UserEntry) -> (String, Hsla) {
    match user.auth_type.to_ascii_uppercase().as_str() {
        "DATABASE" => (
            crate::localization::text("explorer.security.tags.contained"),
            Theme::success(),
        ),
        "WINDOWS" => (
            crate::localization::text("explorer.security.tags.windows_user"),
            Theme::accent(),
        ),
        "EXTERNAL" => (
            crate::localization::text("explorer.security.tags.external_user"),
            Theme::accent(),
        ),
        "NONE" => (
            crate::localization::text("explorer.security.tags.no_login"),
            Theme::text_dim(),
        ),
        _ if user.login.is_none() => (
            crate::localization::text("explorer.security.tags.orphaned"),
            Theme::warning(),
        ),
        _ => (
            crate::localization::text("explorer.security.tags.login"),
            Theme::info(),
        ),
    }
}

/// A small outlined tag in `color`.
fn tag(text: String, color: Hsla) -> Div {
    div()
        .flex_none()
        .px(px(Theme::scaled(6.0)))
        .rounded(px(Theme::scaled(4.0)))
        .border_1()
        .border_color(color.opacity(0.45))
        .bg(color.opacity(0.10))
        .text_color(color)
        .text_size(px(Theme::font_size() * 0.82))
        .child(text)
}

/// Four little bars filled by strength.
fn strength_meter(strength: PasswordStrength, color: Hsla) -> Div {
    let filled = match strength {
        PasswordStrength::Weak => 1,
        PasswordStrength::Fair => 2,
        PasswordStrength::Strong => 4,
    };
    div()
        .flex()
        .flex_row()
        .gap(px(Theme::scaled(2.0)))
        .children((0..4).map(|i| {
            div()
                .w(px(Theme::scaled(14.0)))
                .h(px(Theme::scaled(4.0)))
                .rounded(px(Theme::scaled(2.0)))
                .bg(if i < filled { color } else { Theme::border() })
        }))
}

fn section_title(title: &str) -> Div {
    section_title_owned(title.to_string())
}

fn section_title_owned(title: String) -> Div {
    div()
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .text_color(Theme::text())
        .child(title)
}

/// A titled block in the editor.
fn section(title: String) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(Theme::scaled(8.0)))
        .child(section_title(&title))
}

fn field(label: String, control: AnyElement) -> Div {
    div()
        .flex()
        .flex_col()
        .gap(px(Theme::scaled(3.0)))
        .child(caption(label))
        .child(control)
}

fn caption(text: String) -> Div {
    div()
        .text_color(Theme::text_dim())
        .text_size(px(Theme::font_size() * 0.92))
        .child(text)
}

fn hint(text: &str) -> Div {
    div()
        .whitespace_normal()
        .text_color(Theme::text_dim())
        .text_size(px(Theme::font_size() * 0.92))
        .child(text.to_string())
}

fn list_message(message: &str) -> AnyElement {
    div()
        .flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p(px(Theme::scaled(12.0)))
        .text_center()
        .whitespace_normal()
        .text_color(Theme::text_dim())
        .child(message.to_string())
        .into_any_element()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::DefaultAdminToolsService;

    fn test_admin() -> DefaultAdminToolsService {
        DefaultAdminToolsService
    }

    fn s(value: &str) -> CellValue {
        CellValue::String(value.to_string())
    }

    fn login_form(name: &str) -> LoginForm {
        LoginForm {
            name: name.to_string(),
            ..LoginForm::default()
        }
    }

    // ── Parsing ─────────────────────────────────────────────────────────

    #[test]
    fn parse_logins_reads_kind_and_disabled_flag() {
        let rows = vec![
            vec![s("sa"), s("SQL_LOGIN"), CellValue::Bool(true)],
            vec![s("app"), s("SQL_LOGIN"), CellValue::I64(0)],
            vec![s("CONTOSO\\analyst"), s("WINDOWS_LOGIN"), CellValue::Null],
        ];
        let logins = parse_logins(&rows);
        assert_eq!(logins.len(), 3);
        assert!(logins[0].is_disabled);
        assert!(!logins[1].is_disabled);
        // NULL is_disabled must be tolerated, not panic.
        assert!(!logins[2].is_disabled);
        assert_eq!(logins[2].kind, "WINDOWS_LOGIN");
    }

    #[test]
    fn parse_logins_skips_blank_names() {
        let rows = vec![vec![s("  "), s("SQL_LOGIN")], vec![CellValue::Null]];
        assert!(parse_logins(&rows).is_empty());
    }

    #[test]
    fn parse_users_reads_login_and_schema() {
        let rows = vec![
            vec![
                s("report_reader"),
                s("SQL_USER"),
                s("INSTANCE"),
                s("dbo"),
                s("report_login"),
            ],
            vec![
                s("shell"),
                s("SQL_USER"),
                s("NONE"),
                CellValue::Null,
                CellValue::Null,
            ],
        ];
        let users = parse_users(&rows);
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].login.as_deref(), Some("report_login"));
        assert_eq!(users[0].default_schema.as_deref(), Some("dbo"));
        assert_eq!(users[1].login, None);
    }

    #[test]
    fn parse_names_trims_and_drops_empty() {
        let rows = vec![vec![s("db_datareader")], vec![s("")], vec![CellValue::Null]];
        assert_eq!(parse_names(&rows), vec!["db_datareader".to_string()]);
    }

    #[test]
    fn parse_db_user_mapping_pairs_columns() {
        let rows = vec![
            vec![s("Reporting"), s("analyst")],
            vec![s("Sales"), s("")],
            vec![s("")],
        ];
        let map = parse_db_user_mapping(&rows);
        assert_eq!(map.len(), 1);
        assert_eq!(map.get("Reporting").map(String::as_str), Some("analyst"));
    }

    // ── Login validation ────────────────────────────────────────────────

    fn assert_has_error(errors: &[String], key: &str) {
        let expected = crate::localization::text(key);
        assert!(
            errors.iter().any(|error| error == &expected),
            "missing {expected}: {errors:?}"
        );
    }

    fn assert_has_error_with_max(errors: &[String], key: &str) {
        let expected =
            crate::localization::format(key, &[("max", &MAX_IDENTIFIER_LEN.to_string())]);
        assert!(
            errors.iter().any(|error| error == &expected),
            "missing {expected}: {errors:?}"
        );
    }

    #[test]
    fn new_sql_login_needs_a_name_and_matching_password() {
        let existing = BTreeSet::new();
        let form = login_form("");
        assert_has_error(
            &validate_login(true, &existing, &form),
            "explorer.security.login_name_required",
        );

        let mut form = login_form("analyst");
        assert_has_error(
            &validate_login(true, &existing, &form),
            "explorer.security.sql_password_required",
        );

        form.password = "secret".to_string();
        form.password_confirm = "different".to_string();
        assert!(
            validate_login(true, &existing, &form)
                .iter()
                .any(|e| e.contains("do not match"))
        );

        form.password_confirm = "secret".to_string();
        assert!(validate_login(true, &existing, &form).is_empty());
    }

    #[test]
    fn duplicate_login_names_are_rejected() {
        let existing: BTreeSet<String> = ["analyst".to_string()].into_iter().collect();
        let form = login_form("analyst");
        assert!(
            validate_login(true, &existing, &form)
                .iter()
                .any(|e| e.contains("already exists"))
        );
        // Editing the same login is fine.
        assert!(
            !validate_login(false, &existing, &form)
                .iter()
                .any(|e| e.contains("already exists"))
        );
    }

    #[test]
    fn windows_logins_need_no_password() {
        let existing = BTreeSet::new();
        let form = LoginForm {
            name: "CONTOSO\\analyst".to_string(),
            auth: LoginAuth::Windows,
            ..LoginForm::default()
        };
        assert!(validate_login(true, &existing, &form).is_empty());
    }

    // ── Login diffing ───────────────────────────────────────────────────

    #[test]
    fn new_login_scripts_create_then_roles_then_users() {
        let mut form = login_form("analyst");
        form.password = "secret".to_string();
        form.server_roles.insert("dbcreator".to_string());
        form.db_access.insert("Reporting".to_string());

        let statements = diff_login(&test_admin(), None, &form);
        assert_eq!(statements.len(), 3);
        assert!(statements[0].sql.contains("CREATE LOGIN [analyst]"));
        assert!(statements[0].database.is_none());
        assert!(
            statements[1]
                .sql
                .contains("ALTER SERVER ROLE [dbcreator] ADD MEMBER [analyst]")
        );
        assert_eq!(statements[2].database.as_deref(), Some("Reporting"));
        assert!(
            statements[2]
                .sql
                .contains("CREATE USER [analyst] FOR LOGIN [analyst]")
        );
        // Database-scoped statements carry their own context switch.
        assert!(
            statements[2]
                .batch_text(&test_admin(), sqlly_core::DatabaseType::SqlServer)
                .starts_with("USE [Reporting]")
        );
    }

    #[test]
    fn new_windows_login_uses_from_windows() {
        let form = LoginForm {
            name: "CONTOSO\\analyst".to_string(),
            auth: LoginAuth::Windows,
            ..LoginForm::default()
        };
        let statements = diff_login(&test_admin(), None, &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.contains("FROM WINDOWS"));
        assert!(!statements[0].sql.contains("PASSWORD"));
    }

    #[test]
    fn unchanged_login_produces_no_statements() {
        let baseline = LoginBaseline {
            is_disabled: false,
            server_roles: ["dbcreator".to_string()].into_iter().collect(),
            db_users: [("Reporting".to_string(), "analyst".to_string())]
                .into_iter()
                .collect(),
        };
        let mut form = login_form("analyst");
        form.server_roles.insert("dbcreator".to_string());
        form.db_access.insert("Reporting".to_string());
        assert!(diff_login(&test_admin(), Some(&baseline), &form).is_empty());
    }

    #[test]
    fn password_reset_only_when_a_new_password_was_typed() {
        let baseline = LoginBaseline::default();
        let mut form = login_form("analyst");
        assert!(diff_login(&test_admin(), Some(&baseline), &form).is_empty());
        form.password = "new-secret".to_string();
        let statements = diff_login(&test_admin(), Some(&baseline), &form);
        assert_eq!(statements.len(), 1);
        assert!(
            statements[0]
                .sql
                .contains("ALTER LOGIN [analyst] WITH PASSWORD")
        );
    }

    #[test]
    fn enable_disable_flips_against_the_baseline() {
        let baseline = LoginBaseline {
            is_disabled: true,
            ..LoginBaseline::default()
        };
        let mut form = login_form("analyst");
        form.enabled = true;
        let statements = diff_login(&test_admin(), Some(&baseline), &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.contains("ENABLE"));

        form.enabled = false;
        let baseline = LoginBaseline::default();
        let statements = diff_login(&test_admin(), Some(&baseline), &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.contains("DISABLE"));
    }

    #[test]
    fn role_and_access_changes_diff_both_ways() {
        let baseline = LoginBaseline {
            is_disabled: false,
            server_roles: ["dbcreator".to_string(), "diskadmin".to_string()]
                .into_iter()
                .collect(),
            db_users: [("Old".to_string(), "analyst".to_string())]
                .into_iter()
                .collect(),
        };
        let mut form = login_form("analyst");
        form.server_roles.insert("dbcreator".to_string());
        form.server_roles.insert("securityadmin".to_string());
        form.db_access.insert("New".to_string());

        let statements = diff_login(&test_admin(), Some(&baseline), &form);
        let sql: Vec<&str> = statements.iter().map(|s| s.sql.as_str()).collect();
        assert!(
            sql.iter()
                .any(|q| q.contains("ADD MEMBER [analyst]") && q.contains("securityadmin"))
        );
        assert!(
            sql.iter()
                .any(|q| q.contains("DROP MEMBER [analyst]") && q.contains("diskadmin"))
        );
        assert!(
            sql.iter()
                .any(|q| q.contains("CREATE USER [analyst] FOR LOGIN [analyst]"))
        );
        assert!(sql.iter().any(|q| q.contains("DROP USER [analyst]")));
        // Revoking access runs inside the database it was granted in.
        let drop = statements
            .iter()
            .find(|s| s.sql.contains("DROP USER"))
            .unwrap();
        assert_eq!(drop.database.as_deref(), Some("Old"));
    }

    // ── User validation & diffing ───────────────────────────────────────

    #[test]
    fn new_user_needs_a_name_and_an_existing_login() {
        let existing = BTreeSet::new();
        let logins = vec!["analyst".to_string()];
        let form = UserForm::default();
        let errors = validate_user(true, &existing, &logins, &form);
        assert_has_error(&errors, "explorer.security.user_name_required");
        assert_has_error(&errors, "explorer.security.user_login_required");

        let form = UserForm {
            name: "reader".to_string(),
            login: "ghost".to_string(),
            ..UserForm::default()
        };
        assert!(
            validate_user(true, &existing, &logins, &form)
                .iter()
                .any(|e| e.contains("does not exist"))
        );

        let form = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            ..UserForm::default()
        };
        assert!(validate_user(true, &existing, &logins, &form).is_empty());
    }

    #[test]
    fn new_user_scripts_create_in_the_scoped_database() {
        let mut form = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            default_schema: "sales".to_string(),
            ..UserForm::default()
        };
        form.roles.insert("db_datareader".to_string());
        let statements = diff_user(&test_admin(), None, "Reporting", &form);
        assert_eq!(statements.len(), 2);
        assert_eq!(statements[0].database.as_deref(), Some("Reporting"));
        assert!(
            statements[0]
                .sql
                .contains("CREATE USER [reader] FOR LOGIN [analyst] WITH DEFAULT_SCHEMA = [sales]")
        );
        assert!(
            statements[1]
                .sql
                .contains("ALTER ROLE [db_datareader] ADD MEMBER [reader]")
        );
    }

    #[test]
    fn user_schema_and_role_changes_diff_against_baseline() {
        let baseline = UserBaseline {
            default_schema: Some("dbo".to_string()),
            roles: ["db_datareader".to_string()].into_iter().collect(),
            password_target: UserPasswordTarget::None,
        };
        let mut form = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            default_schema: "sales".to_string(),
            ..UserForm::default()
        };
        form.roles.insert("db_datawriter".to_string());
        let statements = diff_user(&test_admin(), Some(&baseline), "Reporting", &form);
        let sql: Vec<&str> = statements.iter().map(|s| s.sql.as_str()).collect();
        assert!(
            sql.iter()
                .any(|q| q.contains("WITH DEFAULT_SCHEMA = [sales]"))
        );
        assert!(
            sql.iter()
                .any(|q| q.contains("ADD MEMBER [reader]") && q.contains("db_datawriter"))
        );
        assert!(
            sql.iter()
                .any(|q| q.contains("DROP MEMBER [reader]") && q.contains("db_datareader"))
        );
        assert!(
            statements
                .iter()
                .all(|s| s.database.as_deref() == Some("Reporting"))
        );
    }

    #[test]
    fn unchanged_user_produces_no_statements() {
        let baseline = UserBaseline {
            default_schema: Some("dbo".to_string()),
            roles: ["db_datareader".to_string()].into_iter().collect(),
            password_target: UserPasswordTarget::None,
        };
        let mut form = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            default_schema: "dbo".to_string(),
            ..UserForm::default()
        };
        form.roles.insert("db_datareader".to_string());
        assert!(diff_user(&test_admin(), Some(&baseline), "Reporting", &form).is_empty());
    }

    fn user_form(name: &str, auth: UserAuth, password: &str) -> UserForm {
        UserForm {
            name: name.to_string(),
            auth,
            password: password.to_string(),
            password_confirm: password.to_string(),
            default_schema: "dbo".to_string(),
            ..UserForm::default()
        }
    }

    #[test]
    fn new_user_with_new_login_creates_login_then_user() {
        let form = user_form("reader", UserAuth::NewLogin, "S3cret!pw");
        assert!(validate_user(true, &BTreeSet::new(), &[], &form).is_empty());
        let statements = diff_user(&test_admin(), None, "Reporting", &form);
        assert_eq!(statements.len(), 2);
        assert!(statements[0].database.is_none());
        assert!(
            statements[0]
                .sql
                .contains("CREATE LOGIN [reader] WITH PASSWORD = N'S3cret!pw'")
        );
        assert_eq!(statements[1].database.as_deref(), Some("Reporting"));
        assert!(
            statements[1]
                .sql
                .contains("CREATE USER [reader] FOR LOGIN [reader]")
        );
    }

    #[test]
    fn new_login_user_needs_password_and_a_free_login_name() {
        let form = user_form("reader", UserAuth::NewLogin, "");
        let errors = validate_user(true, &BTreeSet::new(), &[], &form);
        assert_has_error(&errors, "explorer.security.user_new_password_required");

        let form = user_form("reader", UserAuth::NewLogin, "pw");
        let errors = validate_user(true, &BTreeSet::new(), &["Reader".to_string()], &form);
        assert!(errors.iter().any(|e| e.contains("already exists")));

        let mut form = user_form("reader", UserAuth::NewLogin, "pw");
        form.password_confirm = "other".to_string();
        let errors = validate_user(true, &BTreeSet::new(), &[], &form);
        assert!(errors.iter().any(|e| e.contains("do not match")));
    }

    #[test]
    fn contained_and_loginless_users_script_without_a_login() {
        let form = user_form("app", UserAuth::Contained, "pw");
        assert!(validate_user(true, &BTreeSet::new(), &[], &form).is_empty());
        let statements = diff_user(&test_admin(), None, "Sales", &form);
        assert_eq!(statements.len(), 1);
        assert!(
            statements[0]
                .sql
                .contains("CREATE USER [app] WITH PASSWORD = N'pw', DEFAULT_SCHEMA = [dbo]")
        );

        let form = user_form("owner", UserAuth::NoLogin, "");
        assert!(validate_user(true, &BTreeSet::new(), &[], &form).is_empty());
        let statements = diff_user(&test_admin(), None, "Sales", &form);
        assert!(statements[0].sql.contains("WITHOUT LOGIN"));
    }

    #[test]
    fn editing_a_user_resets_the_password_where_it_lives() {
        let mut form = user_form("reader", UserAuth::ExistingLogin, "n3w!");
        form.login = "report_login".to_string();

        let contained = UserBaseline {
            default_schema: Some("dbo".to_string()),
            password_target: UserPasswordTarget::Contained,
            ..UserBaseline::default()
        };
        let statements = diff_user(&test_admin(), Some(&contained), "Sales", &form);
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0].database.as_deref(), Some("Sales"));
        assert!(
            statements[0]
                .sql
                .contains("ALTER USER [reader] WITH PASSWORD")
        );

        let mapped = UserBaseline {
            default_schema: Some("dbo".to_string()),
            password_target: UserPasswordTarget::Login("report_login".to_string()),
            ..UserBaseline::default()
        };
        let statements = diff_user(&test_admin(), Some(&mapped), "Sales", &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].database.is_none());
        assert!(
            statements[0]
                .sql
                .contains("ALTER LOGIN [report_login] WITH PASSWORD")
        );

        // Windows/external users have nowhere to put a password.
        let errors = validate_user_with_target(
            false,
            &BTreeSet::new(),
            &[],
            &form,
            Some(&UserPasswordTarget::None),
        );
        assert_has_error(&errors, "explorer.security.user_password_not_applicable");
    }

    #[test]
    fn password_target_follows_auth_type_and_login_kind() {
        let logins = vec![
            LoginEntry {
                name: "app".to_string(),
                kind: "SQL_LOGIN".to_string(),
                is_disabled: false,
            },
            LoginEntry {
                name: "CONTOSO\\bob".to_string(),
                kind: "WINDOWS_LOGIN".to_string(),
                is_disabled: false,
            },
        ];
        let user = |auth: &str, login: Option<&str>| UserEntry {
            name: "u".to_string(),
            kind: "SQL_USER".to_string(),
            auth_type: auth.to_string(),
            login: login.map(str::to_string),
            default_schema: None,
        };
        assert_eq!(
            UserPasswordTarget::for_user(&user("DATABASE", None), &logins),
            UserPasswordTarget::Contained
        );
        assert_eq!(
            UserPasswordTarget::for_user(&user("INSTANCE", Some("app")), &logins),
            UserPasswordTarget::Login("app".to_string())
        );
        assert_eq!(
            UserPasswordTarget::for_user(&user("WINDOWS", Some("CONTOSO\\bob")), &logins),
            UserPasswordTarget::None
        );
        assert_eq!(
            UserPasswordTarget::for_user(&user("NONE", None), &logins),
            UserPasswordTarget::None
        );
    }

    #[test]
    fn sql_pane_text_ends_each_statement() {
        let form = user_form("app", UserAuth::Contained, "pw");
        let statements = diff_user(&test_admin(), None, "Sales", &form);
        let text = preview_body(
            &statements,
            &test_admin(),
            sqlly_core::DatabaseType::SqlServer,
        );
        assert!(text.starts_with("-- create contained user app in Sales\nUSE [Sales];"));
        assert!(text.trim_end().ends_with(';'));
    }

    // ── Hardening ───────────────────────────────────────────────────────

    #[test]
    fn duplicate_login_check_is_case_insensitive() {
        let existing: BTreeSet<String> = ["Analyst".to_string()].into_iter().collect();
        let form = login_form("analyst");
        assert!(
            validate_login(true, &existing, &form)
                .iter()
                .any(|e| e.contains("already exists"))
        );
    }

    #[test]
    fn windows_logins_must_be_domain_qualified() {
        let existing = BTreeSet::new();
        let mut form = login_form("analyst");
        form.auth = LoginAuth::Windows;
        assert!(
            validate_login(true, &existing, &form)
                .iter()
                .any(|e| e.contains("DOMAIN\\user"))
        );

        form.name = "CONTOSO\\analyst".to_string();
        assert!(validate_login(true, &existing, &form).is_empty());
    }

    #[test]
    fn overlong_identifiers_are_rejected() {
        let existing = BTreeSet::new();
        let long_name = "a".repeat(MAX_IDENTIFIER_LEN + 1);
        let form = login_form(&long_name);
        assert_has_error_with_max(
            &validate_login(true, &existing, &form),
            "explorer.security.login_name_too_long",
        );

        let mut form = login_form("analyst");
        form.password = "x".repeat(MAX_IDENTIFIER_LEN + 1);
        form.password_confirm = form.password.clone();
        assert_has_error_with_max(
            &validate_login(true, &existing, &form),
            "explorer.security.password_too_long",
        );

        let long_schema = "s".repeat(MAX_IDENTIFIER_LEN + 1);
        let user = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            default_schema: long_schema,
            ..UserForm::default()
        };
        assert_has_error_with_max(
            &validate_user(true, &BTreeSet::new(), &["analyst".to_string()], &user),
            "explorer.security.schema_name_too_long",
        );
    }

    #[test]
    fn user_validation_accepts_login_case_insensitively() {
        let known = vec!["Analyst".to_string()];
        let user = UserForm {
            name: "reader".to_string(),
            login: "analyst".to_string(),
            default_schema: "dbo".to_string(),
            ..UserForm::default()
        };
        assert!(validate_user(true, &BTreeSet::new(), &known, &user).is_empty());
    }

    #[test]
    fn one_line_collapses_multiline_server_errors() {
        let raw = "Msg 15151, Level 16, State 1, Line 2\r\nCannot alter the login 'sa'.\r\nThe login is disabled.";
        assert_eq!(
            one_line(raw),
            "Msg 15151, Level 16, State 1, Line 2 Cannot alter the login 'sa'. The login is disabled."
        );
        assert_eq!(one_line("single"), "single");
        assert_eq!(one_line("  padded   gaps  "), "padded gaps");
    }

    #[test]
    fn unicode_names_validate_and_diff() {
        // Emoji and CJK are legal inside bracket-quoted identifiers; nothing
        // in the model may panic or miscount them.
        let existing = BTreeSet::new();
        let mut form = login_form("分析者-🔒");
        form.password = "pässwörd✓".to_string();
        form.password_confirm = "pässwörd✓".to_string();
        assert!(validate_login(true, &existing, &form).is_empty());
        let statements = diff_login(&test_admin(), None, &form);
        assert_eq!(statements.len(), 1);
        assert!(statements[0].sql.contains("[分析者-🔒]"));
        // 129 *chars* is too long even though it is fewer bytes than ASCII.
        let long = "あ".repeat(MAX_IDENTIFIER_LEN + 1);
        assert_has_error_with_max(
            &validate_login(true, &existing, &login_form(&long)),
            "explorer.security.login_name_too_long",
        );
    }

    #[test]
    fn apply_policy_blocks_when_connection_is_select_only() {
        let secret = sqlly_core::SecretRef {
            provider: sqlly_core::SecretProvider::InMemory,
            key: "k".to_string(),
        };
        let mut profile = ConnectionProfile::sql_login("test", "localhost", "sa", secret);
        profile.block_all_but_select = true;
        let err =
            sqlly_intel::query::safety::apply_connection_policy_for(&profile, "CREATE ROLE \"x\"")
                .unwrap_err();
        assert!(err.to_string().contains("SELECT"));
    }

    #[test]
    fn clickhouse_tab_strip_is_users_and_roles() {
        assert_eq!(
            tabs_for(Some(SecurityDialect::ClickHouse), false),
            vec![SecurityTab::ChUsers, SecurityTab::ChRoles]
        );
    }

    #[test]
    fn pg_tab_strip_shows_roles_only() {
        assert_eq!(
            tabs_for(Some(SecurityDialect::Postgres), false),
            vec![SecurityTab::Roles]
        );
    }

    /// Oracle users and roles are managed on the shared Roles tab.
    #[test]
    fn oracle_tab_strip_shows_roles_only() {
        assert_eq!(
            tabs_for(Some(SecurityDialect::Oracle), false),
            vec![SecurityTab::Roles]
        );
    }

    #[test]
    fn mysql_tab_strip_shows_accounts_and_roles_on_mysql8() {
        assert_eq!(
            tabs_for(Some(SecurityDialect::MySql), true),
            vec![SecurityTab::Accounts, SecurityTab::MyRoles]
        );
        assert_eq!(
            tabs_for(Some(SecurityDialect::MariaDb), false),
            vec![SecurityTab::Accounts]
        );
    }
}
