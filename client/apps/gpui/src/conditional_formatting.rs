//! Conditional formatting configuration: persisted rules (by column name)
//! and the editor dialog that manages them.
//!
//! The evaluation itself lives in the `sqlly-datatable` crate (rules travel
//! on `GridConfig`); this module owns persistence and the UI. Rules are
//! global by column name — a rule for `amount` colors every grid whose
//! result has an `amount` column — which matches how the per-column format
//! overrides already behave.

use gpui::{
    Context, Entity, EventEmitter, IntoElement, ParentElement, Render, Styled, WeakEntity, Window,
    div, prelude::*, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::color_picker::{ColorPicker, ColorPickerEvent, ColorPickerState};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use sqlly_datatable::{
    ConditionalCondition, ConditionalKind, ConditionalRule, ConditionalStyle, RgbaColor,
};

use crate::dialog::{Dialog, DialogFrame};
use crate::form_controls;
use crate::results::ResultsView;
use crate::theme::Theme;

/// Persisted shape: versioned wrapper over the crate's serde-able rules.
#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(default)]
struct RulesFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    rules: Vec<ConditionalRule>,
}

#[cfg(not(test))]
const RULES_FILE_NAME: &str = "conditional-formatting.json";

#[cfg(all(not(test), not(target_arch = "wasm32")))]
fn rules_path() -> std::path::PathBuf {
    let mut p = crate::preferences::preferences_path();
    p.push(RULES_FILE_NAME);
    p
}

// Tests never read or write the developer's real rules file (a rule saved in
// the installed app must not change a unit test's outcome, and a dialog test
// must not overwrite the user's rules): each test thread gets its own store.
#[cfg(test)]
thread_local! {
    static TEST_RULES: std::cell::RefCell<Vec<ConditionalRule>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Every persisted rule, in declaration order (first match wins per column).
#[cfg(test)]
pub fn load_rules() -> Vec<ConditionalRule> {
    TEST_RULES.with(|rules| rules.borrow().clone())
}

#[cfg(test)]
pub fn save_rules(rules: &[ConditionalRule]) {
    TEST_RULES.with(|store| *store.borrow_mut() = rules.to_vec());
}

/// Every persisted rule, in declaration order (first match wins per column).
#[cfg(all(not(test), not(target_arch = "wasm32")))]
pub fn load_rules() -> Vec<ConditionalRule> {
    std::fs::read_to_string(rules_path())
        .ok()
        .and_then(|text| serde_json::from_str::<RulesFile>(&text).ok())
        .map(|file| file.rules)
        .unwrap_or_default()
}

#[cfg(all(not(test), target_arch = "wasm32"))]
pub fn load_rules() -> Vec<ConditionalRule> {
    crate::host_storage::load_text(crate::host_storage::StorageArea::Data, RULES_FILE_NAME)
        .and_then(|text| serde_json::from_str::<RulesFile>(&text).ok())
        .map(|file| file.rules)
        .unwrap_or_default()
}

#[cfg(all(not(test), not(target_arch = "wasm32")))]
pub fn save_rules(rules: &[ConditionalRule]) {
    let file = RulesFile {
        version: 1,
        rules: rules.to_vec(),
    };
    if let Ok(text) = crate::config_version::to_string_pretty(&file) {
        let _ = std::fs::write(rules_path(), text);
    }
}

#[cfg(all(not(test), target_arch = "wasm32"))]
pub fn save_rules(rules: &[ConditionalRule]) {
    let file = RulesFile {
        version: 1,
        rules: rules.to_vec(),
    };
    if let Ok(text) = crate::config_version::to_string_pretty(&file) {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            RULES_FILE_NAME,
            &text,
        );
    }
}

