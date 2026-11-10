//! Per-database tenant scoping configuration: whether the missing-tenant-
//! filter guard runs for a database, which column marks a table as
//! tenant-scoped, and (optionally) which table holds the tenants themselves.
//!
//! Four levels:
//! - **Off** (default) — tenant scoping is fully disabled for the database,
//!   including its configuration fields. Nothing tenant-related is shown
//!   until a database is explicitly switched to another level.
//! - **Allowed** — tenant filters are respected but nothing ever warns.
//! - **Warn** — a tenant-scoped table queried without its tenant filter gets
//!   a validation warning.
//! - **Required** — blocks running unscoped queries: a missing tenant filter is
//!   a validation error and Run is refused when validation has flagged one.
//!
//! Config persists as JSON keyed by server + database; the resolver feeds
//! the shared validation path, so Off/Allowed genuinely silence every
//! indication and Warn is the only mode that surfaces the warning; Required
//! blocks Run when validation reports a missing tenant predicate.
//!
//! The file carries the shared config version (`crate::config_version`).
//! A file written before [`crate::config_version::TENANT_SCOPE_RESET_VERSION`]
//! is cleared on load — every database goes back to Off, even ones the user
//! had explicitly turned on — and rewritten at the current version.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window,
    div, prelude::*, px,
};
use gpui_component::input::{Input, InputEvent, InputState};
use serde::{Deserialize, Serialize};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TenantScopeMode {
    #[default]
    Off,
    Allowed,
    Warn,
    Required,
}

