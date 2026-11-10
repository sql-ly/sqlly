//! Docked Cell Details panel: the focused cell value (editable on an editable
//! result, staged like any grid edit) plus a row form.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Render, ScrollHandle,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Selectable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Textarea, TextareaState};
use sqlly_core::CellValue;

use crate::cell_detail_model::{
    CellDetail, CellEditGate, DockSide, FocusedCell, PanelEdit, display_text,
};
use crate::structured_data::{self, ContentPreview};
use crate::structured_data_viewer::StructuredDataViewer;
use crate::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetailView {
    Value,
    Row,
}

pub enum CellDetailEvent {
    OpenAsDialog,
    Close,
    ToggleSide,
    FocusColumn(usize),
    /// Stage an edit of the panel's cell (the pinned cell when pinned)
    /// through the grid's staged-edit path.
    StageEdit {
        slot: usize,
        focus: FocusedCell,
        edit: PanelEdit,
    },
}

/// An edit in progress: the multi-line editor and the cell it edits.
struct CellEdit {
    editor: Entity<TextareaState>,
    slot: usize,
    focus: FocusedCell,
    /// The value when editing began; applying unchanged text stages nothing.
    original: CellValue,
    /// Cmd/Ctrl+Enter applies and Escape reverts while the editor has focus.
    _keys: Subscription,
}

pub struct CellDetailPanel {
    detail: Option<CellDetail>,
    slot: usize,
    focus: Option<FocusedCell>,
    pinned: Option<(usize, FocusedCell)>,
    view_mode: DetailView,
    value_viewer: Option<Entity<StructuredDataViewer>>,
    value_key: Option<(usize, usize, usize)>,
    viewer_stale: bool,
    plain_scroll: ScrollHandle,
    focus_handle: FocusHandle,
    /// Whether the shown cell may be edited; `None` until the results view
    /// has decided (or when there is no cell).
    edit_gate: Option<CellEditGate>,
    edit: Option<CellEdit>,
    /// A non-blocking note about the last applied edit (invalid JSON), for
    /// the cell it was applied to.
    edit_notice: Option<((usize, usize, usize), String)>,
}

impl EventEmitter<CellDetailEvent> for CellDetailPanel {}