/// Human summary of one rule for the dialog's list.
pub fn rule_summary(rule: &ConditionalRule) -> String {
    match &rule.kind {
        ConditionalKind::Rule { condition, style } => {
            let condition = match condition {
                ConditionalCondition::IsNull => {
                    crate::localization::text("results.conditional_formatting.condition_is_null")
                }
                ConditionalCondition::IsNotNull => crate::localization::text(
                    "results.conditional_formatting.condition_is_not_null",
                ),
                ConditionalCondition::GreaterThan(v) => crate::localization::format(
                    "results.conditional_formatting.condition_greater_than",
                    &[("value", v)],
                ),
                ConditionalCondition::LessThan(v) => crate::localization::format(
                    "results.conditional_formatting.condition_less_than",
                    &[("value", v)],
                ),
                ConditionalCondition::Between(a, b) => crate::localization::format(
                    "results.conditional_formatting.condition_between",
                    &[("first_value", a), ("second_value", b)],
                ),
                ConditionalCondition::EqualsText(t) => crate::localization::format(
                    "results.conditional_formatting.condition_text_equals",
                    &[("text", t)],
                ),
                ConditionalCondition::ContainsText(t) => crate::localization::format(
                    "results.conditional_formatting.condition_text_contains",
                    &[("text", t)],
                ),
            };
            let mut effects = Vec::new();
            if style.foreground.is_some() {
                effects.push(crate::localization::text(
                    "results.conditional_formatting.effect_text_color",
                ));
            }
            if style.background.is_some() {
                effects.push(crate::localization::text(
                    "results.conditional_formatting.effect_background",
                ));
            }
            if style.bold {
                effects.push(crate::localization::text(
                    "results.conditional_formatting.effect_bold",
                ));
            }
            let effects = if effects.is_empty() {
                crate::localization::text("results.conditional_formatting.effect_none")
            } else {
                effects.join(" + ")
            };
            crate::localization::format(
                "results.conditional_formatting.rule_summary",
                &[("condition", &condition), ("effects", &effects)],
            )
        }
        ConditionalKind::ColorScale { .. } => {
            crate::localization::text("results.conditional_formatting.color_scale_summary")
        }
        ConditionalKind::DataBar { .. } => {
            crate::localization::text("results.conditional_formatting.data_bar_summary")
        }
    }
}

/// The editable templates the Add-rule kind button cycles through.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RuleTemplate {
    IsNull,
    IsNotNull,
    GreaterThan,
    LessThan,
    Between,
    EqualsText,
    ContainsText,
    ColorScale,
    DataBar,
}

impl RuleTemplate {
    const ALL: [RuleTemplate; 9] = [
        RuleTemplate::GreaterThan,
        RuleTemplate::LessThan,
        RuleTemplate::Between,
        RuleTemplate::EqualsText,
        RuleTemplate::ContainsText,
        RuleTemplate::IsNull,
        RuleTemplate::IsNotNull,
        RuleTemplate::ColorScale,
        RuleTemplate::DataBar,
    ];

    fn label(self) -> String {
        crate::localization::text(match self {
            RuleTemplate::IsNull => "results.conditional_formatting.template_is_null",
            RuleTemplate::IsNotNull => "results.conditional_formatting.template_is_not_null",
            RuleTemplate::GreaterThan => "results.conditional_formatting.template_greater_than",
            RuleTemplate::LessThan => "results.conditional_formatting.template_less_than",
            RuleTemplate::Between => "results.conditional_formatting.template_between",
            RuleTemplate::EqualsText => "results.conditional_formatting.template_text_equals",
            RuleTemplate::ContainsText => "results.conditional_formatting.template_text_contains",
            RuleTemplate::ColorScale => "results.conditional_formatting.template_color_scale",
            RuleTemplate::DataBar => "results.conditional_formatting.template_data_bar",
        })
    }

    fn needs_first_value(self) -> bool {
        matches!(
            self,
            RuleTemplate::GreaterThan
                | RuleTemplate::LessThan
                | RuleTemplate::Between
                | RuleTemplate::EqualsText
                | RuleTemplate::ContainsText
        )
    }

    fn needs_second_value(self) -> bool {
        matches!(self, RuleTemplate::Between)
    }

    fn is_numeric(self) -> bool {
        matches!(
            self,
            RuleTemplate::GreaterThan | RuleTemplate::LessThan | RuleTemplate::Between
        )
    }

    fn uses_style(self) -> bool {
        !matches!(self, RuleTemplate::ColorScale | RuleTemplate::DataBar)
    }
}

fn hsla_to_rgba(color: gpui::Hsla) -> RgbaColor {
    let rgba: gpui::Rgba = color.into();
    RgbaColor::new(rgba.r, rgba.g, rgba.b, rgba.a)
}

