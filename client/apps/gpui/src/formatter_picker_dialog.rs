//! Live-preview column formatter picker: JSON path, `{value}` templates, and number styles.

use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    ParentElement, Render, Styled, Subscription, Window, div, prelude::*, px,
};
use gpui_component::Disableable as _;
use gpui_component::Selectable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputEvent, InputState};
use sqlly_core::CellValue;

use crate::dialog::{Dialog, DialogFrame};
use crate::preferences::{
    ColumnFormat, JsonPathFormat, NumberStyle, NumberStyleFormat, ResultKind, TemplateFormat,
};
use crate::result_cell_format::display_transform;
use crate::result_export::cell_display;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq)]
pub enum FormatterChoice {
    JsonPath {
        path: String,
    },
    Template {
        template: String,
    },
    Number {
        style: NumberStyle,
        decimals: u8,
        thousands: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickerTab {
    JsonPath,
    Template,
    Number,
}

#[derive(Debug, Clone)]
pub struct FormatterPickerModel {
    pub column: String,
    pub sample: Option<CellValue>,
    pub choice: FormatterChoice,
}

impl FormatterPickerModel {
    /// Empty path/template, or a `;` in path/template (the directive is `;`-delimited) → Err.
    pub fn validate(&self) -> Result<(), String> {
        match &self.choice {
            FormatterChoice::JsonPath { path } => {
                if path.trim().is_empty() {
                    Err(crate::localization::text(
                        "editor.formatter_picker.json_path_required",
                    ))
                } else if path.contains(';') {
                    Err(crate::localization::text(
                        "editor.formatter_picker.json_path_semicolon",
                    ))
                } else {
                    Ok(())
                }
            }
            FormatterChoice::Template { template } => {
                if template.trim().is_empty() {
                    Err(crate::localization::text(
                        "editor.formatter_picker.template_required",
                    ))
                } else if template.contains(';') {
                    Err(crate::localization::text(
                        "editor.formatter_picker.template_semicolon",
                    ))
                } else {
                    Ok(())
                }
            }
            FormatterChoice::Number { .. } => Ok(()),
        }
    }

    /// The `ColumnFormat` equivalent of `choice`, so preview and apply share one code path.
    pub fn column_format(&self) -> ColumnFormat {
        match &self.choice {
            FormatterChoice::JsonPath { path } => ColumnFormat {
                kind: ResultKind::JsonPath,
                json_path_format: JsonPathFormat {
                    path: path.clone(),
                    ..JsonPathFormat::default()
                },
                ..ColumnFormat::default()
            },
            FormatterChoice::Template { template } => ColumnFormat {
                kind: ResultKind::Template,
                template_format: TemplateFormat {
                    template: template.clone(),
                },
                ..ColumnFormat::default()
            },
            FormatterChoice::Number {
                style,
                decimals,
                thousands,
            } => ColumnFormat {
                kind: ResultKind::Number,
                number_style_format: NumberStyleFormat {
                    style: *style,
                    decimals: *decimals,
                    use_thousands_separators: *thousands,
                },
                ..ColumnFormat::default()
            },
        }
    }

    /// `display_transform` rendered through `cell_display`; "—" when no sample or the transform returns None.
    pub fn preview(&self) -> String {
        let Some(sample) = &self.sample else {
            return "—".into();
        };
        match display_transform(sample, &self.column_format()) {
            Some(value) => cell_display(&value),
            None => "—".into(),
        }
    }

    /// `-- sqlly format: column=<column>; json=<path>` / `template=<t>` / `number=<d>; style=<s>; thousands=<b>`.
    pub fn directive(&self) -> String {
        match &self.choice {
            FormatterChoice::JsonPath { path } => {
                format!("-- sqlly format: column={}; json={path}", self.column)
            }
            FormatterChoice::Template { template } => {
                format!(
                    "-- sqlly format: column={}; template={template}",
                    self.column
                )
            }
            FormatterChoice::Number {
                style,
                decimals,
                thousands,
            } => format!(
                "-- sqlly format: column={}; number={decimals}; style={}; thousands={thousands}",
                self.column,
                style.directive_token()
            ),
        }
    }
}

pub enum FormatterPickerEvent {
    Apply { column: String, directive: String },
    Cancel,
}

pub struct FormatterPickerDialog {
    model: FormatterPickerModel,
    tab: PickerTab,
    path_input: Entity<InputState>,
    template_input: Entity<InputState>,
    decimals_input: Entity<InputState>,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<FormatterPickerEvent> for FormatterPickerDialog {}

impl FormatterPickerDialog {
    pub fn new(
        column: String,
        sample: Option<CellValue>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let path_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("$.customer.name")
                .default_value("$")
        });
        let template_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("{value|upper}")
                .default_value("{value}")
        });
        let decimals_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("2")
                .default_value("2")
        });
        let model = FormatterPickerModel {
            column,
            sample,
            choice: FormatterChoice::JsonPath { path: "$".into() },
        };
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &path_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) && this.tab == PickerTab::JsonPath {
                    let path = state.read(cx).value().to_string();
                    this.model.choice = FormatterChoice::JsonPath { path };
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &template_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) && this.tab == PickerTab::Template {
                    let template = state.read(cx).value().to_string();
                    this.model.choice = FormatterChoice::Template { template };
                    cx.notify();
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &decimals_input,
            window,
            |this: &mut Self, state, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change)
                    && this.tab == PickerTab::Number
                    && let Ok(decimals) = state.read(cx).value().trim().parse::<u8>()
                {
                    this.set_number_decimals(decimals.min(10), cx);
                }
            },
        ));
        Self {
            model,
            tab: PickerTab::JsonPath,
            path_input,
            template_input,
            decimals_input,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new_auto(Theme::scaled(480.0)),
            _subscriptions: subscriptions,
        }
    }

    fn set_tab(&mut self, tab: PickerTab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.model.choice = match tab {
            PickerTab::JsonPath => FormatterChoice::JsonPath {
                path: self.path_value(cx),
            },
            PickerTab::Template => FormatterChoice::Template {
                template: self.template_value(cx),
            },
            PickerTab::Number => self.number_choice(cx),
        };
        cx.notify();
    }

    fn path_value(&self, cx: &App) -> String {
        self.path_input.read(cx).value().to_string()
    }

    fn template_value(&self, cx: &App) -> String {
        self.template_input.read(cx).value().to_string()
    }

    fn number_choice(&self, cx: &App) -> FormatterChoice {
        let decimals = self
            .decimals_input
            .read(cx)
            .value()
            .trim()
            .parse::<u8>()
            .unwrap_or(2)
            .min(10);
        match &self.model.choice {
            FormatterChoice::Number {
                style, thousands, ..
            } => FormatterChoice::Number {
                style: *style,
                decimals,
                thousands: *thousands,
            },
            _ => FormatterChoice::Number {
                style: NumberStyle::Fixed,
                decimals,
                thousands: true,
            },
        }
    }

    fn set_number_decimals(&mut self, decimals: u8, cx: &mut Context<Self>) {
        self.tab = PickerTab::Number;
        self.model.choice = match self.number_choice(cx) {
            FormatterChoice::Number {
                style, thousands, ..
            } => FormatterChoice::Number {
                style,
                decimals,
                thousands,
            },
            other => other,
        };
        cx.notify();
    }

    fn set_number_style(&mut self, style: NumberStyle, cx: &mut Context<Self>) {
        self.tab = PickerTab::Number;
        self.model.choice = match self.number_choice(cx) {
            FormatterChoice::Number {
                decimals,
                thousands,
                ..
            } => FormatterChoice::Number {
                style,
                decimals,
                thousands,
            },
            other => other,
        };
        cx.notify();
    }

    fn toggle_thousands(&mut self, cx: &mut Context<Self>) {
        self.tab = PickerTab::Number;
        self.model.choice = match self.number_choice(cx) {
            FormatterChoice::Number {
                style,
                decimals,
                thousands,
            } => FormatterChoice::Number {
                style,
                decimals,
                thousands: !thousands,
            },
            other => other,
        };
        cx.notify();
    }

    fn emit_apply(&mut self, cx: &mut Context<Self>) {
        if self.model.validate().is_err() {
            return;
        }
        cx.emit(FormatterPickerEvent::Apply {
            column: self.model.column.clone(),
            directive: self.model.directive(),
        });
    }
}

