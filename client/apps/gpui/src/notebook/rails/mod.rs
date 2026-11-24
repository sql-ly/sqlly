//! The notebook shell's chrome around the cell list: top bar, banners and
//! prompts, the trust dialog, cell headers, left rail, inspector and activity
//! panel.

mod activity;
mod chrome;
mod inspector;
mod left;
mod trust;

use gpui::{AppContext as _, Context, Entity, SharedString, Window};
use gpui_component::Icon;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{InputEvent, InputState};

use super::view::NotebookView;
use crate::icon_catalog::AppIcon;

/// A compact icon-only button with its accessible name and tooltip.
pub(in crate::notebook) fn icon_button(
    id: impl Into<SharedString>,
    icon: AppIcon,
    label: String,
) -> Button {
    #[cfg(test)]
    super::perf_counters::bump(super::perf_counters::Pass::Button);
    Button::new(gpui::ElementId::Name(id.into()))
        .ghost()
        .xsmall()
        .icon(Icon::empty().path(icon.path()))
        .accessibility_label(label.clone())
        .tooltip(label)
}

/// A compact text button with its accessible name.
pub(in crate::notebook) fn text_button(id: impl Into<SharedString>, label: String) -> Button {
    #[cfg(test)]
    super::perf_counters::bump(super::perf_counters::Pass::Button);
    Button::new(gpui::ElementId::Name(id.into()))
        .ghost()
        .xsmall()
        .label(label.clone())
        .accessibility_label(label)
}

/// What a [`text_button`] looks like, without the button: cell headers
/// that are neither active nor under the pointer draw their actions this
/// way (the real button is there once the pointer or keyboard arrives).
pub(in crate::notebook) fn quiet_text(label: String, cx: &gpui::App) -> gpui::Div {
    use gpui::{ParentElement as _, Styled as _};
    use gpui_component::ActiveTheme as _;
    gpui::div()
        .flex()
        .flex_shrink_0()
        .items_center()
        .h_5()
        .px_1()
        .text_xs()
        .whitespace_nowrap()
        .text_color(cx.theme().secondary_foreground)
        .child(label)
}

/// The room a cell's drag handle takes, for headers that don't draw it.
pub(in crate::notebook) fn drag_handle_space() -> gpui::AnyElement {
    use gpui::{IntoElement as _, Styled as _};
    gpui::div()
        .flex_none()
        .w(gpui::px(crate::theme::Theme::scaled(16.0)))
        .into_any_element()
}

fn new_input(
    window: &mut Window,
    cx: &mut Context<NotebookView>,
    placeholder: &'static str,
    value: &str,
) -> Entity<InputState> {
    let value = value.to_string();
    let state = cx.new(|cx| {
        InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
            window,
            cx,
            placeholder,
        ))
    });
    if !value.is_empty() {
        state.update(cx, |s, cx| s.set_value(value, window, cx));
    }
    state
}

impl NotebookView {
    /// Create the stateful inputs the chrome renders (they need a window).
    pub(super) fn ensure_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.title_input.is_none() {
            let title = self.doc.title().unwrap_or_default().to_string();
            let input = new_input(
                window,
                cx,
                "notebook.shell.top_bar.title_placeholder",
                &title,
            );
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = input.read(cx).value().to_string();
                    this.set_title(&value, cx);
                }
            })
            .detach();
            self.title_input = Some(input);
        }
        if self.search_input.is_none() {
            let input = new_input(window, cx, "notebook.shell.rail.search_placeholder", "");
            cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    this.search_query = input.read(cx).value().to_string();
                    cx.notify();
                }
            })
            .detach();
            self.search_input = Some(input);
        }
        let active = self.active.clone();
        if let Some(id) = active
            && self.name_input.as_ref().is_none_or(|(cell, _)| cell != &id)
        {
            let name = self
                .doc
                .cell_by_id(&id)
                .and_then(|c| c.name)
                .unwrap_or_default()
                .to_string();
            let input = new_input(
                window,
                cx,
                "notebook.shell.inspector.name_placeholder",
                &name,
            );
            let cell = id.clone();
            cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    let value = input.read(cx).value().to_string();
                    this.rename_cell(&cell, &value, cx);
                }
            })
            .detach();
            self.name_input = Some((id, input));
        }
        self.ensure_param_widgets(window, cx);
    }
}
