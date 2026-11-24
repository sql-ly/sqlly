//! One parameter's input, shared by parameter cells and the parameters
//! rail: the control that fits its type, inline validation, the state of
//! query-filled options, a presets menu and an edit button. The widget only
//! reports what the person did; the notebook parses, validates and stores
//! the value and pushes it back with [`ParamWidget::sync`].

use gpui::{
    AnyElement, AppContext as _, Context, ElementId, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, prelude::FluentBuilder as _, px,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::calendar::Date as PickerDate;
use gpui_component::checkbox::Checkbox;
use gpui_component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_component::input::{
    Input, InputEvent, InputState, NumberInput, NumberInputEvent, StepAction,
};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::select::{Select, SelectEvent, SelectState};
use gpui_component::slider::{Slider, SliderEvent, SliderState};
use gpui_component::switch::Switch;
use gpui_component::time_field::TimePrecision;
use gpui_component::{Icon, IndexPath, Sizable as _};
use serde_json::Value;
use sqlly_services::notebook::cells::parameter::{MASK, OptionItem, display_value, value_text};
use sqlly_services::notebook::params::{DatePreset, ParamType, ParamValue, ParameterDef};

use crate::icon_catalog::AppIcon;
use crate::theme::Theme;

/// Suggestions and lookup results shown at once.
const MAX_SUGGESTIONS: usize = 20;
/// Options above which a multi-select list gets a filter box.
const FILTER_ABOVE: usize = 12;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

/// Where a widget is shown; one widget per parameter per place.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    /// A parameter cell, by cell id.
    Cell(String),
    Rail,
    /// A dashboard's filter bar.
    Dashboard,
}

/// What the person did to the value.
#[derive(Clone, Debug, PartialEq)]
pub enum Edit {
    /// Typed text, parsed for the parameter's type.
    Text(String),
    Value(ParamValue),
    /// Picked options' values (one, or several for a multi-select).
    Choice(Vec<Value>),
    Clear,
}

/// A pick from the presets menu.
#[derive(Clone, Debug, PartialEq)]
pub enum PresetPick {
    Date(DatePreset),
    /// A named preset declared on the parameter.
    Named(String),
    ApplySet {
        id: String,
        personal: bool,
    },
    SaveSet {
        name: String,
        personal: bool,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamWidgetEvent {
    Edited {
        edit: Edit,
        debounce: bool,
    },
    Preset(PresetPick),
    RefreshOptions,
    /// The lookup text changed.
    Search(String),
    EditDefinition,
    /// Enter in the widget's text box: take the typed value now.
    Submit,
    AddSecret {
        name: String,
        value: String,
    },
}

/// How query-filled options stand.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum OptionsState {
    #[default]
    Static,
    Loading,
    Loaded {
        truncated: bool,
    },
    Failed(String),
}

/// A preset or set the menu offers, with its display label.
#[derive(Clone, Debug, PartialEq)]
pub struct MenuEntry {
    pub pick: PresetPick,
    pub label: String,
}

/// What the notebook pushes into a widget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WidgetStatus {
    pub value: Option<ParamValue>,
    pub error: Option<String>,
    /// The value waits for Apply.
    pub pending: bool,
    pub options: Vec<OptionItem>,
    pub options_state: OptionsState,
    pub has_query: bool,
    /// The query filters by the lookup text itself.
    pub searches: bool,
    pub presets: Vec<MenuEntry>,
    pub sets: Vec<MenuEntry>,
}

enum Control {
    Text(Entity<InputState>),
    Number(Entity<InputState>),
    Toggle,
    Date(Entity<DatePickerState>),
    Choice(Entity<SelectState<Vec<String>>>),
    Multi,
    Slider(Entity<SliderState>),
    Lookup(Entity<InputState>),
    Unsupported,
}