/// Build a rule from the editor's current values. Pure, so validation is
/// unit tested.
#[allow(clippy::too_many_arguments)]
fn build_rule(
    column: &str,
    template: RuleTemplate,
    first: &str,
    second: &str,
    foreground: Option<gpui::Hsla>,
    background: Option<gpui::Hsla>,
    bold: bool,
    scale_min: gpui::Hsla,
    scale_max: gpui::Hsla,
    bar_color: gpui::Hsla,
) -> Result<ConditionalRule, String> {
    let numeric = |raw: &str| -> Result<f64, String> {
        raw.trim().parse::<f64>().map_err(|_| {
            crate::localization::format(
                "results.conditional_formatting.invalid_number",
                &[("value", raw.trim())],
            )
        })
    };
    let kind = match template {
        RuleTemplate::ColorScale => ConditionalKind::ColorScale {
            min: hsla_to_rgba(scale_min),
            max: hsla_to_rgba(scale_max),
        },
        RuleTemplate::DataBar => ConditionalKind::DataBar {
            color: hsla_to_rgba(bar_color),
        },
        template => {
            let condition = match template {
                RuleTemplate::IsNull => ConditionalCondition::IsNull,
                RuleTemplate::IsNotNull => ConditionalCondition::IsNotNull,
                RuleTemplate::GreaterThan => ConditionalCondition::GreaterThan(numeric(first)?),
                RuleTemplate::LessThan => ConditionalCondition::LessThan(numeric(first)?),
                RuleTemplate::Between => {
                    ConditionalCondition::Between(numeric(first)?, numeric(second)?)
                }
                RuleTemplate::EqualsText => {
                    if first.trim().is_empty() {
                        return Err(crate::localization::text(
                            "results.conditional_formatting.text_match_required",
                        ));
                    }
                    ConditionalCondition::EqualsText(first.trim().to_string())
                }
                RuleTemplate::ContainsText => {
                    if first.trim().is_empty() {
                        return Err(crate::localization::text(
                            "results.conditional_formatting.text_match_required",
                        ));
                    }
                    ConditionalCondition::ContainsText(first.trim().to_string())
                }
                _ => unreachable!(),
            };
            let style = ConditionalStyle {
                foreground: foreground.map(hsla_to_rgba),
                background: background.map(hsla_to_rgba),
                bold,
            };
            if style.foreground.is_none() && style.background.is_none() && !style.bold {
                return Err(crate::localization::text(
                    "results.conditional_formatting.style_required",
                ));
            }
            ConditionalKind::Rule { condition, style }
        }
    };
    Ok(ConditionalRule {
        column: column.to_string(),
        kind,
    })
}

// ── Dialog ─────────────────────────────────────────────────────────────────

