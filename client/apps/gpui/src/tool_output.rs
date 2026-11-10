//! Tool output modal — reusable overlay that displays CLI-generated output
//! with Copy and Open-in-Query-Tab actions. Mirrors the Swift
//! `presentToolOutput(title, body)` pattern used by Template Explorer,
//! Import/Export, Query Store, Extended Events, and Schema Compare.
//! Rendered on the unified [`crate::dialog::Dialog`] chrome.

use gpui::{
    Context, EventEmitter, IntoElement, Render, ScrollHandle, Styled, Window, div, prelude::*, px,
};
use gpui_component::button::{Button, ButtonVariants as _};

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;

/// Events emitted by the tool output modal.
#[derive(Debug, Clone)]
pub enum ToolOutputEvent {
    /// Open the given SQL text in a new query tab.
    OpenQuery(String),
    Closed,
}

/// A modal showing the textual output of a CLI-backed tool.
///
/// Construct with [`ToolOutputView::new`] passing a title and the body text.
/// The body is shown in a scrollable monospace panel. Two footer buttons let
/// the user copy the output to the clipboard or open it as SQL in a new tab.
pub struct ToolOutputView {
    title: String,
    body: String,
    scroll: ScrollHandle,
    /// When true, the "Open in Query Tab" button is shown. Some outputs
    /// (e.g. Template Explorer listing) are not SQL and should not be opened.
    can_open_as_query: bool,
    frame: DialogFrame,
}

impl EventEmitter<ToolOutputEvent> for ToolOutputView {}

impl ToolOutputView {
    /// Create a new tool output modal.
    ///
    /// - `title`: the dialog title (e.g. "Template Explorer").
    /// - `body`: the CLI output to display.
    /// - `can_open_as_query`: whether to show the "Open in Query Tab" button.
    pub fn new(title: impl Into<String>, body: impl Into<String>, can_open_as_query: bool) -> Self {
        let body = body.into();
        let body = if body == crate::tool_dialog::CANCELLED_MESSAGE {
            crate::localization::text("tools.output.cancelled_message")
        } else {
            body
        };
        Self {
            title: title.into(),
            body,
            scroll: ScrollHandle::default(),
            can_open_as_query,
            frame: DialogFrame::new(Theme::scaled(760.0), Theme::scaled(560.0)),
        }
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        cx.emit(ToolOutputEvent::Closed);
    }

    fn copy_output(&self, cx: &mut Context<Self>) {
        cx.write_to_clipboard(gpui::ClipboardItem::new_string(self.body.clone()));
    }

    fn open_as_query(&self, cx: &mut Context<Self>) {
        cx.emit(ToolOutputEvent::OpenQuery(self.body.clone()));
    }
}

impl Render for ToolOutputView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let can_open = self.can_open_as_query;
        let body = self.body.clone();
        let scroll = self.scroll.clone();

        // bare_body: the pane owns its scroll container so monospace output
        // can also scroll horizontally.
        let mut dialog = Dialog::new("tool-output-dialog", |this: &mut Self| &mut this.frame)
            .title(self.title.clone())
            .easy_close(true)
            .on_close(|this, _window, cx| this.close(cx))
            .bare_body()
            .child(
                div()
                    .id("tool_output_scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_scroll()
                    .track_scroll(&scroll)
                    .child(
                        div()
                            .p(px(s(12.0)))
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text())
                            .font_family(Theme::editor_font_family())
                            .child(body),
                    ),
            )
            .button(
                Button::new("tool-output-copy")
                    .label(crate::localization::text("tools.output.copy_button"))
                    .on_click(cx.listener(|this, _, _window, cx| this.copy_output(cx))),
            );
        if can_open {
            dialog = dialog.button(
                Button::new("tool-output-open")
                    .primary()
                    .label(crate::localization::text("tools.output.open_query_button"))
                    .on_click(cx.listener(|this, _, _window, cx| this.open_as_query(cx))),
            );
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_output_event_closed_clone() {
        let e = ToolOutputEvent::Closed;
        let _ = e.clone();
    }

    #[test]
    fn tool_output_event_open_query_carries_text() {
        let e = ToolOutputEvent::OpenQuery("SELECT 1".into());
        match e {
            ToolOutputEvent::OpenQuery(s) => assert_eq!(s, "SELECT 1"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn can_open_as_query_flag() {
        let v = ToolOutputView::new("t", "b", true);
        assert!(v.can_open_as_query);
        let v = ToolOutputView::new("t", "b", false);
        assert!(!v.can_open_as_query);
    }
}