impl Focusable for FormatterPickerDialog {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FormatterPickerDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let validation = self.model.validate();
        let apply_disabled = validation.is_err();
        let error = validation.err();
        let raw = self
            .model
            .sample
            .as_ref()
            .map(cell_display)
            .unwrap_or_else(|| "—".into());
        let raw_shown = if raw.chars().count() > 120 {
            format!("{}…", raw.chars().take(120).collect::<String>())
        } else {
            raw
        };
        let preview = self.model.preview();
        let tab = self.tab;
        let number = match &self.model.choice {
            FormatterChoice::Number {
                style, thousands, ..
            } => (*style, *thousands),
            _ => (NumberStyle::Fixed, true),
        };

        let mode_row = div()
            .flex()
            .flex_row()
            .gap(px(s(6.0)))
            .child(mode_button(
                "fmt-tab-json",
                &crate::localization::text("editor.formatter_picker.json_tab"),
                tab == PickerTab::JsonPath,
                cx,
                |this, cx| this.set_tab(PickerTab::JsonPath, cx),
            ))
            .child(mode_button(
                "fmt-tab-template",
                &crate::localization::text("editor.formatter_picker.template_tab"),
                tab == PickerTab::Template,
                cx,
                |this, cx| this.set_tab(PickerTab::Template, cx),
            ))
            .child(mode_button(
                "fmt-tab-number",
                &crate::localization::text("editor.formatter_picker.number_tab"),
                tab == PickerTab::Number,
                cx,
                |this, cx| this.set_tab(PickerTab::Number, cx),
            ));