pub struct ParamWidget {
    def: ParameterDef,
    control: Control,
    status: WidgetStatus,
    /// Text last pushed into a text control, to tell echoes from typing.
    shown_text: String,
    /// The value must be pushed into the control on the next render.
    needs_push: bool,
    /// Option labels last pushed into a select.
    shown_labels: Vec<String>,
    /// A sensitive non-text value is shown while being changed.
    revealed: bool,
    lookup_text: String,
    filter: Option<Entity<InputState>>,
    filter_text: String,
    set_name: Option<(bool, Entity<InputState>)>,
    secret_form: Option<(Entity<InputState>, Entity<InputState>)>,
    /// Drawn in a dashboard filter bar: no type line, definition editor or
    /// set saving.
    compact: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ParamWidgetEvent> for ParamWidget {}

fn is_text_type(ty: &ParamType) -> bool {
    matches!(ty, ParamType::String | ParamType::Search)
}

/// The text a value shows in a text control.
pub fn edit_text(value: &ParamValue) -> String {
    match value {
        ParamValue::Null => String::new(),
        other => display_value(other, None).unwrap_or_default(),
    }
}

/// A slider position as parameter text, with the step's decimals.
pub fn slider_text(value: f32, step: f64) -> String {
    let decimals = if step.fract() == 0.0 {
        0
    } else {
        let text = format!("{step}");
        text.split_once('.').map_or(0, |(_, f)| f.len())
    };
    let snapped =
        (f64::from(value) * 10f64.powi(decimals as i32)).round() / 10f64.powi(decimals as i32);
    format!("{snapped:.*}", decimals)
}

fn text_input(
    window: &mut Window,
    cx: &mut Context<ParamWidget>,
    placeholder: &'static str,
    masked: bool,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .placeholder(crate::localization::input_placeholder(
                window,
                cx,
                placeholder,
            ))
            .masked(masked)
    })
}

