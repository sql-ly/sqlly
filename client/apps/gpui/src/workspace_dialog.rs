use gpui::{
    Context, Entity, EventEmitter, IntoElement, Render, Styled, Subscription, Window, div,
    prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::scroll::ScrollableElement;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::workspace_session::{MAX_NAMED_WORKSPACES, NamedWorkspaceIndex};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceDialogMode {
    SaveAs,
    Manage,
}

pub enum WorkspaceDialogEvent {
    Save(String),
    Open(String),
    Rename { id: String, name: String },
    Duplicate { id: String, name: String },
    Delete(String),
    Close,
}

pub struct WorkspaceDialog {
    mode: WorkspaceDialogMode,
    index: NamedWorkspaceIndex,
    selected_id: Option<String>,
    name: String,
    name_input: Entity<InputState>,
    error: Option<String>,
    delete_armed: bool,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<WorkspaceDialogEvent> for WorkspaceDialog {}

impl WorkspaceDialog {
    pub fn new(
        mode: WorkspaceDialogMode,
        index: NamedWorkspaceIndex,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let name_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                if mode == WorkspaceDialogMode::SaveAs {
                    "workspace.dialog.name_placeholder"
                } else {
                    "workspace.dialog.selection_placeholder"
                },
            ))
        });
        let subscription = cx.subscribe_in(
            &name_input,
            window,
            |this: &mut Self, input, event: &InputEvent, _window, cx| match event {
                InputEvent::Change => {
                    this.name = input.read(cx).value().to_string();
                    this.error = None;
                    this.delete_armed = false;
                    cx.notify();
                }
                InputEvent::PressEnter { .. } if this.mode == WorkspaceDialogMode::SaveAs => {
                    this.emit_save(cx);
                }
                _ => {}
            },
        );
        Self {
            mode,
            index,
            selected_id: None,
            name: String::new(),
            name_input,
            error: None,
            delete_armed: false,
            frame: DialogFrame::new_auto(Theme::scaled(620.0)),
            _subscriptions: vec![subscription],
        }
    }

    pub fn focus_name(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.name_input
            .update(cx, |state, cx| state.focus(window, cx));
    }

    pub fn set_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        self.error = Some(error.into());
        self.delete_armed = false;
        cx.notify();
    }

    pub fn update_index(&mut self, index: NamedWorkspaceIndex, cx: &mut Context<Self>) {
        self.index = index;
        if self
            .selected_id
            .as_ref()
            .is_some_and(|id| !self.index.workspaces.iter().any(|item| &item.id == id))
        {
            self.selected_id = None;
            self.name.clear();
        }
        self.delete_armed = false;
        cx.notify();
    }

    fn emit_save(&mut self, cx: &mut Context<Self>) {
        if self.name.trim().is_empty() {
            self.error = Some(crate::localization::text("workspace.dialog.name_required"));
            cx.notify();
            return;
        }
        cx.emit(WorkspaceDialogEvent::Save(self.name.clone()));
    }

    fn select(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.index.workspaces.iter().find(|item| item.id == id) else {
            return;
        };
        self.selected_id = Some(id);
        self.name = item.name.clone();
        self.error = None;
        self.delete_armed = false;
        let name = self.name.clone();
        self.name_input
            .update(cx, |state, cx| state.set_value(&name, window, cx));
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(WorkspaceDialogEvent::Close);
    }
}