        let fields = match tab {
            PickerTab::JsonPath => div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(field_label(&crate::localization::text(
                    "editor.formatter_picker.path_label",
                )))
                .child(
                    div()
                        .text_color(Theme::text())
                        .child(Input::new(&self.path_input)),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "editor.formatter_picker.path_help",
                        )),
                ),
            PickerTab::Template => div()
                .flex()
                .flex_col()
                .gap(px(s(6.0)))
                .child(field_label(&crate::localization::text(
                    "editor.formatter_picker.template_label",
                )))
                .child(
                    div()
                        .text_color(Theme::text())
                        .child(Input::new(&self.template_input)),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.80)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::text(
                            "editor.formatter_picker.template_help",
                        )),
                ),
            PickerTab::Number => {
                let (style, thousands) = number;
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .child(field_label(&crate::localization::text(
                        "editor.formatter_picker.style_label",
                    )))
                    .child(div().flex().flex_row().gap(px(s(6.0))).children(
                        NumberStyle::ALL.iter().copied().map(|candidate| {
                            mode_button(
                                format!("fmt-style-{}", candidate.directive_token()),
                                &candidate.label(),
                                candidate == style,
                                cx,
                                move |this, cx| this.set_number_style(candidate, cx),
                            )
                        }),
                    ))
                    .child(field_label(&crate::localization::text(
                        "editor.formatter_picker.decimals_label",
                    )))
                    .child(
                        div()
                            .w(px(s(80.0)))
                            .text_color(Theme::text())
                            .child(Input::new(&self.decimals_input)),
                    )
                    .child(
                        Checkbox::new("fmt-thousands")
                            .checked(thousands)
                            .label(crate::localization::text(
                                "editor.formatter_picker.thousands_label",
                            ))
                            .tab_stop(true)
                            .on_click(cx.listener(|this, _checked, _window, cx| {
                                this.toggle_thousands(cx);
                            })),
                    )
            }
        };

        let focus_handle = self.focus_handle.clone();
        Dialog::new("formatter-picker-dialog", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "editor.formatter_picker.dialog_title",
                &[("column", &self.model.column)],
            ))
            .key_context("FormatterPicker")
            .track_focus(&focus_handle)
            .on_close(|_this, _window, cx| cx.emit(FormatterPickerEvent::Cancel))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(crate::localization::text(
                        "editor.formatter_picker.preview_help",
                    )),
            )
            .child(mode_row)
            .child(fields)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .p(px(s(10.0)))
                    .rounded(px(s(6.0)))
                    .bg(Theme::sidebar_bg())
                    .border_1()
                    .border_color(Theme::border())
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.80)))
                            .text_color(Theme::text_dim())
                            .child(crate::localization::format(
                                "editor.formatter_picker.raw_preview",
                                &[("value", &raw_shown)],
                            )),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.87)))
                            .text_color(Theme::kind_string())
                            .child(crate::localization::format(
                                "editor.formatter_picker.formatted_preview",
                                &[("value", &preview)],
                            )),
                    ),
            )
            .children_body(error.map(|message| {
                div()
                    .text_size(px(Theme::font_size_ratio(0.80)))
                    .text_color(Theme::text_dim())
                    .child(message)
            }))
            .button(
                Button::new("fmt-apply")
                    .primary()
                    .label(crate::localization::text(
                        "editor.formatter_picker.apply_button",
                    ))
                    .disabled(apply_disabled)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        this.emit_apply(cx);
                    })),
            )
            .build(self, window, cx)
    }
}

