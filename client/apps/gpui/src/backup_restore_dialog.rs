//! Backup / Restore dialog — the first real Tools-menu admin dialog and the
//! template for the rest (see `tool_dialog.rs` for the shared contract).
//!
//! Script-first by design: the dialog builds `BACKUP`/`RESTORE` T-SQL from
//! its form state (via the tested `sqlly-admin` generators), shows the exact
//! SQL it would produce, and its primary action is **Script to Editor** — the
//! statement lands in a new query tab for review, it is never executed from
//! here. Databases are listed live from the dialog's captured connection
//! route; a profile switch after open cannot redirect it.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Subscription,
    Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_core::{ConnectionKind, DatabaseType};
use sqlly_services::admin::backup::BackupManager;
use sqlly_services::admin::restore::RestoreManager;
use sqlly_services::{AdminToolsService, BackupType};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

/// Which statement the form is building.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Backup,
    Restore,
}

/// Form state distilled to what the SQL generators need. Extracted from the
/// view so preview/validation are pure and unit-testable.
#[derive(Clone, Debug)]
pub struct BackupRestoreModel {
    pub mode: Mode,
    pub database: String,
    pub backup_type: BackupType,
    pub path: String,
    pub compress: bool,
    pub checksum: bool,
    pub copy_only: bool,
    pub replace: bool,
    pub recovery: bool,
    /// ClickHouse form: `path` is a backup name on the server's `backups`
    /// disk, `compress` asks for zstd, and `replace` allows restoring into
    /// tables that already hold rows; the other options do not apply.
    pub clickhouse: bool,
}

impl Default for BackupRestoreModel {
    fn default() -> Self {
        Self {
            mode: Mode::Backup,
            database: String::new(),
            backup_type: BackupType::Full,
            path: String::new(),
            compress: true,
            checksum: true,
            copy_only: false,
            replace: false,
            recovery: true,
            clickhouse: false,
        }
    }
}

/// The live database list behind the Database select, per engine.
fn database_list_sql(db: DatabaseType) -> &'static str {
    match db.connection_kind() {
        ConnectionKind::ClickHouseHttp => {
            "SELECT name FROM system.databases WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name"
        }
        _ => "SELECT name FROM sys.databases WHERE state = 0 ORDER BY name",
    }
}

impl BackupRestoreModel {
    /// The exact SQL the primary button will script.
    pub fn generate_sql(&self, admin: &dyn AdminToolsService) -> String {
        if self.clickhouse {
            return match self.mode {
                Mode::Backup => {
                    BackupManager::clickhouse_backup_sql(&self.database, &self.path, self.compress)
                }
                Mode::Restore => {
                    RestoreManager::clickhouse_restore_sql(&self.database, &self.path, self.replace)
                }
            };
        }
        match self.mode {
            Mode::Backup => admin.backup_sql(
                &self.database,
                &self.path,
                self.backup_type,
                self.compress,
                self.checksum,
                self.copy_only,
            ),
            Mode::Restore => {
                admin.restore_sql(&self.database, &self.path, self.replace, self.recovery)
            }
        }
    }

    /// Why the form cannot script yet, if anything.
    pub fn validate(&self, admin: &dyn AdminToolsService) -> Result<(), String> {
        if self.clickhouse {
            return BackupManager::validate_clickhouse(&self.database, &self.path);
        }
        match self.mode {
            Mode::Backup => admin.validate_backup(&self.database, &self.path),
            Mode::Restore => admin.validate_restore(&self.database, &self.path),
        }
    }

    /// Default destination path suggestion for a database.
    pub fn suggested_path(database: &str) -> String {
        if database.is_empty() {
            String::new()
        } else {
            format!("/var/opt/mssql/backup/{database}.bak")
        }
    }