impl Render for WorkspaceDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let selected = self.selected_id.clone();
        let has_selection = selected.is_some();
        let rows = self
            .index
            .workspaces
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = item.id.clone();
                let is_selected = selected.as_deref() == Some(item.id.as_str());
                let is_active = self.index.active_workspace_id.as_deref() == Some(item.id.as_str());
                div()
                    .id(("workspace-row", index))
                    .flex()
                    .items_center()
                    .gap(px(s(8.0)))
                    .px(px(s(10.0)))
                    .py(px(s(7.0)))
                    .rounded(px(s(4.0)))
                    .bg(if is_selected {
                        Theme::selection()
                    } else {
                        Theme::panel_bg()
                    })
                    .hover(|row| row.bg(Theme::row_hover()))
                    .cursor_pointer()
                    .child(div().flex_1().min_w_0().child(item.name.clone()))
                    .children(is_active.then(|| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.72)))
                            .text_color(Theme::accent())
                            .child(crate::localization::text("workspace.dialog.active_badge"))
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select(id.clone(), window, cx);
                    }))
            });

        let title = match self.mode {
            WorkspaceDialogMode::SaveAs => crate::localization::text("workspace.dialog.save_title"),
            WorkspaceDialogMode::Manage => {
                crate::localization::text("workspace.dialog.manage_title")
            }
        };
        let description = match self.mode {
            WorkspaceDialogMode::SaveAs => {
                crate::localization::text("workspace.dialog.save_description")
            }
            WorkspaceDialogMode::Manage => {
                crate::localization::text("workspace.dialog.manage_description")
            }
        };

        let list = div()
            .flex()
            .flex_col()
            .gap(px(s(3.0)))
            .max_h(px(s(260.0)))
            .overflow_y_scrollbar()
            .children(rows);

        let mut buttons: Vec<gpui::AnyElement> = Vec::new();
        match self.mode {
            WorkspaceDialogMode::SaveAs => {
                buttons.push(
                    Button::new("workspace-save")
                        .label(crate::localization::text("workspace.dialog.save_button"))
                        .primary()
                        .disabled(self.name.trim().is_empty())
                        .on_click(cx.listener(|this, _, _window, cx| this.emit_save(cx)))
                        .into_any_element(),
                );
            }
            WorkspaceDialogMode::Manage => {
                let open_id = selected.clone();
                let rename_id = selected.clone();
                let duplicate_id = selected.clone();
                let delete_id = selected;
                buttons.push(
                    Button::new("workspace-rename")
                        .label(crate::localization::text("workspace.dialog.rename_button"))
                        .disabled(!has_selection || self.name.trim().is_empty())
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(id) = rename_id.clone() {
                                cx.emit(WorkspaceDialogEvent::Rename {
                                    id,
                                    name: this.name.clone(),
                                });
                            }
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("workspace-duplicate")
                        .label(crate::localization::text(
                            "workspace.dialog.duplicate_button",
                        ))
                        .disabled(!has_selection || self.name.trim().is_empty())
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(id) = duplicate_id.clone() {
                                let name = crate::localization::format(
                                    "workspace.dialog.duplicate_name",
                                    &[("workspace_name", this.name.trim())],
                                );
                                cx.emit(WorkspaceDialogEvent::Duplicate { id, name });
                            }
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("workspace-delete")
                        .label(if self.delete_armed {
                            crate::localization::text("workspace.dialog.confirm_delete_button")
                        } else {
                            crate::localization::text("workspace.dialog.delete_button")
                        })
                        .disabled(!has_selection)
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if !this.delete_armed {
                                this.delete_armed = true;
                                cx.notify();
                            } else if let Some(id) = delete_id.clone() {
                                cx.emit(WorkspaceDialogEvent::Delete(id));
                            }
                        }))
                        .into_any_element(),
                );
                buttons.push(
                    Button::new("workspace-open")
                        .label(crate::localization::text("workspace.dialog.open_button"))
                        .primary()
                        .disabled(!has_selection)
                        .on_click(cx.listener(move |_this, _, _window, cx| {
                            if let Some(id) = open_id.clone() {
                                cx.emit(WorkspaceDialogEvent::Open(id));
                            }
                        }))
                        .into_any_element(),
                );
            }
        }

        let mut dialog = Dialog::new("workspace-dialog", |this: &mut Self| &mut this.frame)
            .title(title)
            .on_close(|this, _window, cx| this.close(cx))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .child(description),
            )
            .child(Input::new(&self.name_input))
            .children_body((self.mode == WorkspaceDialogMode::Manage).then_some(list))
            .children_body(self.error.as_ref().map(|error| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::error())
                    .child(error.clone())
            }))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.72)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::format(
                        "workspace.dialog.saved_count",
                        &[
                            ("count", &self.index.workspaces.len().to_string()),
                            ("maximum", &MAX_NAMED_WORKSPACES.to_string()),
                        ],
                    )),
            );
        for button in buttons {
            dialog = dialog.button(button);
        }
        dialog.build(self, window, cx)
    }
}