impl ParamWidget {
    pub fn new(def: ParameterDef, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut subs = Vec::new();
        let control = match &def.ty {
            ParamType::String | ParamType::Search => {
                let input = text_input(
                    window,
                    cx,
                    "notebook.params.widget.text_placeholder",
                    def.sensitive,
                );
                subs.push(cx.subscribe(
                    &input,
                    |this, input, event: &InputEvent, cx| match event {
                        InputEvent::Change => {
                            let text = input.read(cx).value().to_string();
                            this.typed(text, cx);
                        }
                        InputEvent::PressEnter { .. } => cx.emit(ParamWidgetEvent::Submit),
                        _ => {}
                    },
                ));
                Control::Text(input)
            }
            ParamType::Integer | ParamType::Decimal => {
                let input = text_input(
                    window,
                    cx,
                    "notebook.params.widget.number_placeholder",
                    def.sensitive,
                );
                subs.push(cx.subscribe(
                    &input,
                    |this, input, event: &InputEvent, cx| match event {
                        InputEvent::Change => {
                            let text = input.read(cx).value().to_string();
                            this.typed(text, cx);
                        }
                        InputEvent::PressEnter { .. } => cx.emit(ParamWidgetEvent::Submit),
                        _ => {}
                    },
                ));
                subs.push(cx.subscribe_in(
                    &input,
                    window,
                    |this, input, event: &NumberInputEvent, window, cx| {
                        let NumberInputEvent::Step(action) = event;
                        let text = input.read(cx).value().to_string();
                        let next = this.stepped(&text, *action);
                        input.update(cx, |s, cx| s.set_value(next.clone(), window, cx));
                        this.typed(next, cx);
                    },
                ));
                Control::Number(input)
            }
            ParamType::Boolean => Control::Toggle,
            ParamType::Date | ParamType::Datetime { .. } | ParamType::DateRange => {
                let ty = def.ty.clone();
                let picker = cx.new(|cx| match ty {
                    ParamType::DateRange => DatePickerState::range(window, cx),
                    ParamType::Datetime { .. } => {
                        DatePickerState::new(window, cx).time_precision(TimePrecision::Minute)
                    }
                    _ => DatePickerState::new(window, cx),
                });
                subs.push(
                    cx.subscribe(&picker, |this, _, event: &DatePickerEvent, cx| {
                        let DatePickerEvent::Change(value) = event;
                        let edit = match (&this.def.ty, value.date()) {
                            (ParamType::DateRange, PickerDate::Range(Some(start), Some(end))) => {
                                Some(Edit::Value(ParamValue::DateRange { start, end }))
                            }
                            (ParamType::DateRange, PickerDate::Range(None, None)) => {
                                Some(Edit::Clear)
                            }
                            (ParamType::DateRange, _) => None,
                            (ParamType::Datetime { .. }, _) => Some(match value.start() {
                                Some(dt) => Edit::Value(ParamValue::DateTime(dt)),
                                None => Edit::Clear,
                            }),
                            (_, PickerDate::Single(Some(day))) => {
                                Some(Edit::Value(ParamValue::Date(day)))
                            }
                            _ => Some(Edit::Clear),
                        };
                        if let Some(edit) = edit {
                            let debounce = matches!(this.def.ty, ParamType::Datetime { .. });
                            this.emit_edit(edit, debounce, cx);
                        }
                    }),
                );
                Control::Date(picker)
            }
            ParamType::Enum | ParamType::Identifier { .. } | ParamType::SecretRef => {
                let select = cx.new(|cx| {
                    SelectState::new(Vec::<String>::new(), None, window, cx).searchable(true)
                });
                subs.push(cx.subscribe(
                    &select,
                    |this, select, _: &SelectEvent<Vec<String>>, cx| {
                        let Some(ix) = select.read(cx).selected_index(cx) else {
                            return;
                        };
                        if let Some(item) = this.status.options.get(ix.row) {
                            let value = item.value.clone();
                            this.emit_edit(Edit::Choice(vec![value]), false, cx);
                        }
                    },
                ));
                Control::Choice(select)
            }
            ParamType::MultiSelect => Control::Multi,
            ParamType::Slider { min, max, step } => {
                let (min, max, step) = (*min as f32, *max as f32, *step as f32);
                let slider = cx.new(|_| {
                    SliderState::new()
                        .min(min)
                        .max(max)
                        .step(step)
                        .default_value(min)
                });
                subs.push(cx.subscribe(&slider, |this, _, event: &SliderEvent, cx| {
                    let (value, debounce) = match event {
                        SliderEvent::Change(v) => (v.start(), true),
                        SliderEvent::Release(v) => (v.start(), false),
                    };
                    let step = match this.def.ty {
                        ParamType::Slider { step, .. } => step,
                        _ => 1.0,
                    };
                    this.emit_edit(Edit::Text(slider_text(value, step)), debounce, cx);
                }));
                Control::Slider(slider)
            }
            ParamType::EntityLookup => {
                let input = text_input(
                    window,
                    cx,
                    "notebook.params.widget.lookup_placeholder",
                    false,
                );
                subs.push(cx.subscribe(&input, |this, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        let text = input.read(cx).value().to_string();
                        if text != this.lookup_text {
                            this.lookup_text = text.clone();
                            cx.emit(ParamWidgetEvent::Search(text));
                            cx.notify();
                        }
                    }
                }));
                Control::Lookup(input)
            }
            ParamType::Unknown(_) => Control::Unsupported,
        };
        Self {
            def,
            control,
            status: WidgetStatus::default(),
            shown_text: String::new(),
            needs_push: true,
            shown_labels: Vec::new(),
            revealed: false,
            lookup_text: String::new(),
            filter: None,
            filter_text: String::new(),
            set_name: None,
            secret_form: None,
            compact: false,
            _subscriptions: subs,
        }
    }

    /// The filter-bar form of the widget.
    pub fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    pub fn def(&self) -> &ParameterDef {
        &self.def
    }

    /// The text box of a text, number or lookup control.
    #[cfg(test)]
    pub fn input(&self) -> Option<Entity<InputState>> {
        match &self.control {
            Control::Text(input) | Control::Number(input) | Control::Lookup(input) => {
                Some(input.clone())
            }
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn is_masked(&self) -> bool {
        self.masked_now()
    }

    #[cfg(test)]
    pub fn status(&self) -> &WidgetStatus {
        &self.status
    }

    /// Push the notebook's view of this parameter. `external` means the
    /// value changed somewhere else (a preset, a set, the other surface, a
    /// linked reset) and replaces what the control shows.
    pub fn sync(&mut self, status: WidgetStatus, external: bool, cx: &mut Context<Self>) {
        if external {
            self.needs_push = true;
        }
        if self.status != status {
            self.status = status;
            cx.notify();
        } else if external {
            cx.notify();
        }
    }

    fn typed(&mut self, text: String, cx: &mut Context<Self>) {
        if text == self.shown_text {
            return;
        }
        self.shown_text = text.clone();
        let edit = if text.trim().is_empty() {
            Edit::Clear
        } else {
            Edit::Text(text)
        };
        self.emit_edit(edit, true, cx);
    }

    fn stepped(&self, text: &str, action: StepAction) -> String {
        let current = text.trim().parse::<f64>().unwrap_or(0.0);
        let next = match action {
            StepAction::Increment => current + 1.0,
            StepAction::Decrement => current - 1.0,
        };
        let bound = |v: Option<&Value>| v.and_then(Value::as_f64);
        let next = match (
            bound(self.def.validation.min.as_ref()),
            bound(self.def.validation.max.as_ref()),
        ) {
            (Some(min), _) if next < min => min,
            (_, Some(max)) if next > max => max,
            _ => next,
        };
        match self.def.ty {
            ParamType::Integer => format!("{}", next.round() as i64),
            _ => {
                let decimals = text.split_once('.').map_or(0, |(_, f)| f.trim().len());
                format!("{next:.*}", decimals)
            }
        }
    }

    fn emit_edit(&mut self, edit: Edit, debounce: bool, cx: &mut Context<Self>) {
        if !is_text_type(&self.def.ty) {
            self.revealed = false;
        }
        cx.emit(ParamWidgetEvent::Edited { edit, debounce });
    }

    /// Act as if the person picked option `index` (or toggled it, in a
    /// multi-select).
    pub fn pick(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(item) = self.status.options.get(index).cloned() else {
            return;
        };
        if self.def.ty == ParamType::MultiSelect {
            let mut chosen = self.chosen_values();
            match chosen.iter().position(|v| *v == item.value) {
                Some(at) => {
                    chosen.remove(at);
                }
                None => chosen.push(item.value.clone()),
            }
            let order: Vec<&Value> = self.status.options.iter().map(|o| &o.value).collect();
            chosen.sort_by_key(|v| order.iter().position(|o| *o == v));
            self.emit_edit(Edit::Choice(chosen), false, cx);
        } else {
            self.lookup_text.clear();
            self.emit_edit(Edit::Choice(vec![item.value]), false, cx);
        }
    }

    /// Act as if the person set the switch.
    pub fn toggle(&mut self, on: bool, cx: &mut Context<Self>) {
        self.emit_edit(Edit::Value(ParamValue::Boolean(on)), false, cx);
    }

    fn chosen_values(&self) -> Vec<Value> {
        match &self.status.value {
            Some(ParamValue::List(items)) => items.iter().map(ParamValue::to_json).collect(),
            Some(ParamValue::Null) | None => Vec::new(),
            Some(single) => vec![single.to_json()],
        }
    }

    /// The option index showing the current value.
    fn selected_option(&self) -> Option<usize> {
        let value = self.status.value.as_ref()?.to_json();
        let text = value_text(&value);
        self.status
            .options
            .iter()
            .position(|o| o.value == value)
            .or_else(|| {
                self.status
                    .options
                    .iter()
                    .position(|o| value_text(&o.value) == text)
            })
    }

    fn masked_now(&self) -> bool {
        self.def.sensitive
            && !is_text_type(&self.def.ty)
            && !self.revealed
            && self
                .status
                .value
                .as_ref()
                .is_some_and(|v| *v != ParamValue::Null)
    }

    /// Bring the control in line with the pushed value.
    fn push_value(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let labels: Vec<String> = self
            .status
            .options
            .iter()
            .map(|o| o.label.clone())
            .collect();
        if let Control::Choice(select) = &self.control
            && labels != self.shown_labels
        {
            self.shown_labels = labels.clone();
            self.needs_push = true;
            let selected = self.selected_option().map(IndexPath::new);
            select.update(cx, |s, cx| {
                s.set_items(labels, window, cx);
                s.set_selected_index(selected, window, cx);
            });
        }
        if !std::mem::take(&mut self.needs_push) {
            return;
        }
        let value = self.status.value.clone();
        match &self.control {
            Control::Text(input) | Control::Number(input) => {
                let text = value.as_ref().map(edit_text).unwrap_or_default();
                if input.read(cx).value() != text {
                    self.shown_text = text.clone();
                    input.update(cx, |s, cx| s.set_value(text, window, cx));
                }
            }
            Control::Date(picker) => {
                let date = match &value {
                    Some(ParamValue::Date(d)) => Some(PickerDate::Single(Some(*d))),
                    Some(ParamValue::DateRange { start, end }) => {
                        Some(PickerDate::Range(Some(*start), Some(*end)))
                    }
                    Some(ParamValue::DateTime(dt)) => {
                        let dt = *dt;
                        picker.update(cx, |s, cx| s.set_date_time(dt, window, cx));
                        None
                    }
                    _ => Some(match self.def.ty {
                        ParamType::DateRange => PickerDate::Range(None, None),
                        _ => PickerDate::Single(None),
                    }),
                };
                if let Some(date) = date {
                    picker.update(cx, |s, cx| s.set_date(date, window, cx));
                }
            }
            Control::Choice(select) => {
                let selected = self.selected_option().map(IndexPath::new);
                select.update(cx, |s, cx| s.set_selected_index(selected, window, cx));
            }
            Control::Slider(slider) => {
                let n = match &value {
                    Some(ParamValue::Integer(n)) => Some(*n as f32),
                    Some(ParamValue::Decimal(s)) => s.parse::<f32>().ok(),
                    _ => None,
                };
                if let Some(n) = n {
                    slider.update(cx, |s, cx| s.set_value(n, window, cx));
                }
            }
            Control::Toggle | Control::Multi | Control::Lookup(_) | Control::Unsupported => {}
        }
    }

    fn id(&self, part: &str, cx: &Context<Self>) -> ElementId {
        ElementId::Name(format!("nbp-{part}-{}", cx.entity_id()).into())
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let edit = t("notebook.params.widget.edit_definition");
        let s = Theme::scaled;
        let label = if self.def.label.is_empty() {
            self.def.name.clone()
        } else {
            self.def.label.clone()
        };
        let kind = crate::localization::text(super::editor::type_label_key(match &self.def.ty {
            ParamType::Unknown(_) => "unknown",
            other => other.kind(),
        }));
        let entries: Vec<MenuEntry> = self
            .status
            .presets
            .iter()
            .chain(&self.status.sets)
            .cloned()
            .collect();
        let has_presets = !entries.is_empty();
        let has_sets = !self.status.sets.is_empty();
        let compact = self.compact;
        let this = cx.entity();
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(s(6.0)))
            .child(
                div()
                    .truncate()
                    .text_size(px(Theme::font_size_ratio(0.9)))
                    .text_color(Theme::text())
                    .child(label),
            )
            .when(!self.compact, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.75)))
                        .text_color(Theme::text_dim())
                        .child(crate::localization::format(
                            "notebook.params.widget.name_and_type",
                            &[("name", self.def.name.clone()), ("kind", kind)],
                        )),
                )
            })
            .when(self.def.sensitive, |d| {
                d.child(
                    Icon::empty()
                        .path(AppIcon::Lock.path())
                        .text_color(Theme::text_dim()),
                )
            })
            .when(self.status.pending, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.75)))
                        .text_color(Theme::warning())
                        .child(t("notebook.params.widget.pending")),
                )
            })
            .child(div().flex_1())
            .when(!self.compact || has_presets, |d| {
                d.child({
                    let menu_this = this.clone();
                    let personal_this = this.clone();
                    let shared_this = this.clone();
                    Button::new(self.id("presets", cx))
                        .ghost()
                        .xsmall()
                        .label(t("notebook.params.presets.menu"))
                        .icon(Icon::empty().path(AppIcon::ChevronDown.path()))
                        .accessibility_label(t("notebook.params.presets.menu_accessibility"))
                        .dropdown_menu(move |mut menu, _, _| {
                            for entry in entries.clone() {
                                let this = menu_this.clone();
                                menu = menu.item(PopupMenuItem::new(entry.label.clone()).on_click(
                                    move |_, _, app| {
                                        let pick = entry.pick.clone();
                                        this.update(app, |_, cx| {
                                            cx.emit(ParamWidgetEvent::Preset(pick))
                                        });
                                    },
                                ));
                            }
                            if compact {
                                return menu;
                            }
                            if has_presets || has_sets {
                                menu = menu.separator();
                            }
                            let shared = shared_this.clone();
                            let personal = personal_this.clone();
                            menu.item(
                                PopupMenuItem::new(t("notebook.params.presets.save_shared"))
                                    .on_click(move |_, window, app| {
                                        shared
                                            .update(app, |w, cx| w.ask_set_name(false, window, cx))
                                    }),
                            )
                            .item(
                                PopupMenuItem::new(t("notebook.params.presets.save_personal"))
                                    .on_click(move |_, window, app| {
                                        personal
                                            .update(app, |w, cx| w.ask_set_name(true, window, cx))
                                    }),
                            )
                        })
                })
            })
            .when(self.status.has_query, |d| {
                let refresh = t("notebook.params.options.refresh");
                d.child(
                    Button::new(self.id("refresh", cx))
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path(AppIcon::RefreshSchema.path()))
                        .accessibility_label(refresh.clone())
                        .tooltip(refresh)
                        .on_click(
                            cx.listener(|_, _, _, cx| cx.emit(ParamWidgetEvent::RefreshOptions)),
                        ),
                )
            })
            .when(!self.compact, |d| {
                d.child(
                    Button::new(self.id("edit", cx))
                        .ghost()
                        .xsmall()
                        .icon(Icon::empty().path(AppIcon::Settings.path()))
                        .accessibility_label(edit.clone())
                        .tooltip(edit)
                        .on_click(
                            cx.listener(|_, _, _, cx| cx.emit(ParamWidgetEvent::EditDefinition)),
                        ),
                )
            })
            .into_any_element()
    }

    fn ask_set_name(&mut self, personal: bool, window: &mut Window, cx: &mut Context<Self>) {
        let input = text_input(
            window,
            cx,
            "notebook.params.presets.set_name_placeholder",
            false,
        );
        let sub = cx.subscribe(&input, move |this, input, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                let name = input.read(cx).value().trim().to_string();
                this.save_set(name, personal, cx);
            }
        });
        self._subscriptions.push(sub);
        input.update(cx, |s, cx| s.focus(window, cx));
        self.set_name = Some((personal, input));
        cx.notify();
    }

    /// Act as if the person named a new set and saved it.
    pub fn save_set(&mut self, name: String, personal: bool, cx: &mut Context<Self>) {
        self.set_name = None;
        if !name.is_empty() {
            cx.emit(ParamWidgetEvent::Preset(PresetPick::SaveSet {
                name,
                personal,
            }));
        }
        cx.notify();
    }

    fn render_control(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        if self.masked_now() {
            return div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .child(div().text_color(Theme::text()).child(MASK))
                .child(
                    Button::new(self.id("reveal", cx))
                        .ghost()
                        .xsmall()
                        .label(t("notebook.params.widget.change_sensitive"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.revealed = true;
                            cx.notify();
                        })),
                )
                .into_any_element();
        }
        match &self.control {
            Control::Text(input) => {
                let input = input.clone();
                let mut col = div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(Input::new(&input).small());
                if self.def.ty == ParamType::Search && !self.def.sensitive {
                    let typed = self.shown_text.to_lowercase();
                    let picks: Vec<(usize, String)> = self
                        .status
                        .options
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| {
                            !typed.is_empty()
                                && o.label.to_lowercase().contains(&typed)
                                && o.label.to_lowercase() != typed
                        })
                        .take(MAX_SUGGESTIONS.min(8))
                        .map(|(i, o)| (i, o.label.clone()))
                        .collect();
                    col = col.children(
                        picks
                            .into_iter()
                            .map(|(i, label)| self.option_row("suggest", i, label, false, cx)),
                    );
                }
                col.into_any_element()
            }
            Control::Number(input) => NumberInput::new(input).small().into_any_element(),
            Control::Toggle => {
                let on = matches!(self.status.value, Some(ParamValue::Boolean(true)));
                let this = cx.entity();
                Switch::new(self.id("switch", cx))
                    .checked(on)
                    .accessibility_label(self.def.name.clone())
                    .on_click(move |checked, _, app| {
                        let checked = *checked;
                        this.update(app, |w, cx| w.toggle(checked, cx));
                    })
                    .into_any_element()
            }
            Control::Date(picker) => DatePicker::new(picker)
                .small()
                .cleanable(true)
                .placeholder(t("notebook.params.widget.date_placeholder"))
                .into_any_element(),
            Control::Choice(select) => {
                let select = select.clone();
                let mut row = div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(6.0)))
                    .child(
                        div().flex_1().child(
                            Select::new(&select)
                                .small()
                                .placeholder(t("notebook.params.widget.choose_placeholder")),
                        ),
                    );
                if self.def.ty == ParamType::SecretRef {
                    row =
                        row.child(
                            Button::new(self.id("add-secret", cx))
                                .ghost()
                                .xsmall()
                                .label(t("notebook.params.secret.add"))
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_secret_form(window, cx)
                                })),
                        );
                }
                let mut col = div().flex().flex_col().gap(px(s(4.0))).child(row);
                if let Some((name, value)) = &self.secret_form {
                    col = col.child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(4.0)))
                            .child(div().flex_1().child(Input::new(name).small()))
                            .child(div().flex_1().child(Input::new(value).small()))
                            .child(
                                Button::new(self.id("save-secret", cx))
                                    .xsmall()
                                    .label(t("notebook.params.secret.save"))
                                    .on_click(cx.listener(|this, _, _, cx| this.submit_secret(cx))),
                            ),
                    );
                }
                col.into_any_element()
            }
            Control::Multi => self.render_multi(window, cx),
            Control::Slider(slider) => {
                let shown = self
                    .status
                    .value
                    .as_ref()
                    .map(edit_text)
                    .unwrap_or_default();
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(s(8.0)))
                    .child(
                        div()
                            .flex_1()
                            .child(Slider::new(slider).accessibility_label(self.def.name.clone())),
                    )
                    .child(
                        div()
                            .min_w(px(s(36.0)))
                            .text_color(Theme::text())
                            .child(shown),
                    )
                    .into_any_element()
            }
            Control::Lookup(input) => {
                let input = input.clone();
                let selected = self
                    .selected_option()
                    .and_then(|i| self.status.options.get(i))
                    .map(|o| {
                        crate::localization::format(
                            "notebook.params.widget.lookup_selected",
                            &[("label", o.label.clone()), ("value", value_text(&o.value))],
                        )
                    })
                    .or_else(|| {
                        self.status
                            .value
                            .as_ref()
                            .filter(|v| **v != ParamValue::Null)
                            .map(edit_text)
                    });
                let typed = self.lookup_text.to_lowercase();
                let server_side = self.status.searches;
                let results: Vec<(usize, String)> = if typed.is_empty() {
                    Vec::new()
                } else {
                    self.status
                        .options
                        .iter()
                        .enumerate()
                        .filter(|(_, o)| {
                            server_side
                                || o.label.to_lowercase().contains(&typed)
                                || value_text(&o.value).to_lowercase().contains(&typed)
                        })
                        .take(MAX_SUGGESTIONS)
                        .map(|(i, o)| (i, format!("{} · {}", o.label, value_text(&o.value))))
                        .collect()
                };
                let empty = !typed.is_empty() && results.is_empty();
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .child(Input::new(&input).small())
                    .when_some(selected, |d, text| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(text),
                        )
                    })
                    .when(!results.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .rounded(px(s(4.0)))
                                .border_1()
                                .border_color(Theme::popup_border())
                                .bg(Theme::popup_bg())
                                .children(results.into_iter().map(|(i, label)| {
                                    self.option_row("lookup", i, label, false, cx)
                                })),
                        )
                    })
                    .when(empty, |d| {
                        d.child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(t("notebook.params.widget.lookup_no_results")),
                        )
                    })
                    .into_any_element()
            }
            Control::Unsupported => div()
                .text_size(px(Theme::font_size_ratio(0.85)))
                .text_color(Theme::text_dim())
                .child(t("notebook.params.widget.unsupported"))
                .into_any_element(),
        }
    }

    fn option_row(
        &self,
        part: &str,
        index: usize,
        label: String,
        checked: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let s = Theme::scaled;
        div()
            .id(ElementId::Name(
                format!("nbp-{part}-{}-{index}", cx.entity_id()).into(),
            ))
            .px(px(s(6.0)))
            .py(px(s(2.0)))
            .cursor_pointer()
            .truncate()
            .text_size(px(Theme::font_size_ratio(0.85)))
            .text_color(Theme::text())
            .when(checked, |d| d.bg(Theme::selection_fill()))
            .hover(|style| style.bg(Theme::row_hover()))
            .on_click(cx.listener(move |this, _, _, cx| this.pick(index, cx)))
            .child(label)
            .into_any_element()
    }

    fn render_multi(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = Theme::scaled;
        if self.status.options.len() > FILTER_ABOVE && self.filter.is_none() {
            let input = text_input(
                window,
                cx,
                "notebook.params.widget.filter_placeholder",
                false,
            );
            self._subscriptions.push(cx.subscribe(
                &input,
                |this, input, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.filter_text = input.read(cx).value().to_lowercase();
                        cx.notify();
                    }
                },
            ));
            self.filter = Some(input);
        }
        let chosen = self.chosen_values();
        let this = cx.entity();
        let rows: Vec<AnyElement> = self
            .status
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| {
                self.filter_text.is_empty() || o.label.to_lowercase().contains(&self.filter_text)
            })
            .map(|(i, o)| {
                let this = this.clone();
                Checkbox::new(ElementId::Name(
                    format!("nbp-multi-{}-{i}", cx.entity_id()).into(),
                ))
                .label(o.label.clone())
                .checked(chosen.contains(&o.value))
                .on_click(move |_, _, app| this.update(app, |w, cx| w.pick(i, cx)))
                .into_any_element()
            })
            .collect();
        let empty = rows.is_empty();
        div()
            .flex()
            .flex_col()
            .gap(px(s(3.0)))
            .when_some(self.filter.clone(), |d, input| {
                d.child(Input::new(&input).small())
            })
            .child(
                div()
                    .id(self.id("multi-list", cx))
                    .flex()
                    .flex_col()
                    .gap(px(s(2.0)))
                    .max_h(px(s(160.0)))
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when(empty, |d| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::text_dim())
                        .child(t("notebook.params.widget.no_options")),
                )
            })
            .into_any_element()
    }

    fn open_secret_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.secret_form.is_some() {
            self.secret_form = None;
        } else {
            let name = text_input(window, cx, "notebook.params.secret.name_placeholder", false);
            let value = text_input(window, cx, "notebook.params.secret.value_placeholder", true);
            self.secret_form = Some((name, value));
        }
        cx.notify();
    }

    fn submit_secret(&mut self, cx: &mut Context<Self>) {
        let Some((name, value)) = self.secret_form.take() else {
            return;
        };
        let name = name.read(cx).value().trim().to_string();
        let value = value.read(cx).value().to_string();
        if !name.is_empty() && !value.is_empty() {
            cx.emit(ParamWidgetEvent::AddSecret { name, value });
        }
        cx.notify();
    }

    fn render_status_line(&self) -> Option<AnyElement> {
        let (text, color) = if let Some(error) = &self.status.error {
            (error.clone(), Theme::error())
        } else {
            match &self.status.options_state {
                OptionsState::Static => return None,
                OptionsState::Loading => (t("notebook.params.options.loading"), Theme::text_dim()),
                OptionsState::Loaded { truncated: true } => (
                    crate::localization::format(
                        "notebook.params.options.truncated",
                        &[("count", self.status.options.len())],
                    ),
                    Theme::warning(),
                ),
                OptionsState::Loaded { truncated: false } => return None,
                OptionsState::Failed(message) => (message.clone(), Theme::error()),
            }
        };
        Some(
            div()
                .text_size(px(Theme::font_size_ratio(0.8)))
                .text_color(color)
                .child(text)
                .into_any_element(),
        )
    }
}

impl Render for ParamWidget {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.push_value(window, cx);
        let s = Theme::scaled;
        let header = self.render_header(cx);
        let control = self.render_control(window, cx);
        let name_row = self.set_name.as_ref().map(|(personal, input)| {
            let key = if *personal {
                "notebook.params.presets.set_name_personal"
            } else {
                "notebook.params.presets.set_name_shared"
            };
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(s(6.0)))
                .child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::text_dim())
                        .child(t(key)),
                )
                .child(div().flex_1().child(Input::new(input).small()))
        });
        div()
            .id(self.id("root", cx))
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .w_full()
            .child(header)
            .child(control)
            .children(self.render_status_line())
            .children(name_row)
    }
}