impl Focusable for CellDetailPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl CellDetailPanel {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            detail: None,
            slot: 0,
            focus: None,
            pinned: None,
            view_mode: DetailView::Value,
            value_viewer: None,
            value_key: None,
            viewer_stale: false,
            plain_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
            edit_gate: None,
            edit: None,
            edit_notice: None,
        }
    }

    pub fn set_detail(
        &mut self,
        slot: usize,
        detail: Option<CellDetail>,
        focus: Option<FocusedCell>,
        cx: &mut Context<Self>,
    ) {
        if let Some(pinned) = self.pinned
            && Some(pinned) != focus.map(|f| (slot, f))
        {
            return;
        }
        let key = focus.map(|f| (slot, f.source_row, f.column));
        let value_changed = match (&self.detail, &detail) {
            (Some(old), Some(new)) => old.value != new.value,
            (None, Some(_)) | (Some(_), None) => true,
            (None, None) => false,
        };
        if self.value_key != key || value_changed {
            self.viewer_stale = true;
            // The draft was for the old cell (or for a value since replaced,
            // e.g. by the edit it just staged).
            self.edit = None;
        }
        if self.value_key != key {
            self.edit_gate = None;
        }
        self.slot = slot;
        self.focus = focus;
        self.detail = detail;
        self.value_key = key;
        cx.notify();
    }

    pub fn apply_hydrated_value(&mut self, value: CellValue, cx: &mut Context<Self>) {
        if self.edit_gate == Some(CellEditGate::AwaitingFullValue) {
            self.edit_gate = Some(CellEditGate::Editable);
        }
        if let Some(detail) = self.detail.as_mut() {
            detail.value = value.clone();
            if let Some(field) = detail.fields.iter_mut().find(|f| f.is_focused) {
                field.value = display_text(&value);
                field.is_null = matches!(value, CellValue::Null);
            }
            self.viewer_stale = true;
            cx.notify();
        }
    }

    /// The full value of a truncated cell could not be loaded: show the
    /// explanation, and never let the preview be edited and staged.
    pub fn apply_hydration_unavailable(&mut self, banner: String, cx: &mut Context<Self>) {
        if self.edit_gate == Some(CellEditGate::AwaitingFullValue) {
            self.edit_gate = Some(CellEditGate::ReadOnly(crate::localization::text(
                "results.cell_details.full_value_unavailable",
            )));
        }
        self.apply_hydrated_value(CellValue::String(banner), cx);
    }

    /// The cell the panel is pinned to, if any.
    pub fn pinned(&self) -> Option<(usize, FocusedCell)> {
        self.pinned
    }

    /// Whether the shown cell `(slot, focus)` may be edited. Ignored for any
    /// other cell (a pinned panel keeps its own).
    pub fn set_edit_gate(
        &mut self,
        slot: usize,
        focus: FocusedCell,
        gate: CellEditGate,
        cx: &mut Context<Self>,
    ) {
        if self.slot != slot || self.focus != Some(focus) {
            return;
        }
        if gate != CellEditGate::Editable {
            self.edit = None;
        }
        if self.edit_gate.as_ref() != Some(&gate) {
            self.edit_gate = Some(gate);
            cx.notify();
        }
    }

    #[cfg(test)]
    pub fn toggle_pin_for_test(&mut self, cx: &mut Context<Self>) {
        self.toggle_pin(cx);
    }

    #[cfg(test)]
    pub fn edit_gate(&self) -> Option<&CellEditGate> {
        self.edit_gate.as_ref()
    }

    #[cfg(test)]
    pub fn is_editing(&self) -> bool {
        self.edit.is_some()
    }

    #[cfg(test)]
    pub fn set_draft(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = &self.edit {
            let text = text.to_string();
            edit.editor
                .update(cx, |state, cx| state.set_value(text, window, cx));
        }
    }

    /// Open the multi-line editor on the shown cell, seeded with its text.
    pub fn begin_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.edit.is_some() || self.edit_gate != Some(CellEditGate::Editable) {
            return;
        }
        let (Some(detail), Some(focus)) = (self.detail.as_ref(), self.focus) else {
            return;
        };
        let Some(seed) = crate::cell_detail_model::edit_seed_text(&detail.value) else {
            return;
        };
        let original = detail.value.clone();
        let editor = cx.new(|cx| {
            TextareaState::new(window, cx)
                .soft_wrap(true)
                .placeholder(crate::localization::input_placeholder(
                    window,
                    cx,
                    "results.cell_details.empty_string_placeholder",
                ))
                .default_value(seed)
        });
        editor.update(cx, |state, cx| state.focus(window, cx));
        // The editor's own Enter bindings insert a newline for
        // Cmd/Ctrl+Enter, and the app binds both chords globally, so the
        // panel claims them first — only while its editor has focus.
        let weak = cx.entity().downgrade();
        let keys = cx.intercept_keystrokes(move |event, window, cx| {
            let Some(panel) = weak.upgrade() else {
                return;
            };
            let keystroke = &event.keystroke;
            let modifiers = &keystroke.modifiers;
            let apply = keystroke.key == "enter"
                && crate::keybindings::primary_modifier(modifiers)
                && !modifiers.shift
                && !modifiers.alt;
            let revert = keystroke.key == "escape" && !modifiers.modified();
            if !apply && !revert {
                return;
            }
            let focused = panel
                .read(cx)
                .edit
                .as_ref()
                .is_some_and(|edit| edit.editor.read(cx).focus_handle(cx).is_focused(window));
            if !focused {
                return;
            }
            cx.stop_propagation();
            panel.update(cx, |panel, cx| {
                if apply {
                    panel.apply_edit(cx);
                } else {
                    panel.cancel_edit(cx);
                }
            });
        });
        self.edit = Some(CellEdit {
            editor,
            slot: self.slot,
            focus,
            original,
            _keys: keys,
        });
        self.edit_notice = None;
        self.view_mode = DetailView::Value;
        cx.notify();
    }

    /// Stage the editor's text for the edited cell. Unchanged text closes
    /// the editor without staging; a staged value closes it when the panel
    /// re-reads the cell, and a refused one leaves the draft in place.
    pub fn apply_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else {
            return;
        };
        let draft = edit.editor.read(cx).value().to_string();
        let Some(panel_edit) = crate::cell_detail_model::edit_for_apply(&edit.original, &draft)
        else {
            self.cancel_edit(cx);
            return;
        };
        let (slot, focus) = (edit.slot, edit.focus);
        self.edit_notice = self
            .detail
            .as_ref()
            .and_then(|detail| crate::cell_detail_model::json_warning(&detail.data_type, &draft))
            .map(|notice| ((slot, focus.source_row, focus.column), notice));
        cx.emit(CellDetailEvent::StageEdit {
            slot,
            focus,
            edit: panel_edit,
        });
        cx.notify();
    }

    /// Stage NULL for the edited cell (an empty draft stages '' instead).
    pub fn set_null_edit(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else {
            return;
        };
        let (slot, focus) = (edit.slot, edit.focus);
        self.edit_notice = None;
        cx.emit(CellDetailEvent::StageEdit {
            slot,
            focus,
            edit: PanelEdit::Null,
        });
    }

    /// Discard the draft and close the editor (Escape / Cancel).
    pub fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            cx.notify();
        }
    }

    /// The focused cell's raw value, for the byte-exact format picker.
    pub fn value_for_open(&self) -> Option<CellValue> {
        self.detail.as_ref().map(|detail| detail.value.clone())
    }

    pub fn preview_for_open(&self) -> Option<(String, ContentPreview, String)> {
        let detail = self.detail.as_ref()?;
        let preview = structured_data::classify_cell(&detail.value);
        let raw = raw_text_for_value(&detail.value);
        Some((detail.column_name.clone(), preview, raw))
    }

    fn toggle_pin(&mut self, cx: &mut Context<Self>) {
        if self.pinned.is_some() {
            self.pinned = None;
        } else if let Some(focus) = self.focus {
            self.pinned = Some((self.slot, focus));
        }
        cx.notify();
    }

    fn ensure_value_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.viewer_stale {
            return;
        }
        self.viewer_stale = false;
        let Some(detail) = self.detail.as_ref() else {
            self.value_viewer = None;
            return;
        };
        let preview = structured_data::classify_cell(&detail.value);
        let column = detail.column_name.clone();
        let raw = raw_text_for_value(&detail.value);
        self.value_viewer = match preview {
            ContentPreview::Text => None,
            ContentPreview::Json
            | ContentPreview::Xml
            | ContentPreview::Yaml
            | ContentPreview::Markdown => {
                Some(cx.new(|cx| StructuredDataViewer::new(&column, &raw, window, cx).embedded()))
            }
            ContentPreview::Hex => {
                // Hand the embedded hex view the bytes too, so its format
                // picker can re-read a binary value (e.g. as MessagePack).
                let payload = structured_data::picker_payload(&detail.value, &preview);
                Some(cx.new(|cx| {
                    let mut viewer =
                        StructuredDataViewer::new_hex(&column, raw, window, cx).embedded();
                    if let Some(payload) = payload {
                        viewer.set_payload(payload, None, cx);
                    }
                    viewer
                }))
            }
            ContentPreview::Image(fmt, bytes) => Some(cx.new(|cx| {
                StructuredDataViewer::new_image(&column, fmt, bytes, window, cx).embedded()
            })),
            ContentPreview::Spatial(geom) => Some(cx.new(|cx| {
                StructuredDataViewer::new_spatial(&column, raw, geom, window, cx).embedded()
            })),
        };
    }

    fn mode_button(
        &self,
        id: &'static str,
        label: String,
        mode: DetailView,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        let active = self.view_mode == mode;
        div()
            .id(SharedString::from(format!("cell-detail-mode-{id}")))
            .cursor_pointer()
            .px(px(s(10.0)))
            .py(px(s(3.0)))
            .rounded(px(s(4.0)))
            .text_size(px(Theme::font_size_ratio(0.92)))
            .when(active, |d| {
                d.bg(Theme::accent().opacity(0.2))
                    .text_color(Theme::accent())
            })
            .when(!active, |d| {
                d.text_color(Theme::text_dim())
                    .hover(|st| st.bg(Theme::row_hover()))
            })
            .child(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.view_mode != mode {
                    this.view_mode = mode;
                    cx.notify();
                }
            }))
    }

    fn render_header(&self, cx: &mut Context<Self>) -> gpui::Div {
        let s = Theme::scaled;
        let pinned = self.pinned.is_some();
        let (title, row_label) = match &self.detail {
            Some(d) => (
                format!("{} · {}", d.column_name, d.data_type),
                d.row_label.clone(),
            ),
            None => (
                crate::localization::text("results.cell_details.empty_title"),
                String::new(),
            ),
        };
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .h(px(Theme::chrome_bar_height()))
            .px(px(s(8.0)))
            .gap(px(s(8.0)))
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .flex_1()
                    .child(
                        div()
                            .text_size(px(Theme::font_size()))
                            .text_color(Theme::text())
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(title),
                    )
                    .when(!row_label.is_empty(), |d| {
                        // One line, cut with an ellipsis: in a narrow docked
                        // panel this label otherwise wraps word by word under
                        // the toolbar buttons and spills into the body.
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.85)))
                                .text_color(Theme::text_dim())
                                .whitespace_nowrap()
                                .overflow_hidden()
                                .text_ellipsis()
                                .child(row_label),
                        )
                    }),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(s(2.0)))
                            .p(px(s(2.0)))
                            .rounded(px(s(5.0)))
                            .border_1()
                            .border_color(Theme::border())
                            .child(self.mode_button(
                                "value",
                                crate::localization::text("results.cell_details.value_tab"),
                                DetailView::Value,
                                cx,
                            ))
                            .child(self.mode_button(
                                "row",
                                crate::localization::text("results.cell_details.row_tab"),
                                DetailView::Row,
                                cx,
                            )),
                    )
                    .child(
                        Button::new("cell-detail-pin")
                            .small()
                            .ghost()
                            .label(crate::localization::text(if pinned {
                                "results.cell_details.unpin_button"
                            } else {
                                "results.cell_details.pin_button"
                            }))
                            .when(pinned, |b| b.selected(true))
                            .on_click(cx.listener(|this, _, _, cx| this.toggle_pin(cx))),
                    )
                    .child(
                        Button::new("cell-detail-side")
                            .small()
                            .ghost()
                            .label(crate::localization::text(
                                "results.cell_details.switch_dock_side_button",
                            ))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(CellDetailEvent::ToggleSide);
                            })),
                    )
                    .child(
                        Button::new("cell-detail-open")
                            .small()
                            .ghost()
                            .label(crate::localization::text(
                                "results.cell_details.open_window_button",
                            ))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(CellDetailEvent::OpenAsDialog);
                            })),
                    )
                    .child(
                        Button::new("cell-detail-close")
                            .small()
                            .ghost()
                            .label(crate::localization::text(
                                "results.cell_details.close_button",
                            ))
                            .on_click(cx.listener(|_, _, _, cx| {
                                cx.emit(CellDetailEvent::Close);
                            })),
                    ),
            )
    }

    fn render_plain_value(&self, detail: &CellDetail) -> gpui::Stateful<gpui::Div> {
        let text = display_text(&detail.value);
        let is_null = matches!(detail.value, CellValue::Null);
        div()
            .id("cell-detail-plain")
            .flex_1()
            .min_h_0()
            .overflow_scroll()
            .track_scroll(&self.plain_scroll)
            .p(px(Theme::scaled(8.0)))
            .font_family(Theme::editor_font_family())
            .text_size(px(Theme::editor_font_size()))
            .when(is_null, |d| d.text_color(Theme::text_dim()))
            .child(text)
    }

    fn render_row_form(
        &self,
        detail: &CellDetail,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let s = Theme::scaled;
        let row_h = Theme::row_height();
        let mut body = div()
            .id("cell-detail-row")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll();
        for (i, field) in detail.fields.iter().enumerate() {
            let is_null = field.is_null;
            let name = field.name.clone();
            let value = field.value.clone();
            let focused = field.is_focused;
            body = body.child(
                div()
                    .id(SharedString::from(format!("cell-detail-field-{i}")))
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(row_h))
                    .px(px(s(8.0)))
                    .gap(px(s(8.0)))
                    .cursor_pointer()
                    .when(focused, |d| d.bg(Theme::selection_fill()))
                    .hover(|st| st.bg(Theme::row_hover()))
                    .child(
                        div()
                            .w(px(s(140.0)))
                            .text_color(Theme::text_dim())
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .overflow_hidden()
                            .text_size(px(Theme::font_size()))
                            .when(is_null, |d| d.text_color(Theme::text_dim()))
                            .child(value),
                    )
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.emit(CellDetailEvent::FocusColumn(i));
                    })),
            );
        }
        body
    }
}