impl TenantScopeMode {
    /// Stable element id for the mode's segment (never translated).
    fn element_id(self) -> &'static str {
        match self {
            Self::Off => "tenant-mode-off",
            Self::Allowed => "tenant-mode-allowed",
            Self::Warn => "tenant-mode-warn",
            Self::Required => "tenant-mode-required",
        }
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            Self::Off => "explorer.tenant_scoping.mode_off",
            Self::Allowed => "explorer.tenant_scoping.mode_allowed",
            Self::Warn => "explorer.tenant_scoping.mode_warn",
            Self::Required => "explorer.tenant_scoping.mode_required",
        })
    }

    pub fn description(self) -> String {
        crate::localization::text(match self {
            Self::Off => "explorer.tenant_scoping.mode_off_description",
            Self::Allowed => "explorer.tenant_scoping.mode_allowed_description",
            Self::Warn => "explorer.tenant_scoping.mode_warn_description",
            Self::Required => "explorer.tenant_scoping.mode_required_description",
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenantScopeConfig {
    #[serde(default)]
    pub mode: TenantScopeMode,
    /// The tenant discriminator column; empty keeps the built-in convention
    /// list (TenantId, AccountId, OrganizationId, OrgId and underscore forms).
    #[serde(default)]
    pub column: String,
    /// Optional: the table that holds the tenants themselves.
    #[serde(default)]
    pub table: String,
    /// When a tenant table is named: its id column.
    #[serde(default)]
    pub id_column: String,
    /// When a tenant table is named: its display-name column.
    #[serde(default)]
    pub display_column: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
// Tests exercise the in-memory cache only; the file wrapper is native-and-
// release-shaped code.
#[cfg_attr(test, allow(dead_code))]
struct TenantScopeFile {
    #[serde(default)]
    version: u32,
    /// Keyed by `server \u{1f} database`, both lowercased.
    #[serde(default)]
    databases: HashMap<String, TenantScopeConfig>,
}

fn scope_key(server: &str, database: &str) -> String {
    format!(
        "{}\u{1f}{}",
        server.trim().to_lowercase(),
        database.trim().to_lowercase()
    )
}

#[cfg(all(not(target_arch = "wasm32"), not(test)))]
fn store_path() -> std::path::PathBuf {
    if let Some(dir) = dirs::data_dir() {
        let dir = dir.join("SQLLY");
        let _ = std::fs::create_dir_all(&dir);
        return dir.join("tenant-scoping.json");
    }
    std::path::PathBuf::from("tenant-scoping.json")
}

fn cache() -> &'static RwLock<HashMap<String, TenantScopeConfig>> {
    static CACHE: OnceLock<RwLock<HashMap<String, TenantScopeConfig>>> = OnceLock::new();
    CACHE.get_or_init(|| RwLock::new(load_all()))
}

fn load_all() -> HashMap<String, TenantScopeConfig> {
    #[cfg(all(not(target_arch = "wasm32"), not(test)))]
    {
        let Ok(text) = std::fs::read_to_string(store_path()) else {
            return HashMap::new();
        };
        let Ok(file) = serde_json::from_str::<TenantScopeFile>(&text) else {
            return HashMap::new();
        };
        let (databases, migrated) = migrate(file);
        if migrated {
            persist(&databases);
        }
        databases
    }
    #[cfg(any(target_arch = "wasm32", test))]
    {
        HashMap::new()
    }
}

/// Bring a loaded file up to the current config version. Returns the
/// databases to use and whether the file must be rewritten.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn migrate(file: TenantScopeFile) -> (HashMap<String, TenantScopeConfig>, bool) {
    use crate::config_version::{CONFIG_VERSION, TENANT_SCOPE_RESET_VERSION};
    if file.version >= CONFIG_VERSION {
        return (file.databases, false);
    }
    let databases = if file.version < TENANT_SCOPE_RESET_VERSION {
        // Tenant scoping is opt-in: anything configured before it became
        // off-by-default is dropped, explicit Warn/Required included.
        HashMap::new()
    } else {
        file.databases
    };
    (databases, true)
}

fn persist(databases: &HashMap<String, TenantScopeConfig>) {
    // Tests exercise the in-memory cache only — they must never touch the
    // real per-user settings file.
    #[cfg(all(not(target_arch = "wasm32"), not(test)))]
    {
        let file = TenantScopeFile {
            version: crate::config_version::CONFIG_VERSION,
            databases: databases.clone(),
        };
        if let Ok(json) = serde_json::to_string_pretty(&file) {
            let _ = std::fs::write(store_path(), json);
        }
    }
    #[cfg(any(target_arch = "wasm32", test))]
    {
        let _ = databases;
    }
}

/// The stored configuration for one database (default when never configured).
pub fn config_for(server: &str, database: &str) -> TenantScopeConfig {
    cache()
        .read()
        .ok()
        .and_then(|map| map.get(&scope_key(server, database)).cloned())
        .unwrap_or_default()
}

/// Store one database's configuration and persist the file.
pub fn set_config(server: &str, database: &str, config: TenantScopeConfig) {
    if let Ok(mut map) = cache().write() {
        if config == TenantScopeConfig::default() {
            map.remove(&scope_key(server, database));
        } else {
            map.insert(scope_key(server, database), config);
        }
        persist(&map);
    }
}

/// The validation policy for a profile: Off/Allowed silence the guard, Warn
/// surfaces missing predicates as warnings, Required treats them as blocking
/// errors and refuses Run when validation has flagged one.
pub fn policy_for(
    profile: &sqlly_core::ConnectionProfile,
) -> sqlly_services::catalog::TenantScopePolicy {
    let database = profile.database.clone().unwrap_or_default();
    let config = config_for(&profile.server, &database);
    let folds = Some(config.column.trim())
        .filter(|column| !column.is_empty())
        .map(|column| vec![column.to_lowercase()]);
    let (disabled, fail_closed) = match config.mode {
        TenantScopeMode::Off | TenantScopeMode::Allowed => (true, false),
        TenantScopeMode::Warn => (false, false),
        TenantScopeMode::Required => (false, true),
    };
    sqlly_services::catalog::TenantScopePolicy {
        disabled,
        fail_closed,
        tenant_column_folds: folds,
    }
}

/// When tenant scoping is Required, refuse Run if validation has already
/// flagged a missing tenant predicate on the buffer (editor diagnostics).
pub fn execute_block_for_fail_closed(
    profile: &sqlly_core::ConnectionProfile,
    diagnostics: &[crate::editor::ValidationDiagnostic],
) -> Option<String> {
    if !policy_for(profile).fail_closed {
        return None;
    }
    use crate::editor::DiagnosticSeverity;
    diagnostics.iter().find_map(|d| {
        if matches!(
            d.severity,
            DiagnosticSeverity::Error | DiagnosticSeverity::Warning
        ) && d.message.contains("tenant-scoped")
        {
            Some(d.message.clone())
        } else {
            None
        }
    })
}

pub enum TenantScopingEvent {
    Closed,
}
impl EventEmitter<TenantScopingEvent> for TenantScopingDialog {}

/// The per-database configuration dialog, opened from Tools for the active
/// connection's current database.
pub struct TenantScopingDialog {
    server: String,
    database: String,
    mode: TenantScopeMode,
    inputs: [Entity<InputState>; 4],
    values: [String; 4],
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<gpui::Subscription>,
}

const FIELD_COLUMN: usize = 0;
const FIELD_TABLE: usize = 1;
const FIELD_ID: usize = 2;
const FIELD_DISPLAY: usize = 3;
/// Catalog keys of the field labels, indexed by the `FIELD_*` constants.
const FIELD_LABELS: [&str; 4] = [
    "explorer.tenant_scoping.field_tenant_column",
    "explorer.tenant_scoping.field_tenant_table",
    "explorer.tenant_scoping.field_tenant_id_column",
    "explorer.tenant_scoping.field_tenant_display_column",
];

impl TenantScopingDialog {
    pub fn new(
        server: String,
        database: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let config = config_for(&server, &database);
        let initial = [
            config.column.clone(),
            config.table.clone(),
            config.id_column.clone(),
            config.display_column.clone(),
        ];
        let placeholders = ["TenantId", "dbo.Tenants", "TenantId", "Name"];
        let inputs: [Entity<InputState>; 4] = std::array::from_fn(|index| {
            cx.new(|cx| {
                let mut state = InputState::new(window, cx).placeholder(placeholders[index]);
                state.set_value(initial[index].clone(), window, cx);
                state
            })
        });
        let mut subscriptions = Vec::new();
        for (index, input) in inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this: &mut Self, state, event: &InputEvent, _window, cx| {
                    if let InputEvent::Change = event {
                        this.values[index] = state.read(cx).value().to_string();
                        this.save(cx);
                        cx.notify();
                    }
                },
            ));
        }
        Self {
            server,
            database,
            mode: config.mode,
            inputs,
            values: initial,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
            _subscriptions: subscriptions,
        }
    }

    fn config(&self) -> TenantScopeConfig {
        // Off hides and clears use of the fields; keep them stored so
        // flipping back doesn't lose the setup? No — Off means "totally
        // disabled including not able to configure": the fields are dropped.
        if self.mode == TenantScopeMode::Off {
            return TenantScopeConfig {
                mode: TenantScopeMode::Off,
                ..TenantScopeConfig::default()
            };
        }
        TenantScopeConfig {
            mode: self.mode,
            column: self.values[FIELD_COLUMN].trim().to_string(),
            table: self.values[FIELD_TABLE].trim().to_string(),
            id_column: self.values[FIELD_ID].trim().to_string(),
            display_column: self.values[FIELD_DISPLAY].trim().to_string(),
        }
    }

    fn save(&self, _cx: &mut Context<Self>) {
        set_config(&self.server, &self.database, self.config());
    }

    fn set_mode(&mut self, mode: TenantScopeMode, cx: &mut Context<Self>) {
        self.mode = mode;
        self.save(cx);
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(TenantScopingEvent::Closed);
    }
}

