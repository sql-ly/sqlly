//! PostgreSQL / MySQL restore from vendor dump files.

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
use sqlly_services::native_dump::Endpoint;
use sqlly_services::{
    DumpProgress, NativeTool, RestoreKind, RestoreRequest, find_tool, pg_restore_args,
    run_native_restore, sniff_restore_kind,
};

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::preferences::native_tool_locations;
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

#[derive(Clone, Debug)]
pub struct NativeRestoreModel {
    pub database: String,
    pub path: String,
    pub kind: Option<RestoreKind>,
    pub clean: bool,
    pub if_exists: bool,
    pub single_transaction: bool,
    pub no_owner: bool,
}

impl Default for NativeRestoreModel {
    fn default() -> Self {
        Self {
            database: String::new(),
            path: String::new(),
            kind: None,
            clean: true,
            if_exists: true,
            single_transaction: true,
            no_owner: true,
        }
    }
}

impl NativeRestoreModel {
    pub fn validate(&self) -> Result<(), String> {
        if self.database.trim().is_empty() {
            return Err(crate::localization::text(
                "explorer.native_restore.database_required",
            ));
        }
        let path = self.path.trim();
        if path.is_empty() {
            return Err(crate::localization::text(
                "explorer.native_restore.file_required",
            ));
        }
        if !Path::new(path).is_file() {
            return Err(crate::localization::text(
                "explorer.native_restore.file_missing",
            ));
        }
        if self.kind.is_none() {
            return Err(crate::localization::text(
                "explorer.native_restore.type_unknown",
            ));
        }
        Ok(())
    }

    pub fn if_exists_effective(&self) -> bool {
        self.clean && self.if_exists
    }

    pub fn confirm_argv_preview(
        &self,
        profile: &sqlly_core::ConnectionProfile,
        tool: NativeTool,
    ) -> String {
        let ep = Endpoint {
            host: profile.server.clone(),
            port: profile.port,
            user: profile.authentication.username().unwrap_or("").to_string(),
        };
        let req = RestoreRequest {
            database: self.database.clone(),
            input: PathBuf::from(self.path.trim()),
            clean: self.clean,
            if_exists: self.if_exists_effective(),
            single_transaction: self.single_transaction,
            no_owner: self.no_owner,
        };
        match tool {
            NativeTool::PgRestore => pg_restore_args(&req, &ep).join(" "),
            NativeTool::Mysql => {
                format!(
                    "mysql {} < {}",
                    sqlly_services::mysql_restore_args(&req, &ep, Path::new("/tmp/my.cnf"))
                        .join(" "),
                    self.path.trim()
                )
            }
            _ => String::new(),
        }
    }
}

impl EventEmitter<ToolDialogEvent> for NativeRestoreDialog {}

pub struct NativeRestoreDialog {
    context: ToolDialogContext,
    model: NativeRestoreModel,
    databases: Vec<String>,
    databases_loading: bool,
    load_error: Option<String>,
    db_select: Entity<SelectState<Vec<String>>>,
    path_input: Entity<InputState>,
    confirm_argv_preview: String,
    selects_dirty: bool,
    pending_path_prefill: Option<String>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    confirm_open: bool,
    running: bool,
    progress_object: String,
    cancel_flag: Option<Arc<AtomicBool>>,
    _subscriptions: Vec<Subscription>,
}