fn field_label(text: &str) -> impl IntoElement {
    div()
        .font_weight(FontWeight::MEDIUM)
        .text_color(Theme::text())
        .text_size(px(Theme::font_size_ratio(0.87)))
        .child(text.to_string())
}

fn mode_button(
    id: impl Into<gpui::ElementId>,
    label: &str,
    selected: bool,
    cx: &mut Context<FormatterPickerDialog>,
    on_click: impl Fn(&mut FormatterPickerDialog, &mut Context<FormatterPickerDialog>) + 'static,
) -> Button {
    Button::new(id)
        .label(label.to_string())
        .ghost()
        .selected(selected)
        .on_click(cx.listener(move |this, _, _window, cx| on_click(this, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_format::{QueryColumnFormat, parse};
    use sqlly_core::CellValue;

    fn json_model(path: &str) -> FormatterPickerModel {
        FormatterPickerModel {
            column: "payload".into(),
            sample: Some(CellValue::String(r#"{"customer":{"name":"Ada"}}"#.into())),
            choice: FormatterChoice::JsonPath { path: path.into() },
        }
    }

    #[test]
    fn validate_rejects_empty_and_semicolon() {
        assert!(json_model("").validate().is_err());
        assert!(json_model("$.a;drop").validate().is_err());
        assert!(json_model("$.customer.name").validate().is_ok());
        let template = FormatterPickerModel {
            column: "name".into(),
            sample: None,
            choice: FormatterChoice::Template {
                template: "{value};bad".into(),
            },
        };
        assert!(template.validate().is_err());
    }

    #[test]
    fn preview_json_path_on_sample() {
        assert_eq!(json_model("$.customer.name").preview(), "Ada");
    }

    #[test]
    fn preview_number_bytes_on_integer_sample() {
        let model = FormatterPickerModel {
            column: "size".into(),
            sample: Some(CellValue::I64(1536)),
            choice: FormatterChoice::Number {
                style: NumberStyle::Bytes,
                decimals: 2,
                thousands: true,
            },
        };
        assert_eq!(model.preview(), "1.50 KiB");
    }

    #[test]
    fn directive_shapes() {
        assert_eq!(
            json_model("$.customer.name").directive(),
            "-- sqlly format: column=payload; json=$.customer.name"
        );
        let size = FormatterPickerModel {
            column: "size".into(),
            sample: None,
            choice: FormatterChoice::Number {
                style: NumberStyle::Bytes,
                decimals: 1,
                thousands: true,
            },
        };
        assert_eq!(
            size.directive(),
            "-- sqlly format: column=size; number=1; style=bytes; thousands=true"
        );
        let tmpl = FormatterPickerModel {
            column: "name".into(),
            sample: None,
            choice: FormatterChoice::Template {
                template: "{value|upper}".into(),
            },
        };
        assert_eq!(
            tmpl.directive(),
            "-- sqlly format: column=name; template={value|upper}"
        );
    }

    #[test]
    fn directive_round_trips_through_query_format_parse() {
        let directive = json_model("$.customer.name").directive();
        let hints = parse(&format!("{directive}\nSELECT 1"));
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::JsonPath {
                path: "$.customer.name".into(),
            }
        );
        let size = FormatterPickerModel {
            column: "size".into(),
            sample: None,
            choice: FormatterChoice::Number {
                style: NumberStyle::Bytes,
                decimals: 1,
                thousands: true,
            },
        };
        let hints = parse(&format!("{}\nSELECT 1", size.directive()));
        assert_eq!(
            hints[0].format,
            QueryColumnFormat::NumberStyle {
                style: NumberStyle::Bytes,
                decimals: 1,
                thousands: true,
            }
        );
    }
}
