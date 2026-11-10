//! Run-now confirmation for an explorer Maintenance action.
//!
//! The Script form of every Maintenance item opens a tab directly; this
//! dialog is the Run form's one extra step: it names the object and the exact
//! statement(s), surfaces the lock / long-running warning for the operations
//! that deserve one, and only then hands the SQL to the workbench, which
//! opens it in a tab bound to the node's connection and executes it through
//! the normal query path (so progress, messages, and errors land in the usual
//! results pane, and every execute gate still applies). A connection whose
//! policy allows SELECT only, or a locked production connection, cannot Run
//! from here — the button says why and Script to Editor stays available.

use gpui::{
    EventEmitter, FocusHandle, Focusable, IntoElement, ParentElement, Render, Styled, Window, div,
    px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use sqlly_services::maintenance::MaintenanceTarget;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent};
use crate::tooltip::AppTooltip;

/// Why Run is refused up front (the execution path would refuse it too, but
/// a greyed button with the reason beats a red error after a click).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunBlock {
    /// The connection (or an entity it inherits from) allows SELECT only.
    SelectOnly,
    /// The connection is a locked production connection.
    ProductionReadOnly,
}

impl RunBlock {
    pub fn message(&self) -> String {
        crate::localization::text(match self {
            Self::SelectOnly => "explorer.maintenance_dialog.select_only_message",
            Self::ProductionReadOnly => "explorer.maintenance_dialog.production_read_only_message",
        })
    }
}

/// Everything the dialog shows, resolved by the app before opening it.
#[derive(Clone, Debug)]
pub struct MaintenanceRequest {
    /// The statement as the menu named it: `VACUUM (FULL)`, `DBCC CHECKDB`.
    pub label: String,
    pub target: MaintenanceTarget,
    /// The exact SQL that will run (no header comment; the tab gets one).
    pub sql: String,
    /// Lock / long-running warning for operations that carry one.
    pub warning: Option<&'static str>,
    /// The tab's contents: header comment plus `sql`.
    pub script: String,
    pub run_block: Option<RunBlock>,
}

pub struct MaintenanceDialog {
    context: ToolDialogContext,
    request: MaintenanceRequest,
    focus_handle: FocusHandle,
    frame: DialogFrame,
}

impl EventEmitter<ToolDialogEvent> for MaintenanceDialog {}

impl MaintenanceDialog {
    pub fn new(
        context: ToolDialogContext,
        request: MaintenanceRequest,
        cx: &mut gpui::Context<Self>,
    ) -> Self {
        Self {
            context,
            request,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
        }
    }

    /// The title line: `Run VACUUM (FULL) — public.orders`.
    pub fn title(&self) -> String {
        crate::localization::format(
            "explorer.maintenance_dialog.title",
            &[
                ("operation", &self.request.label),
                ("target", &self.request.target.display_name()),
            ],
        )
    }

    pub fn run_allowed(&self) -> bool {
        self.request.run_block.is_none()
    }

    fn run(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.run_allowed() {
            return;
        }
        let profile = Box::new(self.context.profile.clone());
        cx.emit(ToolDialogEvent::ScriptToEditorForProfileAndRun {
            sql: self.request.script.clone(),
            profile,
        });
        cx.emit(ToolDialogEvent::Close);
    }

    fn script_to_editor(&mut self, cx: &mut gpui::Context<Self>) {
        let profile = Box::new(self.context.profile.clone());
        cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
            sql: self.request.script.clone(),
            profile,
        });
        cx.emit(ToolDialogEvent::Close);
    }

    fn close(&mut self, cx: &mut gpui::Context<Self>) {
        cx.emit(ToolDialogEvent::Close);
    }
}

impl Focusable for MaintenanceDialog {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MaintenanceDialog {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let field = |label: String, control: gpui::AnyElement| {
            div()
                .flex()
                .flex_row()
                .items_start()
                .gap(px(s(8.0)))
                .child(
                    div()
                        .w(px(s(90.0)))
                        .flex_shrink_0()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(label),
                )
                .child(div().flex_1().min_w(px(0.0)).child(control))
        };
        let scope_label = crate::localization::text(match self.request.target {
            MaintenanceTarget::Database { .. } => "explorer.maintenance_dialog.database_label",
            MaintenanceTarget::Table { .. } => "explorer.maintenance_dialog.table_label",
            MaintenanceTarget::Index { .. } => "explorer.maintenance_dialog.index_label",
        });

        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(field(
                crate::localization::text("explorer.maintenance_dialog.connection_label"),
                div()
                    .text_size(px(Theme::font_size_ratio(0.95)))
                    .text_color(Theme::text())
                    .child(self.context.profile.name.clone())
                    .into_any_element(),
            ))
            .child(field(
                scope_label,
                div()
                    .text_size(px(Theme::font_size_ratio(0.95)))
                    .text_color(Theme::text())
                    .child(self.request.target.display_name())
                    .into_any_element(),
            ));

        if let Some(warning) = self.request.warning {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::warning())
                    .whitespace_normal()
                    .child(crate::localization::format(
                        "explorer.maintenance_dialog.warning",
                        &[("warning", warning)],
                    )),
            );
        }
        if let Some(block) = &self.request.run_block {
            body = body.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.87)))
                    .text_color(Theme::error())
                    .whitespace_normal()
                    .child(block.message()),
            );
        }

        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap(px(s(4.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "explorer.maintenance_dialog.statement_label",
                        )),
                )
                .child(
                    div()
                        .p(px(s(8.0)))
                        .rounded(px(s(4.0)))
                        .bg(Theme::sidebar_bg())
                        .font_family(Theme::editor_font_family())
                        .border_1()
                        .border_color(Theme::border())
                        .text_size(px(Theme::font_size_ratio(0.87)))
                        .text_color(Theme::text())
                        .max_h(px(s(220.0)))
                        .overflow_hidden()
                        .whitespace_normal()
                        .child(self.request.sql.clone()),
                ),
        );
        body = body.child(
            div()
                .text_size(px(Theme::font_size_ratio(0.80)))
                .text_color(Theme::text_dim())
                .whitespace_normal()
                .child(crate::localization::text(
                    "explorer.maintenance_dialog.run_explanation",
                )),
        );

        let run_allowed = self.run_allowed();
        let run_tip = self
            .request
            .run_block
            .as_ref()
            .map(RunBlock::message)
            .unwrap_or_default();
        let title = self.title();
        let focus_handle = self.focus_handle.clone();
        Dialog::new("maintenance-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .easy_close(true)
            .easy_close_when_dirty(true)
            .key_context("MaintenanceDialog")
            .track_focus(&focus_handle)
            .on_close(|this, _window, cx| this.close(cx))
            .child(body)
            .button(
                Button::new("maintenance-dialog-script")
                    .label(crate::localization::text(
                        "explorer.maintenance_dialog.script_button",
                    ))
                    .on_click(cx.listener(|this, _, _window, cx| this.script_to_editor(cx))),
            )
            .button({
                let mut run = Button::new("maintenance-dialog-run")
                    .primary()
                    .label(crate::localization::text(
                        "explorer.maintenance_dialog.run_button",
                    ))
                    .disabled(!run_allowed);
                if !run_tip.is_empty() {
                    run = run.app_tooltip_text(run_tip);
                }
                run.on_click(cx.listener(|this, _, _window, cx| this.run(cx)))
            })
            .build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_block_messages_name_the_policy() {
        assert!(
            RunBlock::SelectOnly
                .message()
                .contains("SELECT statements only")
        );
        assert!(RunBlock::ProductionReadOnly.message().contains("read-only"));
    }
}