pub struct ConditionalFormattingDialog {
    column: String,
    /// The FULL persisted rule list; edits touch only this column's rules.
    rules: Vec<ConditionalRule>,
    template_index: usize,
    first_input: Entity<InputState>,
    second_input: Entity<InputState>,
    foreground_picker: Entity<ColorPickerState>,
    background_picker: Entity<ColorPickerState>,
    scale_min_picker: Entity<ColorPickerState>,
    scale_max_picker: Entity<ColorPickerState>,
    bar_picker: Entity<ColorPickerState>,
    foreground: Option<gpui::Hsla>,
    background: Option<gpui::Hsla>,
    scale_min: gpui::Hsla,
    scale_max: gpui::Hsla,
    bar_color: gpui::Hsla,
    bold: bool,
    error: Option<String>,
    /// The results view whose grids refresh immediately on change; other
    /// grids pick the rules up on their next build.
    results: WeakEntity<ResultsView>,
    /// Rule-template dropdown, mirroring `template_index`.
    template_select: Entity<SelectState<Vec<String>>>,
    /// Chrome geometry for the inline (non-modal) card.
    frame: DialogFrame,
    /// True once popped out into its own OS window: the window boundary is
    /// the dialog boundary and close tears the window down.
    windowed: bool,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<()> for ConditionalFormattingDialog {}

impl ConditionalFormattingDialog {
    pub fn new(
        column: String,
        results: WeakEntity<ResultsView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.conditional_formatting.first_value_placeholder",
            ))
        });
        let second_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(crate::localization::input_placeholder(
                window,
                cx,
                "results.conditional_formatting.second_value_placeholder",
            ))
        });
        let foreground_picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let background_picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let scale_min_picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let scale_max_picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let bar_picker = cx.new(|cx| ColorPickerState::new(window, cx));
        let template_select = cx.new(|cx| {
            SelectState::new(
                RuleTemplate::ALL
                    .iter()
                    .map(|t| t.label().to_string())
                    .collect::<Vec<_>>(),
                Some(gpui_component::IndexPath::new(0)),
                window,
                cx,
            )
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe(
            &template_select,
            |this: &mut Self, select, event: &SelectEvent<Vec<String>>, cx| {
                let SelectEvent::Confirm(value) = event;
                if value.is_none() {
                    return;
                }
                // By row (parallel to `RuleTemplate::ALL`), never by the
                // translated label.
                let row = select.read(cx).selected_index(cx).map(|ix| ix.row);
                if let Some(ix) = row.filter(|ix| *ix < RuleTemplate::ALL.len()) {
                    this.template_index = ix;
                    this.error = None;
                    cx.notify();
                }
            },
        ));
        for input in [&first_input, &second_input] {
            subscriptions.push(cx.subscribe(
                input,
                |this: &mut Self, _state, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                },
            ));
        }
        type PickerApply = fn(&mut ConditionalFormattingDialog, Option<gpui::Hsla>);
        let pickers: [(&Entity<ColorPickerState>, PickerApply); 5] = [
            (&foreground_picker, |this, c| this.foreground = c),
            (&background_picker, |this, c| this.background = c),
            (&scale_min_picker, |this, c| {
                if let Some(c) = c {
                    this.scale_min = c;
                }
            }),
            (&scale_max_picker, |this, c| {
                if let Some(c) = c {
                    this.scale_max = c;
                }
            }),
            (&bar_picker, |this, c| {
                if let Some(c) = c {
                    this.bar_color = c;
                }
            }),
        ];
        for (picker, apply) in pickers {
            subscriptions.push(cx.subscribe(
                picker,
                move |this: &mut Self, _state, event: &ColorPickerEvent, cx| {
                    let ColorPickerEvent::Change(color) = event;
                    apply(this, *color);
                    cx.notify();
                },
            ));
        }
        Self {
            column,
            rules: load_rules(),
            template_index: 0,
            first_input,
            second_input,
            foreground_picker,
            background_picker,
            scale_min_picker,
            scale_max_picker,
            bar_picker,
            foreground: None,
            background: None,
            scale_min: gpui::hsla(0.33, 0.6, 0.45, 1.0),
            scale_max: gpui::hsla(0.0, 0.7, 0.5, 1.0),
            bar_color: gpui::hsla(0.58, 0.6, 0.5, 1.0),
            bold: false,
            error: None,
            results,
            template_select,
            frame: DialogFrame::new_auto(Theme::scaled(560.0)),
            windowed: false,
            _subscriptions: subscriptions,
        }
    }

    /// Pop out into a standalone OS window; the inline card closes and the
    /// same dialog (rules list included) carries on in the window.
    fn pop_out(&mut self, cx: &mut Context<Self>) {
        let entity = cx.entity();
        // Opening the window renders this dialog into it immediately, which
        // must not happen inside its own update — defer past it.
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized(
                    "results.conditional_formatting.detached_title",
                    Theme::scaled(560.0),
                    Theme::scaled(480.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |dialog, _| dialog.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                // The window owns the dialog now; drop the inline card.
                entity.update(cx, |_, cx| cx.emit(()));
            }
        });
    }

    fn template(&self) -> RuleTemplate {
        RuleTemplate::ALL[self.template_index % RuleTemplate::ALL.len()]
    }

    fn persist_and_refresh(&mut self, cx: &mut Context<Self>) {
        save_rules(&self.rules);
        if let Some(results) = self.results.upgrade() {
            results.update(cx, |results, cx| results.refresh_grid_formatting(cx));
        }
        cx.notify();
    }

    fn add_rule(&mut self, cx: &mut Context<Self>) {
        let first = self.first_input.read(cx).value().to_string();
        let second = self.second_input.read(cx).value().to_string();
        match build_rule(
            &self.column,
            self.template(),
            &first,
            &second,
            self.foreground,
            self.background,
            self.bold,
            self.scale_min,
            self.scale_max,
            self.bar_color,
        ) {
            Ok(rule) => {
                self.rules.push(rule);
                self.error = None;
                self.persist_and_refresh(cx);
            }
            Err(error) => {
                self.error = Some(error);
                cx.notify();
            }
        }
    }

    fn remove_rule(&mut self, position: usize, cx: &mut Context<Self>) {
        let mut seen = 0usize;
        let column = self.column.clone();
        self.rules.retain(|rule| {
            if rule.column.eq_ignore_ascii_case(&column) {
                let keep = seen != position;
                seen += 1;
                keep
            } else {
                true
            }
        });
        self.persist_and_refresh(cx);
    }
}

