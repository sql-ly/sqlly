//! In-app deploy for Schema Compare — streams the reviewed deploy script through
//! `run_script` with optional single-transaction wrapping.

use std::io::Cursor;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui::{
    EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, prelude::*,
    px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use gpui_component::checkbox::Checkbox;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType, Dialect};
use sqlly_services::{
    ChunkReader, DeployPlan, ErrorDecision, OnError, ScriptCancel, ScriptOutcome, ScriptProgress,
    ScriptRunOptions, ScriptUnitFailure, TransactionMode, run_script,
};
use tokio::sync::mpsc;

use crate::app::StatusLevel;
use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Page {
    Review,
    Progress,
    Summary,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeployModel {
    pub runnable_steps: usize,
    pub manual_steps: usize,
    pub transactional: bool,
    pub engine_transactional_ddl: bool,
    pub ai_review_required: bool,
}

impl DeployModel {
    pub fn from_plan(
        plan: &DeployPlan,
        target_type: DatabaseType,
        ai_review_required: bool,
    ) -> Self {
        let engine_transactional_ddl = matches!(
            target_type.connection_kind(),
            ConnectionKind::Tds
                | ConnectionKind::PostgresWire
                | ConnectionKind::SqliteFile
                | ConnectionKind::DuckDbFile
                | ConnectionKind::HranaHttp
        );
        Self {
            runnable_steps: plan.steps.iter().filter(|step| step.runnable).count(),
            manual_steps: plan
                .steps
                .len()
                .saturating_sub(plan.steps.iter().filter(|step| step.runnable).count()),
            transactional: engine_transactional_ddl,
            engine_transactional_ddl,
            ai_review_required,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.runnable_steps == 0 {
            return Err(crate::localization::text(
                "explorer.schema_deploy.no_runnable_steps",
            ));
        }
        if self.ai_review_required {
            return Err(crate::localization::text(
                "explorer.schema_deploy.ai_review_required",
            ));
        }
        if self.transactional && !self.engine_transactional_ddl {
            return Err(crate::localization::text(
                "explorer.schema_deploy.transaction_unsupported",
            ));
        }
        Ok(())
    }

    pub fn warning(&self) -> Option<String> {
        if !self.engine_transactional_ddl {
            return Some(crate::localization::text(
                "explorer.schema_deploy.nontransactional_warning",
            ));
        }
        if self.manual_steps > 0 {
            return Some(crate::localization::format(
                "explorer.schema_deploy.manual_steps_warning",
                &[("count", &self.manual_steps.to_string())],
            ));
        }
        None
    }

    pub fn to_options(&self, dialect: Dialect, session_key: String) -> ScriptRunOptions {
        ScriptRunOptions {
            dialect,
            on_error: OnError::Stop,
            transaction: if self.transactional {
                TransactionMode::Single
            } else {
                TransactionMode::None
            },
            coalesce_inserts: false,
            session_key,
            coalesce_max_rows: 500,
            coalesce_max_bytes: 1024 * 1024,
            definer: None,
        }
    }
}

pub struct DeployRequest {
    pub script: String,
    pub model: DeployModel,
}

impl EventEmitter<ToolDialogEvent> for SchemaCompareDeployDialog {}

pub struct SchemaCompareDeployDialog {
    context: ToolDialogContext,
    page: Page,
    model: DeployModel,
    script: String,
    running: bool,
    progress: Option<ScriptProgress>,
    failures: Vec<ScriptUnitFailure>,
    last_summary: Option<String>,
    cancel: Option<ScriptCancel>,
    cancel_flag: Option<Arc<AtomicBool>>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _tasks: Vec<gpui::Task<()>>,
}

impl SchemaCompareDeployDialog {
    pub fn new(
        context: ToolDialogContext,
        request: DeployRequest,
        _window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self {
            context,
            page: Page::Review,
            model: request.model,
            script: request.script,
            running: false,
            progress: None,
            failures: Vec::new(),
            last_summary: None,
            cancel: None,
            cancel_flag: None,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(720.0)),
            _tasks: Vec::new(),
        }
    }

    pub fn take_last_summary(&mut self) -> Option<String> {
        self.last_summary.take()
    }

    fn start_run(&mut self, cx: &mut gpui::Context<Self>) {
        if let Err(reason) = self.model.validate() {
            cx.emit(ToolDialogEvent::Status {
                level: StatusLevel::Info,
                message: reason,
            });
            return;
        }
        self.page = Page::Progress;
        self.running = true;
        self.failures.clear();
        self.progress = None;

        let cancel = ScriptCancel::new();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.cancel_flag = Some(cancel_flag);

        let profile = self.context.profile.clone();
        let services = self.context.services.clone();
        let script = self.script.clone();
        let total_bytes = script.len() as u64;
        let dialect = profile.database_type.family();
        let options = self
            .model
            .to_options(dialect, format!("schema-deploy:{}", uuid::Uuid::new_v4()));
        let (progress_tx, mut progress_rx) = mpsc::channel::<ScriptProgress>(32);

        let weak_progress = cx.weak_entity();
        let weak_run = cx.weak_entity();
        let async_cx = cx.to_async();
        let async_cx_progress = async_cx.clone();

        let progress_task = cx.spawn(async move |_this, _cx| {
            while let Some(progress) = progress_rx.recv().await {
                let mut cx = async_cx_progress.clone();
                weak_progress
                    .update(&mut cx, |this, cx| {
                        this.progress = Some(progress);
                        cx.notify();
                    })
                    .ok();
            }
        });

        let async_cx_run = async_cx.clone();
        cx.spawn(async move |_this, _cx| {
            let reader = ChunkReader::new(Cursor::new(script.into_bytes()));
            let outcome = run_script(
                services.queries.as_ref(),
                &profile,
                reader,
                total_bytes,
                &options,
                progress_tx,
                cancel,
                |_| Box::pin(async { ErrorDecision::Stop }),
            )
            .await;

            let mut cx = async_cx_run.clone();
            weak_run
                .update(&mut cx, |this, cx| {
                    this.running = false;
                    this.cancel = None;
                    this.cancel_flag = None;
                    match outcome {
                        Ok(outcome) => this.finish_run(outcome, cx),
                        Err(err) => {
                            cx.emit(ToolDialogEvent::Status {
                                level: StatusLevel::Error,
                                message: crate::localization::format(
                                    "explorer.schema_deploy.failed",
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

        self._tasks = vec![progress_task];
        cx.notify();
    }

    fn finish_run(&mut self, outcome: ScriptOutcome, cx: &mut gpui::Context<Self>) {
        self.failures = outcome.failures.clone();
        let summary = crate::localization::format(
            "explorer.schema_deploy.summary",
            &[
                ("done", &outcome.units_done.to_string()),
                ("failed", &outcome.units_failed.to_string()),
                ("skipped", &outcome.units_skipped.to_string()),
                ("rows", &outcome.rows_affected.to_string()),
            ],
        );
        self.last_summary = Some(summary.clone());
        self.page = Page::Summary;
        cx.emit(ToolDialogEvent::Status {
            level: if outcome.units_failed > 0 {
                StatusLevel::Error
            } else {
                StatusLevel::Info
            },
            message: crate::localization::text(if outcome.units_failed > 0 {
                "explorer.schema_deploy.finished_failures"
            } else {
                "explorer.schema_deploy.completed"
            }),
        });
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
        if let Some(flag) = &self.cancel_flag {
            flag.store(true, Ordering::SeqCst);
        }
        cx.emit(ToolDialogEvent::Close);
    }
}

impl gpui::Focusable for SchemaCompareDeployDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SchemaCompareDeployDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let warning = self.model.warning();
        let body = match self.page {
            Page::Review => div()
                .flex()
                .flex_col()
                .gap(px(s(8.0)))
                .child(crate::localization::format(
                    "explorer.schema_deploy.target",
                    &[
                        ("name", &self.context.profile.name),
                        (
                            "engine",
                            &self
                                .context
                                .profile
                                .database_type
                                .display_name()
                                .to_string(),
                        ),
                    ],
                ))
                .child(crate::localization::format(
                    "explorer.schema_deploy.step_counts",
                    &[
                        ("runnable", &self.model.runnable_steps.to_string()),
                        ("manual", &self.model.manual_steps.to_string()),
                    ],
                ))
                .child(
                    Checkbox::new("sc-deploy-txn")
                        .checked(self.model.transactional)
                        .disabled(!self.model.engine_transactional_ddl)
                        .label(crate::localization::text(
                            "explorer.schema_deploy.single_transaction",
                        ))
                        .on_click(cx.listener(|this, checked: &bool, _window, cx| {
                            this.model.transactional = *checked;
                            cx.notify();
                        })),
                )
                .when_some(warning, |el, text| {
                    el.child(div().text_color(Theme::text_dim()).child(text))
                })
                .child(
                    div()
                        .max_h(px(s(240.0)))
                        .overflow_hidden()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .child(self.script.clone()),
                )
                .into_any_element(),
            Page::Progress => {
                let progress = self
                    .progress
                    .as_ref()
                    .map(|p| {
                        let message = p.last_message.clone().unwrap_or_else(|| {
                            crate::localization::text("explorer.schema_deploy.running")
                        });
                        crate::localization::format(
                            "explorer.schema_deploy.progress",
                            &[
                                ("done", &p.units_done.to_string()),
                                ("total", &self.model.runnable_steps.to_string()),
                                ("message", &message),
                            ],
                        )
                    })
                    .unwrap_or_else(|| {
                        crate::localization::text("explorer.schema_deploy.starting")
                    });
                div().child(progress).into_any_element()
            }
            Page::Summary => div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .children(self.failures.iter().map(|failure| {
                    crate::localization::format(
                        "explorer.schema_deploy.failure_line",
                        &[
                            ("line", &failure.start_line.to_string()),
                            ("message", &failure.message),
                            ("decision", &format!("{:?}", failure.decision)),
                        ],
                    )
                }))
                .into_any_element(),
        };

        let focus_handle = self.focus_handle.clone();
        let dialog = Dialog::new("schema-compare-deploy", |this: &mut Self| &mut this.frame)
            .title(crate::localization::text("explorer.schema_deploy.title"))
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body);
        let dialog = match self.page {
            // Destructive confirm: the user must choose Deploy or Cancel, so
            // confirm mode (which blocks scrim dismiss) is the right shape.
            Page::Review => dialog
                .confirm(
                    crate::localization::text("explorer.schema_deploy.confirm_button"),
                    crate::localization::text("explorer.schema_deploy.review_cancel_button"),
                )
                .danger()
                // Mirrors the old button's `.disabled(self.running)` gate.
                .on_confirm(|this, _window, cx| {
                    if !this.running {
                        this.start_run(cx);
                    }
                })
                .on_reject(|this, _window, cx| this.close(cx)),
            // While the deploy runs, the only action is Cancel, which also
            // cancels the in-flight script (same path as the ✕).
            Page::Progress => dialog.use_default_close(false).button(
                Button::new("sc-deploy-cancel")
                    .label(crate::localization::text(
                        "explorer.schema_deploy.progress_cancel_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.close(cx))),
            ),
            Page::Summary => dialog.dismiss_on_scrim_click(false),
        };
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(runnable: usize, manual: usize, transactional: bool, engine: bool) -> DeployModel {
        DeployModel {
            runnable_steps: runnable,
            manual_steps: manual,
            transactional,
            engine_transactional_ddl: engine,
            ai_review_required: false,
        }
    }

    #[test]
    fn validate_rejects_zero_runnable_steps() {
        assert!(model(0, 0, true, true).validate().is_err());
    }

    #[test]
    fn validate_rejects_transaction_on_mysql() {
        let mut m = model(1, 0, true, false);
        m.engine_transactional_ddl = false;
        assert!(m.validate().is_err());
    }

    #[test]
    fn validate_rejects_when_ai_review_required() {
        let mut m = model(1, 0, true, true);
        m.ai_review_required = true;
        assert!(m.validate().is_err());
    }

    #[test]
    fn to_options_maps_transaction_flag() {
        let on = model(1, 0, true, true).to_options(Dialect::SqlServer, "k".into());
        assert_eq!(on.transaction, TransactionMode::Single);
        let off = model(1, 0, false, true).to_options(Dialect::SqlServer, "k".into());
        assert_eq!(off.transaction, TransactionMode::None);
    }

    #[test]
    fn warning_mentions_manual_steps() {
        let warning = model(1, 2, true, true).warning().unwrap();
        assert!(warning.contains("manual review"));
    }
}