impl CellDetailPanel {
    /// The editor filling the Value view while an edit is open.
    fn render_edit_body(&self) -> Option<gpui::AnyElement> {
        let edit = self.edit.as_ref()?;
        if self.view_mode != DetailView::Value {
            return None;
        }
        Some(
            div()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .p(px(Theme::scaled(8.0)))
                .child(
                    Textarea::new(&edit.editor)
                        .h(gpui::relative(1.0))
                        .font_family(Theme::editor_font_family())
                        .text_size(px(Theme::editor_font_size())),
                )
                .into_any_element(),
        )
    }

    /// The strip under the Value view: Edit (or why editing is not
    /// possible), and while editing Set NULL / Cancel / Apply.
    fn render_edit_footer(&self, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.view_mode != DetailView::Value || self.detail.is_none() {
            return None;
        }
        let s = Theme::scaled;
        let note = |text: String| {
            div()
                .flex_1()
                .min_w_0()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(text)
        };
        let notice = self.value_key.and_then(|key| {
            self.edit_notice
                .as_ref()
                .filter(|(at, _)| *at == key)
                .map(|(_, notice)| notice.clone())
        });
        let mut bar = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .items_center()
            .gap(px(s(6.0)))
            .px(px(s(8.0)))
            .py(px(s(4.0)))
            .border_t_1()
            .border_color(Theme::border());
        if self.edit.is_some() {
            bar = bar
                .child(
                    Button::new("cell-detail-set-null")
                        .small()
                        .ghost()
                        .label(crate::localization::text(
                            "results.cell_details.set_null_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.set_null_edit(cx))),
                )
                .child(note(crate::localization::format(
                    "results.cell_details.edit_hint",
                    &[(
                        "apply_shortcut",
                        &format!("{}+Enter", crate::keybindings::PRIMARY_LABEL),
                    )],
                )))
                .child(
                    Button::new("cell-detail-cancel-edit")
                        .small()
                        .ghost()
                        .label(crate::localization::text(
                            "results.cell_details.cancel_edit_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.cancel_edit(cx))),
                )
                .child(
                    Button::new("cell-detail-apply-edit")
                        .small()
                        .primary()
                        .label(crate::localization::text(
                            "results.cell_details.apply_edit_button",
                        ))
                        .on_click(cx.listener(|this, _, _, cx| this.apply_edit(cx))),
                );
            return Some(bar);
        }
        match self.edit_gate.as_ref()? {
            CellEditGate::Editable => {
                bar = bar.child(note(notice.unwrap_or_default())).child(
                    Button::new("cell-detail-edit")
                        .small()
                        .ghost()
                        .label(crate::localization::text(
                            "results.cell_details.edit_button",
                        ))
                        .on_click(cx.listener(|this, _, window, cx| this.begin_edit(window, cx))),
                );
            }
            CellEditGate::AwaitingFullValue => {
                bar = bar.child(note(crate::localization::text(
                    "results.cell_details.loading_full_value",
                )));
            }
            CellEditGate::ReadOnly(reason) => {
                bar = bar.child(note(crate::localization::format(
                    "results.cell_details.read_only_reason",
                    &[("reason", reason)],
                )));
            }
        }
        Some(bar)
    }
}

impl Render for CellDetailPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_value_viewer(window, cx);
        let header = self.render_header(cx);
        let edit_footer = self.render_edit_footer(cx);
        let body = if let Some(edit_body) = self.render_edit_body() {
            edit_body
        } else {
            self.render_view_body(cx)
        };
        div()
            .flex()
            .flex_col()
            .size_full()
            .bg(Theme::panel_bg())
            .track_focus(&self.focus_handle)
            .child(header)
            .child(body)
            .children(edit_footer)
    }
}

impl CellDetailPanel {
    fn render_view_body(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        match (self.view_mode, self.detail.as_ref()) {
            (_, None) => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(Theme::text_dim())
                .text_size(px(Theme::font_size()))
                .child(crate::localization::text(
                    "results.cell_details.select_cell_empty_state",
                ))
                .into_any_element(),
            (DetailView::Row, Some(detail)) => self.render_row_form(detail, cx).into_any_element(),
            (DetailView::Value, Some(detail)) => {
                if let Some(viewer) = &self.value_viewer {
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .child(viewer.clone())
                        .into_any_element()
                } else {
                    self.render_plain_value(detail).into_any_element()
                }
            }
        }
    }
}

pub fn raw_text_for_value(value: &CellValue) -> String {
    match value {
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => structured_data::hex_dump(b),
        other => display_text(other),
    }
}

#[allow(dead_code)]
pub fn dock_size(side: DockSide, width: f32, height: f32) -> f32 {
    match side {
        DockSide::Right => width,
        DockSide::Bottom => height,
    }
}