impl Render for ConditionalFormattingDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let template = self.template();
        let column_rules: Vec<(usize, String)> = self
            .rules
            .iter()
            .filter(|rule| rule.column.eq_ignore_ascii_case(&self.column))
            .enumerate()
            .map(|(i, rule)| (i, rule_summary(rule)))
            .collect();

        let mut existing = div().flex().flex_col().gap(px(s(2.0)));
        if column_rules.is_empty() {
            existing = existing.child(
                div()
                    .text_color(Theme::text_dim())
                    .text_size(px(Theme::font_size_ratio(0.85)))
                    .child(crate::localization::text(
                        "results.conditional_formatting.no_rules",
                    )),
            );
        }
        for (position, summary) in column_rules {
            existing =
                existing.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap(px(s(8.0)))
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(0.0))
                                .truncate()
                                .text_size(px(Theme::font_size_ratio(0.88)))
                                .text_color(Theme::text())
                                .child(summary),
                        )
                        .child(
                            Button::new(("cond-remove", position))
                                .label(crate::localization::text(
                                    "results.conditional_formatting.remove_rule_button",
                                ))
                                .ghost()
                                .xsmall()
                                .on_click(cx.listener(move |this, _ev, _w, cx| {
                                    this.remove_rule(position, cx)
                                })),
                        ),
                );
        }

        // Same rows, field frames, and control size as the Settings window
        // (`form_controls`): default-size widgets, a 180px label column, and
        // a pinned text color so typed values are not black-on-dark.
        let mut editor = div().flex().flex_col().child(form_controls::labeled_row(
            crate::localization::text("results.conditional_formatting.rule_label"),
            form_controls::field(Select::new(&self.template_select).w_full()),
        ));
        if template.needs_first_value() {
            let label = crate::localization::text(if template.is_numeric() {
                "results.conditional_formatting.number_label"
            } else {
                "results.conditional_formatting.text_label"
            });
            editor = editor.child(form_controls::labeled_row(
                label,
                form_controls::field(Input::new(&self.first_input)),
            ));
            if template.needs_second_value() {
                editor = editor.child(form_controls::labeled_row(
                    crate::localization::text("results.conditional_formatting.and_label"),
                    form_controls::field(Input::new(&self.second_input)),
                ));
            }
        }
        if template.uses_style() {
            editor = editor
                .child(form_controls::labeled_row(
                    crate::localization::text("results.conditional_formatting.text_color_label"),
                    ColorPicker::new(&self.foreground_picker),
                ))
                .child(form_controls::labeled_row(
                    crate::localization::text("results.conditional_formatting.fill_color_label"),
                    ColorPicker::new(&self.background_picker),
                ))
                .child(
                    form_controls::checkbox_row(
                        Checkbox::new("cond-bold")
                            .checked(self.bold)
                            .label(crate::localization::text(
                                "results.conditional_formatting.bold_label",
                            ))
                            .tab_stop(true)
                            .on_click(cx.listener(|this, checked: &bool, _w, cx| {
                                this.bold = *checked;
                                cx.notify();
                            })),
                    )
                    .pl(px(form_controls::control_column_inset())),
                );
        } else if template == RuleTemplate::ColorScale {
            editor = editor
                .child(form_controls::labeled_row(
                    crate::localization::text("results.conditional_formatting.low_color_label"),
                    ColorPicker::new(&self.scale_min_picker),
                ))
                .child(form_controls::labeled_row(
                    crate::localization::text("results.conditional_formatting.high_color_label"),
                    ColorPicker::new(&self.scale_max_picker),
                ));
        } else {
            editor = editor.child(form_controls::labeled_row(
                crate::localization::text("results.conditional_formatting.bar_color_label"),
                ColorPicker::new(&self.bar_picker),
            ));
        }
        editor = editor.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(8.0)))
                .pt(px(s(6.0)))
                .child(
                    Button::new("cond-add")
                        .label(crate::localization::text(
                            "results.conditional_formatting.add_rule_button",
                        ))
                        .small()
                        .on_click(cx.listener(|this, _ev, _w, cx| this.add_rule(cx))),
                )
                .children(self.error.clone().map(|error| {
                    div()
                        .text_size(px(Theme::font_size_ratio(0.82)))
                        .text_color(Theme::error())
                        .child(error)
                })),
        );

        let body = div()
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.82)))
                    .text_color(Theme::text_dim())
                    .whitespace_normal()
                    .child(crate::localization::text(
                        "results.conditional_formatting.rules_explanation",
                    )),
            )
            .child(existing)
            .child(div().h(px(1.0)).bg(Theme::border()))
            .child(editor);

        // Non-blocking: the grid behind stays fully interactive, and edits
        // apply live — so Escape / click-outside always dismiss, dirty or not.
        let mut dialog = Dialog::new("conditional-formatting", |this: &mut Self| &mut this.frame)
            .title(crate::localization::format(
                "results.conditional_formatting.dialog_title",
                &[("column_name", &self.column)],
            ))
            .is_modal(false)
            .easy_close(true)
            .easy_close_when_dirty(true)
            .windowed(self.windowed)
            .on_close(|this, window, cx| {
                if this.windowed {
                    window.remove_window();
                } else {
                    cx.emit(());
                }
            })
            .child(body);
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsla(h: f32) -> gpui::Hsla {
        gpui::hsla(h, 0.5, 0.5, 1.0)
    }

    #[test]
    fn build_rule_validates_values_and_styles() {
        // Numeric parse failure.
        let err = build_rule(
            "amount",
            RuleTemplate::GreaterThan,
            "abc",
            "",
            None,
            Some(hsla(0.1)),
            false,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap_err();
        assert!(err.contains("not a number"));

        // A style-less rule is rejected.
        let err = build_rule(
            "amount",
            RuleTemplate::GreaterThan,
            "10",
            "",
            None,
            None,
            false,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap_err();
        assert!(err.contains("Pick"));

        // A good threshold rule.
        let rule = build_rule(
            "amount",
            RuleTemplate::Between,
            "5",
            "10",
            None,
            Some(hsla(0.1)),
            true,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap();
        assert_eq!(rule.column, "amount");
        match rule.kind {
            ConditionalKind::Rule { condition, style } => {
                assert!(
                    matches!(condition, ConditionalCondition::Between(a, b) if a == 5.0 && b == 10.0)
                );
                assert!(style.bold);
                assert!(style.background.is_some());
            }
            other => panic!("expected threshold rule, got {other:?}"),
        }

        // Scale and bar need no style/value.
        let scale = build_rule(
            "amount",
            RuleTemplate::ColorScale,
            "",
            "",
            None,
            None,
            false,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap();
        assert!(matches!(scale.kind, ConditionalKind::ColorScale { .. }));
        let bar = build_rule(
            "amount",
            RuleTemplate::DataBar,
            "",
            "",
            None,
            None,
            false,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap();
        assert!(matches!(bar.kind, ConditionalKind::DataBar { .. }));

        // Text rules need text.
        assert!(
            build_rule(
                "s",
                RuleTemplate::ContainsText,
                "  ",
                "",
                None,
                Some(hsla(0.1)),
                false,
                hsla(0.3),
                hsla(0.0),
                hsla(0.6),
            )
            .is_err()
        );
    }

    #[test]
    fn rule_summaries_read_naturally() {
        let rule = build_rule(
            "amount",
            RuleTemplate::GreaterThan,
            "100",
            "",
            None,
            Some(hsla(0.1)),
            true,
            hsla(0.3),
            hsla(0.0),
            hsla(0.6),
        )
        .unwrap();
        let summary = rule_summary(&rule);
        assert!(summary.contains("> 100"), "{summary}");
        assert!(summary.contains("background + bold"), "{summary}");
        let scale = ConditionalRule {
            column: "n".into(),
            kind: ConditionalKind::ColorScale {
                min: RgbaColor::new(0.0, 0.0, 0.0, 1.0),
                max: RgbaColor::new(1.0, 1.0, 1.0, 1.0),
            },
        };
        assert!(rule_summary(&scale).contains("Color scale"));
    }

    #[test]
    fn rules_file_round_trips_and_tolerates_garbage() {
        let rule = ConditionalRule {
            column: "amount".into(),
            kind: ConditionalKind::DataBar {
                color: RgbaColor::new(0.2, 0.4, 0.9, 1.0),
            },
        };
        let file = RulesFile {
            version: 1,
            rules: vec![rule],
        };
        let text = serde_json::to_string(&file).unwrap();
        let back: RulesFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.rules.len(), 1);
        assert_eq!(back.rules[0].column, "amount");
        let empty: RulesFile = serde_json::from_str("{}").unwrap();
        assert!(empty.rules.is_empty());
    }
}