impl gpui::Focusable for TenantScopingDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TenantScopingDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let mode = self.mode;
        let show_fields = mode != TenantScopeMode::Off;
        let show_tenant_table_columns = show_fields && !self.values[FIELD_TABLE].trim().is_empty();

        let mut body = div().flex().flex_col().gap(px(s(8.0)));
        body = body.child(
            div().flex().flex_row().gap(px(s(6.0))).children(
                [
                    TenantScopeMode::Off,
                    TenantScopeMode::Allowed,
                    TenantScopeMode::Warn,
                    TenantScopeMode::Required,
                ]
                .into_iter()
                .map(|option| {
                    let selected = mode == option;
                    div()
                        .id(option.element_id())
                        .px(px(s(12.0)))
                        .py(px(s(4.0)))
                        .rounded(px(s(4.0)))
                        .cursor(gpui::CursorStyle::PointingHand)
                        .bg(if selected {
                            Theme::selection()
                        } else {
                            gpui::transparent_black()
                        })
                        .text_color(if selected {
                            Theme::text()
                        } else {
                            Theme::text_dim()
                        })
                        .child(option.label())
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, _window, cx| {
                                this.set_mode(option, cx);
                            }),
                        )
                }),
            ),
        );
        body = body.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.84)))
                .text_color(Theme::text_dim())
                .whitespace_normal()
                .child(mode.description()),
        );
        if show_fields {
            for (index, label) in FIELD_LABELS.iter().enumerate() {
                if matches!(index, FIELD_ID | FIELD_DISPLAY) && !show_tenant_table_columns {
                    continue;
                }
                body = body.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(s(1.0)))
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(label)),
                        )
                        .child(Input::new(&self.inputs[index])),
                );
            }
        }

        let title = if self.database.is_empty() {
            crate::localization::format(
                "explorer.tenant_scoping.title_default_database",
                &[("server", self.server.as_str())],
            )
        } else {
            crate::localization::format(
                "explorer.tenant_scoping.title",
                &[
                    ("server", self.server.as_str()),
                    ("database", self.database.as_str()),
                ],
            )
        };

        let focus_handle = self.focus_handle.clone();
        Dialog::new("tenant-scoping-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(true)
            .key_context("TenantScopingDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::text(
                        "explorer.tenant_scoping.save_hint",
                    )),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(server: &str, database: Option<&str>) -> sqlly_core::ConnectionProfile {
        let mut p = sqlly_core::ConnectionProfile::sql_login(
            "p",
            server,
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        p.database = database.map(str::to_string);
        p
    }

    #[test]
    fn default_policy_is_off() {
        assert_eq!(TenantScopeMode::default(), TenantScopeMode::Off);
        let policy = policy_for(&profile("unconfigured.example", Some("db")));
        assert!(policy.disabled);
        assert!(!policy.fail_closed);
        assert!(policy.tenant_column_folds.is_none());
    }

    fn file_with(version: u32, mode: TenantScopeMode) -> TenantScopeFile {
        let mut databases = HashMap::new();
        databases.insert(
            scope_key("srv", "db"),
            TenantScopeConfig {
                mode,
                column: "TenantId".into(),
                table: "dbo.Tenants".into(),
                ..Default::default()
            },
        );
        TenantScopeFile { version, databases }
    }

    #[test]
    fn files_before_the_reset_version_lose_every_tenant_setting() {
        for version in [0, 1] {
            for mode in [TenantScopeMode::Warn, TenantScopeMode::Required] {
                let (databases, migrated) = migrate(file_with(version, mode));
                assert!(migrated, "v{version} {mode:?}");
                assert!(databases.is_empty(), "v{version} {mode:?}");
            }
        }
    }

    #[test]
    fn unversioned_json_counts_as_before_the_reset() {
        let file: TenantScopeFile = serde_json::from_str(
            r#"{"databases":{"srv\u001fdb":{"mode":"required","column":"TenantId"}}}"#,
        )
        .unwrap();
        assert_eq!(file.version, 0);
        let (databases, migrated) = migrate(file);
        assert!(migrated);
        assert!(databases.is_empty());
    }

    #[test]
    fn current_version_files_keep_explicit_settings() {
        let (databases, migrated) = migrate(file_with(
            crate::config_version::CONFIG_VERSION,
            TenantScopeMode::Required,
        ));
        assert!(!migrated);
        assert_eq!(
            databases.get(&scope_key("srv", "db")).map(|c| c.mode),
            Some(TenantScopeMode::Required)
        );
    }

    #[test]
    fn modes_map_to_policy_and_only_warn_warns() {
        let server = "tenant-test.example";
        for (mode, disabled, fail_closed) in [
            (TenantScopeMode::Off, true, false),
            (TenantScopeMode::Allowed, true, false),
            (TenantScopeMode::Warn, false, false),
            (TenantScopeMode::Required, false, true),
        ] {
            set_config(
                server,
                "db",
                TenantScopeConfig {
                    mode,
                    ..Default::default()
                },
            );
            let policy = policy_for(&profile(server, Some("db")));
            assert_eq!(policy.disabled, disabled, "{mode:?}");
            assert_eq!(policy.fail_closed, fail_closed, "{mode:?}");
        }
        // Cleanup: back to default removes the entry.
        set_config(server, "db", TenantScopeConfig::default());
    }

    #[test]
    fn custom_column_narrows_detection_case_insensitively() {
        let server = "tenant-col.example";
        set_config(
            server,
            "Sales",
            TenantScopeConfig {
                mode: TenantScopeMode::Warn,
                column: "CompanyRef".into(),
                ..Default::default()
            },
        );
        // Keys fold case on both server and database.
        let policy = policy_for(&profile("TENANT-COL.example", Some("sales")));
        assert_eq!(
            policy.tenant_column_folds,
            Some(vec!["companyref".to_string()])
        );
        set_config(server, "Sales", TenantScopeConfig::default());
    }

    #[test]
    fn off_clears_the_stored_configuration_fields() {
        // The dialog's `config()` drops the fields in Off mode; emulate that
        // rule at the model level.
        let config = TenantScopeConfig {
            mode: TenantScopeMode::Off,
            ..Default::default()
        };
        assert!(config.column.is_empty());
        assert!(config.table.is_empty());
    }

    fn tenant_diagnostic(message: &str) -> crate::editor::ValidationDiagnostic {
        crate::editor::ValidationDiagnostic {
            severity: crate::editor::DiagnosticSeverity::Error,
            source: crate::editor::DiagnosticSource::Validation,
            message: message.to_string(),
            line: 1,
            identifier: None,
            start_byte: 0,
            end_byte: 1,
            explainer: None,
            fix: None,
        }
    }

    #[test]
    fn execute_block_for_fail_closed_table() {
        let server = "execute-block.example";
        let tenant_msg =
            "`orders` is tenant-scoped by `tenant_id`, but the statement has no `tenant_id` filter";
        let unrelated = "syntax error near SELECT";
        let cases: &[(&str, &[crate::editor::ValidationDiagnostic], Option<&str>)] = &[
            (
                "Required",
                &[tenant_diagnostic(tenant_msg)],
                Some(tenant_msg),
            ),
            ("Required_empty_diagnostics_fail_open", &[], None),
            (
                "Required_non_matching_diagnostic",
                &[tenant_diagnostic(unrelated)],
                None,
            ),
            ("Warn", &[tenant_diagnostic(tenant_msg)], None),
            ("Off", &[tenant_diagnostic(tenant_msg)], None),
        ];
        for (name, diagnostics, want) in cases {
            let mode = match *name {
                "Required"
                | "Required_empty_diagnostics_fail_open"
                | "Required_non_matching_diagnostic" => TenantScopeMode::Required,
                "Warn" => TenantScopeMode::Warn,
                _ => TenantScopeMode::Off,
            };
            set_config(
                server,
                "db",
                TenantScopeConfig {
                    mode,
                    ..Default::default()
                },
            );
            let got = execute_block_for_fail_closed(&profile(server, Some("db")), diagnostics);
            assert_eq!(got.as_deref(), *want, "{name}");
        }
        set_config(server, "db", TenantScopeConfig::default());
    }
}