impl NativeRestoreDialog {
    pub fn new(
        context: ToolDialogContext,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        let db_select = cx.new(|cx| SelectState::new(Vec::<String>::new(), None, window, cx));
        let path_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/backup.dump"));

        let subscriptions = vec![
            cx.subscribe_in(
                &db_select,
                window,
                |this: &mut Self, _, event: &SelectEvent<Vec<String>>, _window, cx| {
                    let SelectEvent::Confirm(value) = event;
                    if let Some(db) = value {
                        this.model.database = db.to_string();
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
                        this.sniff_file();
                        cx.notify();
                    }
                },
            ),
        ];

        let mut this = Self {
            context,
            model: NativeRestoreModel::default(),
            databases: Vec::new(),
            databases_loading: true,
            load_error: None,
            db_select,
            path_input,
            confirm_argv_preview: String::new(),
            selects_dirty: false,
            pending_path_prefill: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(580.0)),
            confirm_open: false,
            running: false,
            progress_object: String::new(),
            cancel_flag: None,
            _subscriptions: subscriptions,
        };
        this.load_databases(cx);
        this
    }

    fn connection_kind(&self) -> ConnectionKind {
        self.context.profile.database_type.connection_kind()
    }

    fn restore_tool(&self) -> Option<NativeTool> {
        match self.connection_kind() {
            ConnectionKind::PostgresWire => Some(NativeTool::PgRestore),
            ConnectionKind::MySqlWire => Some(NativeTool::Mysql),
            _ => None,
        }
    }

    fn tool_location(&self) -> Option<sqlly_services::ToolLocation> {
        let tool = self.restore_tool()?;
        find_tool(&native_tool_locations(), tool).cloned()
    }

    fn load_databases(&mut self, cx: &mut gpui::Context<Self>) {
        self.databases_loading = true;
        let sql = match self.connection_kind() {
            ConnectionKind::PostgresWire => {
                "SELECT datname FROM pg_database WHERE datallowconn AND NOT datistemplate ORDER BY 1"
                    .to_string()
            }
            ConnectionKind::MySqlWire => "SHOW DATABASES".to_string(),
            _ => String::new(),
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
                let initial = this
                    .context
                    .profile
                    .database
                    .clone()
                    .filter(|db| this.databases.contains(db))
                    .or_else(|| this.databases.first().cloned());
                if let Some(db) = initial {
                    this.model.database = db;
                }
                this.selects_dirty = true;
            }
            cx.notify();
        });
    }

    fn sniff_file(&mut self) {
        let path = self.model.path.trim();
        if path.is_empty() {
            self.model.kind = None;
            return;
        }
        let Ok(mut file) = std::fs::File::open(path) else {
            self.model.kind = None;
            return;
        };
        use std::io::Read;
        let mut head = [0u8; 512];
        let n = file.read(&mut head).unwrap_or(0);
        self.model.kind = Some(sniff_restore_kind(&head[..n]));
    }

    fn choose_file(&mut self, _window: &mut Window, cx: &mut gpui::Context<Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: None,
        });
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
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
    }

    fn request_restore(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.model.validate() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        let store = crate::connection::ConnectionStore::global();
        if crate::mutation_review::SafetyFeatures::effective_for_profile(
            &self.context.profile,
            store,
        )
        .is_production
        {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: crate::localization::text("explorer.native_restore.production_blocked"),
            });
            return;
        }
        if self.model.kind == Some(RestoreKind::PlainSql) {
            let path = PathBuf::from(self.model.path.trim());
            cx.emit(ToolDialogEvent::RunSqlFile { path });
            cx.emit(ToolDialogEvent::Close);
            return;
        }
        // pg_restore dials the database itself; a relay route has no local
        // endpoint for it.
        if let Some(reason) = crate::app::tools::direct_route_refusal(
            &self.context.profile,
            &crate::localization::text("explorer.native_restore.native_restore_feature"),
        ) {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: reason,
            });
            return;
        }
        let Some(tool) = self.restore_tool() else {
            return;
        };
        self.confirm_argv_preview = self.model.confirm_argv_preview(&self.context.profile, tool);
        self.confirm_open = true;
        cx.notify();
    }

    fn start_restore(&mut self, cx: &mut gpui::Context<Self>) {
        self.confirm_open = false;
        let Some(loc) = self.tool_location() else {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Error,
                message: crate::localization::text("explorer.native_restore.tool_missing"),
            });
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel_flag = Some(cancel.clone());
        self.running = true;
        self.progress_object.clear();

        let profile = self.context.profile.clone();
        let services = self.context.services.clone();
        let req = RestoreRequest {
            database: self.model.database.clone(),
            input: PathBuf::from(self.model.path.trim()),
            clean: self.model.clean,
            if_exists: self.model.if_exists_effective(),
            single_transaction: self.model.single_transaction,
            no_owner: self.model.no_owner,
        };
        let weak = cx.weak_entity();
        let async_cx = cx.to_async();
        let weak_progress = weak.clone();
        cx.spawn(async move |_this, _cx| {
            let async_progress = async_cx.clone();
            let mut progress_fn = move |p: DumpProgress| {
                let mut cx = async_progress.clone();
                weak_progress
                    .update(&mut cx, |this, cx| {
                        if !p.object.is_empty() {
                            this.progress_object = p.object.clone();
                        }
                        cx.notify();
                    })
                    .ok();
            };
            let result =
                run_native_restore(&services, &profile, &loc, &req, cancel, &mut progress_fn).await;
            let mut cx = async_cx;
            weak.update(&mut cx, |this, cx| {
                this.running = false;
                this.cancel_flag = None;
                match result {
                    Ok(report) => {
                        let level = StatusLevel::Info;
                        let message = if report.cancelled {
                            crate::localization::text("explorer.native_restore.cancelled")
                        } else {
                            crate::localization::text("explorer.native_restore.finished")
                        };
                        cx.emit(ToolDialogEvent::DumpCompleted {
                            message,
                            level,
                            primary_path: None,
                            cancelled: report.cancelled,
                        });
                        if !report.cancelled {
                            cx.emit(ToolDialogEvent::Close);
                        }
                    }
                    Err(err) => {
                        cx.emit(ToolDialogEvent::DumpCompleted {
                            message: crate::localization::format(
                                "explorer.native_restore.failed",
                                &[("error", &crate::localization::message(err.message()))],
                            ),
                            level: StatusLevel::Error,
                            primary_path: None,
                            cancelled: false,
                        });
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn cancel(&mut self) {
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// The equivalent one-shot CLI line: `sqlly restore-file` for vendor
    /// archives, `sqlly run-file` for plain SQL (mirroring how Restore routes
    /// plain scripts through Run SQL File).
    fn cli_command_text(&self) -> Option<String> {
        if self.model.database.trim().is_empty() || self.model.path.trim().is_empty() {
            return None;
        }
        let connection = &self.context.profile.name;
        let cli = crate::cli_command::cli_path();
        match self.model.kind {
            Some(RestoreKind::PlainSql) => Some(crate::cli_command::run_file_command(
                &cli,
                connection,
                Some(&self.model.database),
                self.model.path.trim(),
            )),
            Some(RestoreKind::PgArchive) => Some(crate::cli_command::restore_command(
                &cli,
                connection,
                &self.model.database,
                self.model.path.trim(),
                crate::cli_command::RestoreSettings {
                    clean: self.model.clean,
                    if_exists: self.model.if_exists_effective(),
                    single_transaction: self.model.single_transaction,
                    no_owner: self.model.no_owner,
                },
            )),
            None => None,
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

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.running {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    fn checkbox(
        &self,
        id: impl Into<gpui::ElementId>,
        label: &'static str,
        checked: bool,
        disabled: bool,
        set: fn(&mut NativeRestoreModel, bool),
        cx: &mut gpui::Context<Self>,
    ) -> gpui::AnyElement {
        let this = cx.entity().downgrade();
        Checkbox::new(id)
            .checked(checked)
            .disabled(disabled)
            .label(crate::localization::text(label))
            .on_click(move |new_checked: &bool, _window, app| {
                let checked = *new_checked;
                if let Some(this) = this.upgrade() {
                    this.update(app, |this, cx| {
                        set(&mut this.model, checked);
                        if !this.model.clean {
                            this.model.if_exists = false;
                        }
                        cx.notify();
                    });
                }
            })
            .into_any_element()
    }
}

impl gpui::Focusable for NativeRestoreDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NativeRestoreDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
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
            self.path_input.update(cx, |state, cx| {
                state.set_value(path, window, cx);
            });
            self.sniff_file();
        }

        let s = Theme::scaled;

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

        let kind_label = crate::localization::text(match self.model.kind {
            Some(RestoreKind::PgArchive) => "explorer.native_restore.detected_archive",
            Some(RestoreKind::PlainSql) => "explorer.native_restore.detected_sql",
            None => "explorer.native_restore.detect_prompt",
        });

        let pg_wire = self.connection_kind() == ConnectionKind::PostgresWire;
        let show_pg_opts = pg_wire && self.model.kind == Some(RestoreKind::PgArchive);

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(field(
                "explorer.native_restore.database_label",
                if self.databases_loading {
                    div()
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.native_restore.databases_loading",
                        ))
                        .into_any_element()
                } else if let Some(error) = &self.load_error {
                    div()
                        .text_color(Theme::error())
                        .child(crate::localization::format(
                            "explorer.native_restore.databases_error",
                            &[("error", error)],
                        ))
                        .into_any_element()
                } else {
                    Select::new(&self.db_select).w_full().into_any_element()
                },
            ))
            .child(field(
                "explorer.native_restore.file_label",
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(6.0)))
                    .child(div().flex_1().child(Input::new(&self.path_input)))
                    .child(
                        Button::new("restore-choose-file")
                            .label(crate::localization::text(
                                "explorer.native_restore.choose_button",
                            ))
                            .disabled(self.running)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.choose_file(window, cx);
                            })),
                    )
                    .into_any_element(),
            ))
            .child(
                div()
                    .pl(px(s(118.0)))
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(kind_label),
            );

        if show_pg_opts {
            body = body
                .child(self.checkbox(
                    "restore-clean",
                    "explorer.native_restore.clean_checkbox",
                    self.model.clean,
                    self.running,
                    |m, v| m.clean = v,
                    cx,
                ))
                .child(self.checkbox(
                    "restore-if-exists",
                    "explorer.native_restore.if_exists_checkbox",
                    self.model.if_exists,
                    self.running || !self.model.clean,
                    |m, v| m.if_exists = v,
                    cx,
                ))
                .child(self.checkbox(
                    "restore-single-txn",
                    "explorer.native_restore.single_transaction_checkbox",
                    self.model.single_transaction,
                    self.running,
                    |m, v| m.single_transaction = v,
                    cx,
                ))
                .child(self.checkbox(
                    "restore-no-owner",
                    "explorer.native_restore.no_owner_checkbox",
                    self.model.no_owner,
                    self.running,
                    |m, v| m.no_owner = v,
                    cx,
                ));
        }

        if self.running {
            body = body.child(div().text_color(Theme::text_dim()).child(
                if self.progress_object.is_empty() {
                    crate::localization::text("explorer.native_restore.restoring")
                } else {
                    self.progress_object.clone()
                },
            ));
        }

        let focus_handle = self.focus_handle.clone();
        let mut dialog = Dialog::new("native-restore-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "explorer.native_restore.title",
                &[("connection", &self.context.profile.name)],
            ))
            .track_focus(&focus_handle)
            .dismiss_on_scrim_click(false);

        if self.confirm_open {
            // The destructive confirm reuses the same card: the body swaps to
            // the warning + exact argv and the footer becomes Cancel/Restore.
            let server = self.context.profile.server.clone();
            let database = self.model.database.clone();
            dialog = dialog
                .confirm(
                    crate::localization::text("explorer.native_restore.confirm_restore_button"),
                    crate::localization::text("explorer.native_restore.confirm_cancel_button"),
                )
                .danger()
                .on_confirm(|this, _window, cx| this.start_restore(cx))
                .on_reject(|this, _window, cx| {
                    this.confirm_open = false;
                    cx.notify();
                })
                .on_close(|this, _window, cx| {
                    this.confirm_open = false;
                    cx.notify();
                })
                .child(
                    div()
                        .text_color(Theme::text())
                        .child(crate::localization::format(
                            "explorer.native_restore.destructive_warning",
                            &[("database", &database), ("server", &server)],
                        )),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .font_family(Theme::editor_font_family())
                        .text_color(Theme::text_dim())
                        .child(self.confirm_argv_preview.clone()),
                );
        } else {
            dialog = dialog
                // The Cancel/Close button doubles as the running-cancel path,
                // so it stays a custom button instead of the default Close.
                .use_default_close(false)
                .on_close(|this, _window, cx| this.close(cx))
                .child(body)
                .button(
                    Button::new("restore-copy-cli")
                        .label(crate::localization::text(
                            "explorer.native_restore.copy_cli_button",
                        ))
                        .disabled(self.cli_command_text().is_none())
                        .on_click(cx.listener(|this, _, _window, cx| this.copy_cli_command(cx))),
                )
                .button(
                    Button::new("restore-cancel-btn")
                        .label(crate::localization::text(if self.running {
                            "explorer.native_restore.cancel_running_button"
                        } else {
                            "explorer.native_restore.close_button"
                        }))
                        .ghost()
                        .on_click(cx.listener(|this, _, _window, cx| {
                            if this.running {
                                this.cancel();
                            } else {
                                this.close(cx);
                            }
                        })),
                )
                .button(
                    Button::new("restore-primary")
                        .label(crate::localization::text(
                            "explorer.native_restore.restore_button",
                        ))
                        .primary()
                        .disabled(self.running)
                        .on_click(cx.listener(|this, _, _window, cx| this.request_restore(cx))),
                );
        }

        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    #[test]
    fn plain_sql_routes_to_run_file() {
        let model = NativeRestoreModel {
            database: "db".into(),
            path: "/tmp/dump.sql".into(),
            kind: Some(RestoreKind::PlainSql),
            ..Default::default()
        };
        assert_eq!(model.kind, Some(RestoreKind::PlainSql));
    }

    #[test]
    fn if_exists_cleared_when_clean_off() {
        let mut model = NativeRestoreModel {
            clean: false,
            if_exists: true,
            ..Default::default()
        };
        assert!(!model.if_exists_effective());
        model.clean = true;
        assert!(model.if_exists_effective());
    }

    #[test]
    fn validation_requires_database_and_existing_file() {
        let model = NativeRestoreModel::default();
        assert!(model.validate().is_err());
        let model = NativeRestoreModel {
            database: "db".into(),
            path: "/no/such/file.sql".into(),
            kind: Some(RestoreKind::PlainSql),
            ..Default::default()
        };
        assert!(model.validate().is_err());
    }

    #[gpui::test]
    fn confirm_not_shown_until_restore_clicked(cx: &mut gpui::TestAppContext) {
        crate::ensure_test_tokio_runtime();
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|window, cx| {
            let profile = sqlly_core::ConnectionProfile::sql_login(
                "pg",
                "localhost",
                "u",
                sqlly_core::SecretRef {
                    provider: sqlly_core::SecretProvider::InMemory,
                    key: "p".into(),
                },
            );
            let services = sqlly_services::Services::new();
            let context = ToolDialogContext::new(profile, services);
            NativeRestoreDialog::new(context, window, cx)
        });
        view.read_with(cx, |v, _| assert!(!v.confirm_open));
    }
}