    /// Default destination for the engine: a file path on SQL Server, a
    /// backup name on ClickHouse's `backups` disk.
    pub fn suggested_path_for(database: &str, clickhouse: bool) -> String {
        if database.is_empty() {
            String::new()
        } else if clickhouse {
            format!("{database}.zip")
        } else {
            Self::suggested_path(database)
        }
    }
}

impl EventEmitter<ToolDialogEvent> for BackupRestoreDialog {}

pub struct BackupRestoreDialog {
    context: ToolDialogContext,
    model: BackupRestoreModel,
    databases: Vec<String>,
    databases_loading: bool,
    load_error: Option<String>,
    db_select: Entity<SelectState<Vec<String>>>,
    path_input: Entity<InputState>,
    /// Select items need a `&mut Window`; loads mark this and render syncs.
    selects_dirty: bool,
    /// Path prefill requested (database changed) — applied at render.
    pending_path_prefill: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl BackupRestoreDialog {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let clickhouse = matches!(
            context.profile.database_type.connection_kind(),
            ConnectionKind::ClickHouseHttp
        );
        let db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let path_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                if clickhouse {
                    "explorer.backup_restore.clickhouse_path_placeholder"
                } else {
                    "explorer.backup_restore.sqlserver_path_placeholder"
                },
            ))
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
                            this.pending_path_prefill = Some(
                                BackupRestoreModel::suggested_path_for(&db, this.model.clickhouse),
                            );
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
        ];

        let mut this = Self {
            context,
            model: BackupRestoreModel {
                clickhouse,
                ..BackupRestoreModel::default()
            },
            databases: Vec::new(),
            databases_loading: true,
            load_error: None,
            db_select,
            path_input,
            selects_dirty: false,
            pending_path_prefill: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
            _subscriptions: subscriptions,
        };
        this.load_databases(cx);
        this
    }

    fn load_databases(&mut self, cx: &mut gpui::Context<Self>) {
        self.databases_loading = true;
        crate::tool_dialog::run_query(
            &self.context,
            database_list_sql(self.context.profile.database_type).to_string(),
            cx,
            |this, outcome, cx| {
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
                    // Preselect the profile's database (or the first listed).
                    let initial = this
                        .context
                        .profile
                        .database
                        .clone()
                        .filter(|db| this.databases.contains(db))
                        .or_else(|| this.databases.first().cloned());
                    if let Some(db) = initial {
                        this.pending_path_prefill = Some(BackupRestoreModel::suggested_path_for(
                            &db,
                            this.model.clickhouse,
                        ));
                        this.model.database = db;
                    }
                    this.selects_dirty = true;
                }
                cx.notify();
            },
        );
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let admin = &*self.context.services.admin_tools;
        if let Err(reason) = self.model.validate(admin) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        let sql = self.model.generate_sql(admin);
        cx.emit(ToolDialogEvent::ScriptToEditor { sql });
        cx.emit(ToolDialogEvent::Close);
    }

    /// Copy the `sqlly query --allow-writes` line that runs this BACKUP /
    /// RESTORE script, for cron or a runbook. SQL Server runs it from
    /// `master` so a restore never collides with its own session.
    fn copy_cli_command(&mut self, cx: &mut gpui::Context<Self>) {
        let admin = &*self.context.services.admin_tools;
        if let Err(reason) = self.model.validate(admin) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        let sql = self.model.generate_sql(admin);
        let database = (self.context.profile.database_type.connection_kind()
            == ConnectionKind::Tds)
            .then_some("master");
        let command = crate::cli_command::admin_sql_command(
            &crate::cli_command::cli_path(),
            &self.context.profile.name,
            database,
            &sql,
        );
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(command));
        cx.emit(ToolDialogEvent::Status {
            level: StatusLevel::Success,
            message: crate::cli_command::copied_status(),
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }

    fn checkbox(
        &self,
        id: &'static str,
        label: &'static str,
        checked: bool,
        set: fn(&mut BackupRestoreModel, bool),
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

impl gpui::Focusable for BackupRestoreDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BackupRestoreDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        // Window-needing sync work deferred from async loads.
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
        }
        if let Some(path) = self.pending_path_prefill.take() {
            self.model.path = path.clone();
            self.path_input
                .update(cx, |state, cx| state.set_value(path, window, cx));
        }

        let mode = self.model.mode;
        let admin = &*self.context.services.admin_tools;
        let validation = self.model.validate(admin).err();
        let preview = if validation.is_none() {
            self.model.generate_sql(admin)
        } else {
            String::new()
        };

        let mode_tab = |this_mode: Mode, label: &'static str, cx: &mut gpui::Context<Self>| {
            let active = mode == this_mode;
            div()
                .px(px(s(12.0)))
                .py(px(s(4.0)))
                .rounded(px(s(4.0)))
                .cursor_pointer()
                .bg(if active {
                    Theme::selection()
                } else {
                    gpui::transparent_black()
                })
                .text_color(if active {
                    Theme::text()
                } else {
                    Theme::text_dim()
                })
                .child(crate::localization::text(label))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        this.model.mode = this_mode;
                        cx.notify();
                    }),
                )
        };

        let type_tab =
            |this_type: BackupType, label: &'static str, cx: &mut gpui::Context<Self>| {
                let active = self.model.backup_type == this_type;
                div()
                    .px(px(s(10.0)))
                    .py(px(s(3.0)))
                    .rounded(px(s(4.0)))
                    .cursor_pointer()
                    .bg(if active {
                        Theme::selection()
                    } else {
                        gpui::transparent_black()
                    })
                    .text_color(if active {
                        Theme::text()
                    } else {
                        Theme::text_dim()
                    })
                    .child(crate::localization::text(label))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| {
                            this.model.backup_type = this_type;
                            cx.notify();
                        }),
                    )
            };

        let field = |label: &'static str, control: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(90.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(label)),
                )
                .child(div().flex_1().min_w(px(0.0)).child(control))
        };

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(4.0)))
                    .child(mode_tab(
                        Mode::Backup,
                        "explorer.backup_restore.backup_tab",
                        cx,
                    ))
                    .child(mode_tab(
                        Mode::Restore,
                        "explorer.backup_restore.restore_tab",
                        cx,
                    )),
            )
            .child(field(
                "explorer.backup_restore.database_label",
                if self.databases_loading {
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.backup_restore.databases_loading",
                        ))
                        .into_any_element()
                } else if let Some(error) = &self.load_error {
                    div()
                        .text_color(Theme::error())
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .child(crate::localization::format(
                            "explorer.backup_restore.databases_error",
                            &[("error", error)],
                        ))
                        .into_any_element()
                } else {
                    Select::new(&self.db_select).w_full().into_any_element()
                },
            ))
            .child(field(
                if self.model.clickhouse {
                    "explorer.backup_restore.backup_name_label"
                } else if mode == Mode::Backup {
                    "explorer.backup_restore.backup_to_label"
                } else {
                    "explorer.backup_restore.restore_from_label"
                },
                Input::new(&self.path_input).into_any_element(),
            ));

        if self.model.clickhouse {
            // ClickHouse: one whole-database backup to the `backups` disk;
            // the SQL Server type/checksum/recovery options do not apply.
            body = body
                .child(
                    div()
                        .pl(px(s(98.0)))
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .whitespace_normal()
                        .child(crate::localization::text(
                            "explorer.backup_restore.clickhouse_explanation",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(14.0)))
                        .pl(px(s(98.0)))
                        .child(if mode == Mode::Backup {
                            self.checkbox(
                                "br-compress",
                                "explorer.backup_restore.clickhouse_compression",
                                self.model.compress,
                                |m, v| m.compress = v,
                                cx,
                            )
                        } else {
                            self.checkbox(
                                "br-replace",
                                "explorer.backup_restore.clickhouse_replace",
                                self.model.replace,
                                |m, v| m.replace = v,
                                cx,
                            )
                        }),
                );
        } else if mode == Mode::Backup {
            body = body
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(4.0)))
                        .child(
                            div()
                                .w(px(s(90.0)))
                                .flex_shrink_0()
                                .text_size(px(Theme::font_size_ratio(0.87)))
                                .text_color(Theme::text_dim())
                                .child(crate::localization::text(
                                    "explorer.backup_restore.type_label",
                                )),
                        )
                        .child(type_tab(
                            BackupType::Full,
                            "explorer.backup_restore.type_full",
                            cx,
                        ))
                        .child(type_tab(
                            BackupType::Differential,
                            "explorer.backup_restore.type_differential",
                            cx,
                        ))
                        .child(type_tab(
                            BackupType::Log,
                            "explorer.backup_restore.type_log",
                            cx,
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(14.0)))
                        .pl(px(s(98.0)))
                        .child(self.checkbox(
                            "br-compress",
                            "explorer.backup_restore.compression",
                            self.model.compress,
                            |m, v| m.compress = v,
                            cx,
                        ))
                        .child(self.checkbox(
                            "br-checksum",
                            "explorer.backup_restore.checksum",
                            self.model.checksum,
                            |m, v| m.checksum = v,
                            cx,
                        ))
                        .child(self.checkbox(
                            "br-copyonly",
                            "explorer.backup_restore.copy_only",
                            self.model.copy_only,
                            |m, v| m.copy_only = v,
                            cx,
                        )),
                );
        } else {
            body = body.child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(14.0)))
                    .pl(px(s(98.0)))
                    .child(self.checkbox(
                        "br-replace",
                        "explorer.backup_restore.replace",
                        self.model.replace,
                        |m, v| m.replace = v,
                        cx,
                    ))
                    .child(self.checkbox(
                        "br-recovery",
                        "explorer.backup_restore.recovery",
                        self.model.recovery,
                        |m, v| m.recovery = v,
                        cx,
                    )),
            );
        }

        // SQL preview (or the validation reason it can't be generated yet).
        body = body.child(
            div()
                .mt(px(s(4.0)))
                .p(px(s(8.0)))
                .bg(Theme::sidebar_bg())
                .border_1()
                .border_color(Theme::border())
                .rounded(px(s(4.0)))
                .min_h(px(s(48.0)))
                .text_size(px(Theme::font_size_ratio(0.87)))
                .child(match &validation {
                    Some(reason) => div().text_color(Theme::text_dim()).child(reason.clone()),
                    None => div()
                        .text_color(Theme::kind_string())
                        .whitespace_normal()
                        .child(preview.clone()),
                }),
        );

        let focus_handle = self.focus_handle.clone();
        Dialog::new("backup-restore-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.backup_restore.title",
                &[("connection", &self.context.profile.name)],
            ))
            .key_context("BackupRestoreDialog")
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .leading_button(
                div()
                    .text_size(px(Theme::font_size_ratio(0.74)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "explorer.backup_restore.script_explanation",
                    )),
            )
            .button(
                Button::new("br-copy-cli")
                    .label(crate::localization::text(
                        "explorer.backup_restore.copy_cli_button",
                    ))
                    .disabled(validation.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| this.copy_cli_command(cx))),
            )
            .button(
                Button::new("br-script")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.backup_restore.script_button",
                    ))
                    .disabled(validation.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| this.script_to_editor(cx))),
            )
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_services::DefaultAdminToolsService;

    #[test]
    fn backup_model_generates_expected_sql() {
        let admin = DefaultAdminToolsService;
        let model = BackupRestoreModel {
            database: "Sales".into(),
            path: "/tmp/sales.bak".into(),
            ..Default::default()
        };
        let sql = model.generate_sql(&admin);
        assert!(sql.starts_with("BACKUP DATABASE [Sales] TO DISK = N'/tmp/sales.bak'"));
        assert!(sql.contains("COMPRESSION"));
        assert!(sql.contains("CHECKSUM"));
        assert!(!sql.contains("COPY_ONLY"));
    }

    #[test]
    fn restore_model_generates_expected_sql() {
        let admin = DefaultAdminToolsService;
        let model = BackupRestoreModel {
            mode: Mode::Restore,
            database: "Sales".into(),
            path: "/tmp/sales.bak".into(),
            replace: true,
            recovery: true,
            ..Default::default()
        };
        let sql = model.generate_sql(&admin);
        assert!(sql.starts_with("RESTORE DATABASE [Sales] FROM DISK = N'/tmp/sales.bak'"));
        assert!(sql.contains("REPLACE"));
        assert!(sql.contains("RECOVERY"));
    }

    #[test]
    fn log_backup_uses_log_clause() {
        let admin = DefaultAdminToolsService;
        let model = BackupRestoreModel {
            database: "Sales".into(),
            path: "/tmp/sales.trn".into(),
            backup_type: BackupType::Log,
            ..Default::default()
        };
        assert!(model.generate_sql(&admin).starts_with("BACKUP LOG [Sales]"));
    }

    #[test]
    fn validation_requires_database_and_path() {
        let admin = DefaultAdminToolsService;
        let model = BackupRestoreModel::default();
        assert!(model.validate(&admin).is_err());
        let model = BackupRestoreModel {
            database: "Sales".into(),
            path: "/tmp/sales.bak".into(),
            ..Default::default()
        };
        assert!(model.validate(&admin).is_ok());
    }

    #[test]
    fn copy_only_log_backup_is_accepted() {
        let admin = DefaultAdminToolsService;
        // BACKUP LOG ... WITH COPY_ONLY is valid T-SQL (a log backup that
        // doesn't truncate the chain); the old rejection was over-cautious.
        let model = BackupRestoreModel {
            database: "Sales".into(),
            path: "/tmp/sales.trn".into(),
            backup_type: BackupType::Log,
            copy_only: true,
            ..Default::default()
        };
        assert!(model.validate(&admin).is_ok());
        assert!(model.generate_sql(&admin).contains("COPY_ONLY"));
    }

    #[test]
    fn clickhouse_form_scripts_backup_and_restore_to_the_backups_disk() {
        let admin = DefaultAdminToolsService;
        let model = BackupRestoreModel {
            database: "shop".into(),
            path: "shop.zip".into(),
            clickhouse: true,
            ..Default::default()
        };
        assert!(model.validate(&admin).is_ok());
        assert_eq!(
            model.generate_sql(&admin),
            "BACKUP DATABASE `shop` TO Disk('backups', 'shop.zip') SETTINGS compression_method = 'zstd';"
        );
        let restore = BackupRestoreModel {
            mode: Mode::Restore,
            replace: true,
            ..model.clone()
        };
        assert_eq!(
            restore.generate_sql(&admin),
            "RESTORE DATABASE `shop` FROM Disk('backups', 'shop.zip') SETTINGS allow_non_empty_tables = true;"
        );
        let escaped = BackupRestoreModel {
            path: "../shop.zip".into(),
            ..model
        };
        assert!(escaped.validate(&admin).is_err());
        assert_eq!(
            BackupRestoreModel::suggested_path_for("shop", true),
            "shop.zip"
        );
        assert_eq!(
            BackupRestoreModel::suggested_path_for("Sales", false),
            "/var/opt/mssql/backup/Sales.bak"
        );
        assert!(database_list_sql(DatabaseType::ClickHouse).contains("system.databases"));
        assert!(database_list_sql(DatabaseType::SqlServer).contains("sys.databases"));
    }

    #[test]
    fn suggested_path_follows_database() {
        assert_eq!(
            BackupRestoreModel::suggested_path("Sales"),
            "/var/opt/mssql/backup/Sales.bak"
        );
        assert_eq!(BackupRestoreModel::suggested_path(""), "");
    }
}
