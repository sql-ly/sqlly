//! The parameter definition editor: name, label, type and its settings,
//! default, allowed values (a list or a read-only discovery query),
//! validation, sensitivity, sharing, scope, reactivity and presets. The form
//! is a [`Draft`] of plain text and flags; [`Draft::build`] turns it into a
//! definition or names the field that is wrong.

use gpui::{
    AnyElement, AppContext as _, Context, ElementId, Entity, EventEmitter, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
    div, prelude::FluentBuilder as _, px,
};
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use gpui_component::input::{Input, InputState, Textarea, TextareaState};
use gpui_component::select::{Select, SelectEvent, SelectState};
use serde_json::Value;
use sqlly_intel::natural_date::{DateParseOptions, parse_natural_date};
use sqlly_services::notebook::cells::parameter::{OptionsQuery, takes_options};
use sqlly_services::notebook::params::{
    self, AllowedValues, Clock, DatePreset, NamedPreset, ParamScope, ParamType, ParamValue,
    ParameterDef, Preset, Reactivity, Validation,
};

use crate::theme::Theme;

fn t(key: &str) -> String {
    crate::localization::text(key)
}

/// Every type, in the order the type picker lists them.
pub const KINDS: [&str; 14] = [
    "string",
    "integer",
    "decimal",
    "boolean",
    "date",
    "datetime",
    "date_range",
    "enum",
    "multi_select",
    "search",
    "entity_lookup",
    "slider",
    "secret_ref",
    "identifier",
];

/// Every built-in date preset, in menu order.
pub const DATE_PRESETS: [DatePreset; 14] = [
    DatePreset::Today,
    DatePreset::Yesterday,
    DatePreset::Last7Days,
    DatePreset::Last30Days,
    DatePreset::Last90Days,
    DatePreset::WeekToDate,
    DatePreset::PreviousWeek,
    DatePreset::MonthToDate,
    DatePreset::PreviousMonth,
    DatePreset::CurrentQuarter,
    DatePreset::QuarterToDate,
    DatePreset::PreviousQuarter,
    DatePreset::YearToDate,
    DatePreset::PreviousYear,
];

pub fn date_preset_id(preset: DatePreset) -> String {
    serde_json::to_value(preset)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The translation key of a date preset's label.
pub fn date_preset_label_key(preset: DatePreset) -> &'static str {
    match preset {
        DatePreset::Today => "notebook.params.presets.date.today",
        DatePreset::Yesterday => "notebook.params.presets.date.yesterday",
        DatePreset::Last7Days => "notebook.params.presets.date.last_7_days",
        DatePreset::Last30Days => "notebook.params.presets.date.last_30_days",
        DatePreset::Last90Days => "notebook.params.presets.date.last_90_days",
        DatePreset::WeekToDate => "notebook.params.presets.date.week_to_date",
        DatePreset::PreviousWeek => "notebook.params.presets.date.previous_week",
        DatePreset::MonthToDate => "notebook.params.presets.date.month_to_date",
        DatePreset::PreviousMonth => "notebook.params.presets.date.previous_month",
        DatePreset::CurrentQuarter => "notebook.params.presets.date.current_quarter",
        DatePreset::QuarterToDate => "notebook.params.presets.date.quarter_to_date",
        DatePreset::PreviousQuarter => "notebook.params.presets.date.previous_quarter",
        DatePreset::YearToDate => "notebook.params.presets.date.year_to_date",
        DatePreset::PreviousYear => "notebook.params.presets.date.previous_year",
    }
}

/// The translation key of a parameter type's name, from its `kind()`.
pub fn type_label_key(kind: &str) -> &'static str {
    match kind {
        "string" => "notebook.params.type.string",
        "integer" => "notebook.params.type.integer",
        "decimal" => "notebook.params.type.decimal",
        "boolean" => "notebook.params.type.boolean",
        "date" => "notebook.params.type.date",
        "datetime" => "notebook.params.type.datetime",
        "date_range" => "notebook.params.type.date_range",
        "enum" => "notebook.params.type.enum",
        "multi_select" => "notebook.params.type.multi_select",
        "search" => "notebook.params.type.search",
        "entity_lookup" => "notebook.params.type.entity_lookup",
        "slider" => "notebook.params.type.slider",
        "secret_ref" => "notebook.params.type.secret_ref",
        "identifier" => "notebook.params.type.identifier",
        _ => "notebook.params.type.unknown",
    }
}

/// Whether `preset` makes sense for `ty` (single dates take only a day).
pub fn preset_fits(ty: &ParamType, preset: DatePreset) -> bool {
    match ty {
        ParamType::DateRange => true,
        ParamType::Date => matches!(preset, DatePreset::Today | DatePreset::Yesterday),
        _ => false,
    }
}

/// The date presets a parameter's menu offers: the ones it lists, else
/// every one that fits its type.
pub fn offered_date_presets(def: &ParameterDef) -> Vec<DatePreset> {
    let listed: Vec<DatePreset> = def
        .presets
        .iter()
        .filter_map(|p| match p {
            Preset::Date(d) => Some(*d),
            Preset::Named(_) | Preset::Unknown(_) => None,
        })
        .filter(|d| preset_fits(&def.ty, *d))
        .collect();
    if listed.is_empty() {
        DATE_PRESETS
            .into_iter()
            .filter(|d| preset_fits(&def.ty, *d))
            .collect()
    } else {
        listed
    }
}

/// Where a parameter's choices come from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Source {
    #[default]
    None,
    List,
    Query,
}

/// The fields of the form, as typed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Draft {
    pub name: String,
    pub label: String,
    pub kind: String,
    pub slider_min: String,
    pub slider_max: String,
    pub slider_step: String,
    pub time_zone: String,
    pub allowlist: String,
    pub default_text: String,
    pub default_preset: Option<DatePreset>,
    pub source: Source,
    pub allowed_list: String,
    pub query_sql: String,
    pub query_slot: Option<String>,
    pub value_column: String,
    pub label_column: String,
    pub limit: String,
    pub required: bool,
    pub min: String,
    pub max: String,
    pub min_length: String,
    pub max_length: String,
    pub sensitive: bool,
    pub shareable: bool,
    pub personal: bool,
    pub apply: bool,
    pub date_presets: Vec<DatePreset>,
    /// `Label = value` entries separated by `;`.
    pub named_presets: String,
}

/// A form field, for pointing at the one that is wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Name,
    Slider,
    Allowlist,
    Default,
    AllowedList,
    Query,
    Limit,
    Validation,
    NamedPresets,
}

fn list_items(text: &str, sep: char) -> Vec<String> {
    text.split(sep)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn scalar(text: &str) -> Value {
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(|v| v.is_number() || v.is_boolean())
        .unwrap_or_else(|| Value::String(text.to_string()))
}

fn number(text: &str) -> Result<Option<Value>, ()> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_number)
        .map(Some)
        .ok_or(())
}

fn count(text: &str) -> Result<Option<usize>, ()> {
    let text = text.trim();
    if text.is_empty() {
        Ok(None)
    } else {
        text.parse::<usize>().map(Some).map_err(|_| ())
    }
}

/// Parse a default typed into the form: the type's own text form, or for
/// dates a phrase such as "last week" or "yesterday".
pub fn parse_default(
    def: &ParameterDef,
    text: &str,
    clock: &dyn Clock,
) -> Result<ParamValue, params::ParamError> {
    match params::parse_text(def, text) {
        Ok(value) => Ok(value),
        Err(error) => {
            if !matches!(
                def.ty,
                ParamType::Date | ParamType::Datetime { .. } | ParamType::DateRange
            ) {
                return Err(error);
            }
            let parsed = parse_natural_date(text, clock.now(), &DateParseOptions::default())
                .map_err(|_| error.clone())?;
            let last = if parsed.end > parsed.start {
                (parsed.end - chrono::Duration::seconds(1)).date()
            } else {
                parsed.start.date()
            };
            Ok(match def.ty {
                ParamType::Date => ParamValue::Date(parsed.start.date()),
                ParamType::DateRange => ParamValue::DateRange {
                    start: parsed.start.date(),
                    end: last.max(parsed.start.date()),
                },
                _ => ParamValue::DateTime(parsed.start),
            })
        }
    }
}

impl Draft {
    /// A form for `def` and its discovery query.
    pub fn from_def(def: &ParameterDef) -> Self {
        let mut d = Draft {
            name: def.name.clone(),
            label: def.label.clone(),
            kind: match &def.ty {
                ParamType::Unknown(_) => "string".to_string(),
                ty => ty.kind().to_string(),
            },
            slider_step: "1".into(),
            required: def.validation.required,
            sensitive: def.sensitive,
            shareable: def.shareable,
            personal: def.scope == ParamScope::Personal,
            apply: def.reactivity.waits_for_apply(),
            ..Draft::default()
        };
        match &def.ty {
            ParamType::Slider { min, max, step } => {
                d.slider_min = min.to_string();
                d.slider_max = max.to_string();
                d.slider_step = step.to_string();
            }
            ParamType::Datetime { tz } => d.time_zone = tz.clone().unwrap_or_default(),
            ParamType::Identifier { allowlist } => d.allowlist = allowlist.join(", "),
            _ => {}
        }
        match &def.default {
            Some(Value::Object(map)) if map.contains_key("preset") => {
                d.default_preset = map["preset"]
                    .as_str()
                    .and_then(|id| serde_json::from_value(Value::String(id.into())).ok());
            }
            Some(Value::Object(map)) => {
                let part = |k: &str| map.get(k).and_then(Value::as_str).unwrap_or_default();
                d.default_text = format!("{}..{}", part("start"), part("end"));
            }
            Some(Value::Array(items)) => {
                d.default_text = items
                    .iter()
                    .map(sqlly_services::notebook::cells::parameter::value_text)
                    .collect::<Vec<_>>()
                    .join(", ");
            }
            Some(other) => {
                d.default_text = sqlly_services::notebook::cells::parameter::value_text(other)
            }
            None => {}
        }
        if let Some(AllowedValues::Static(values)) = &def.allowed_values {
            d.source = Source::List;
            d.allowed_list = values
                .iter()
                .map(|v| match v {
                    Value::Object(map) => {
                        let value = map
                            .get("value")
                            .map(sqlly_services::notebook::cells::parameter::value_text)
                            .unwrap_or_default();
                        match map.get("label").and_then(Value::as_str) {
                            Some(label) if !label.is_empty() => format!("{value}={label}"),
                            _ => value,
                        }
                    }
                    other => sqlly_services::notebook::cells::parameter::value_text(other),
                })
                .collect::<Vec<_>>()
                .join(", ");
        }
        if let Some(q) = OptionsQuery::of(def) {
            d.source = Source::Query;
            d.query_sql = q.sql.clone();
            d.query_slot = q.connection_ref.clone();
            d.value_column = q.value_column.clone().unwrap_or_default();
            d.label_column = q.label_column.clone().unwrap_or_default();
            d.limit = q.limit.map(|n| n.to_string()).unwrap_or_default();
        }
        let text = |v: &Option<Value>| {
            v.as_ref()
                .map(sqlly_services::notebook::cells::parameter::value_text)
                .unwrap_or_default()
        };
        d.min = text(&def.validation.min);
        d.max = text(&def.validation.max);
        d.min_length = def
            .validation
            .min_length
            .map(|n| n.to_string())
            .unwrap_or_default();
        d.max_length = def
            .validation
            .max_length
            .map(|n| n.to_string())
            .unwrap_or_default();
        let mut named = Vec::new();
        for preset in &def.presets {
            match preset {
                Preset::Date(p) => d.date_presets.push(*p),
                Preset::Named(n) => named.push(format!(
                    "{} = {}",
                    if n.label.is_empty() { &n.id } else { &n.label },
                    sqlly_services::notebook::cells::parameter::value_text(&n.value)
                )),
                Preset::Unknown(_) => {}
            }
        }
        d.named_presets = named.join("; ");
        d
    }

    fn ty(&self) -> Result<ParamType, Field> {
        Ok(match self.kind.as_str() {
            "slider" => {
                let n = |s: &str| s.trim().parse::<f64>().ok().filter(|v| v.is_finite());
                match (
                    n(&self.slider_min),
                    n(&self.slider_max),
                    n(&self.slider_step).or(Some(1.0)),
                ) {
                    (Some(min), Some(max), Some(step)) if min < max && step > 0.0 => {
                        ParamType::Slider { min, max, step }
                    }
                    _ => return Err(Field::Slider),
                }
            }
            "datetime" => ParamType::Datetime {
                tz: Some(self.time_zone.trim().to_string()).filter(|s| !s.is_empty()),
            },
            "identifier" => {
                let allowlist = list_items(&self.allowlist, ',');
                if allowlist.is_empty() {
                    return Err(Field::Allowlist);
                }
                ParamType::Identifier { allowlist }
            }
            other => serde_json::from_value(Value::String(other.to_string()))
                .ok()
                .filter(|ty: &ParamType| !matches!(ty, ParamType::Unknown(_)))
                .unwrap_or(ParamType::String),
        })
    }

    /// The definition this form describes, built on `base` (an existing
    /// definition keeps its id and any fields this form does not show).
    pub fn build(
        &self,
        base: Option<&ParameterDef>,
        clock: &dyn Clock,
    ) -> Result<ParameterDef, (Field, String)> {
        let name = self.name.trim().trim_start_matches(['@', ':']).to_string();
        if name.is_empty() {
            return Err((Field::Name, t("notebook.params.editor.error_name")));
        }
        let ty = self.ty().map_err(|f| (f, t(Self::field_error_key(f))))?;
        let mut def = base
            .cloned()
            .unwrap_or_else(|| ParameterDef::new(name.clone(), ty.clone()));
        if base.is_none() {
            def.id = name.clone();
        }
        def.name = name;
        def.label = self.label.trim().to_string();
        def.ty = ty;
        def.sensitive = self.sensitive;
        def.shareable = self.shareable && !self.sensitive;
        // A setting from a newer SQLLY stays as it was unless the form
        // changes what it does here.
        def.scope = match (self.personal, def.scope) {
            (false, unknown @ ParamScope::Unknown(_)) => unknown,
            (true, _) => ParamScope::Personal,
            (false, _) => ParamScope::Shared,
        };
        def.reactivity = match (self.apply, def.reactivity) {
            (true, unknown @ Reactivity::Unknown(_)) => unknown,
            (true, _) => Reactivity::Apply,
            (false, _) => Reactivity::Auto,
        };
        let bad = |f: Field| (f, t(Self::field_error_key(f)));
        let mut validation = Validation {
            required: self.required,
            extra: def.validation.extra.clone(),
            ..Validation::default()
        };
        let dates = matches!(
            def.ty,
            ParamType::Date | ParamType::Datetime { .. } | ParamType::DateRange
        );
        let bound = |text: &str| -> Result<Option<Value>, (Field, String)> {
            let text = text.trim();
            if text.is_empty() {
                Ok(None)
            } else if dates {
                Ok(Some(Value::String(text.to_string())))
            } else {
                number(text).map_err(|_| bad(Field::Validation))
            }
        };
        validation.min = bound(&self.min)?;
        validation.max = bound(&self.max)?;
        validation.min_length = count(&self.min_length).map_err(|_| bad(Field::Validation))?;
        validation.max_length = count(&self.max_length).map_err(|_| bad(Field::Validation))?;
        def.validation = validation;
        def.allowed_values = None;
        OptionsQuery::store(&mut def, None);
        let choices =
            matches!(def.ty, ParamType::Enum | ParamType::MultiSelect) || takes_options(&def.ty);
        match self.source {
            Source::List if choices => {
                let values: Vec<Value> = list_items(&self.allowed_list, ',')
                    .iter()
                    .map(|item| match item.split_once('=') {
                        Some((value, label)) => serde_json::json!({
                            "label": label.trim(),
                            "value": scalar(value.trim()),
                        }),
                        None => scalar(item),
                    })
                    .collect();
                if values.is_empty() {
                    return Err(bad(Field::AllowedList));
                }
                def.allowed_values = Some(AllowedValues::Static(values));
            }
            Source::Query if choices => {
                if self.query_sql.trim().is_empty() {
                    return Err(bad(Field::Query));
                }
                let mut q = OptionsQuery::new(self.query_sql.trim());
                q.connection_ref = self.query_slot.clone();
                q.value_column =
                    Some(self.value_column.trim().to_string()).filter(|s| !s.is_empty());
                q.label_column =
                    Some(self.label_column.trim().to_string()).filter(|s| !s.is_empty());
                q.limit = match self.limit.trim() {
                    "" => None,
                    text => Some(
                        text.parse::<u32>()
                            .ok()
                            .filter(|n| *n > 0)
                            .ok_or_else(|| bad(Field::Limit))?,
                    ),
                };
                OptionsQuery::store(&mut def, Some(q));
            }
            _ => {}
        }
        let mut presets: Vec<Preset> = self
            .date_presets
            .iter()
            .filter(|p| preset_fits(&def.ty, **p))
            .map(|p| Preset::Date(*p))
            .collect();
        presets.extend(
            def.presets
                .iter()
                .filter(|p| matches!(p, Preset::Unknown(_)))
                .cloned(),
        );
        for (n, entry) in list_items(&self.named_presets, ';').iter().enumerate() {
            let (label, raw) = entry
                .split_once('=')
                .ok_or_else(|| bad(Field::NamedPresets))?;
            let value = parse_default(&def, raw.trim(), clock).map_err(|e| {
                (
                    Field::NamedPresets,
                    crate::localization::message(e.message()),
                )
            })?;
            presets.push(Preset::Named(NamedPreset {
                id: format!("preset_{}", n + 1),
                label: label.trim().to_string(),
                value: value.to_json(),
            }));
        }
        def.presets = presets;
        def.default = if def.is_redacted() {
            None
        } else if let Some(preset) = self.default_preset.filter(|p| preset_fits(&def.ty, *p)) {
            Some(serde_json::json!({ "preset": date_preset_id(preset) }))
        } else if self.default_text.trim().is_empty() {
            None
        } else {
            let value = parse_default(&def, &self.default_text, clock)
                .map_err(|e| (Field::Default, crate::localization::message(e.message())))?;
            if matches!(def.allowed_values, Some(AllowedValues::Static(_))) {
                params::validate(&def, &value)
                    .map_err(|e| (Field::Default, crate::localization::message(e.message())))?;
            }
            Some(value.to_json())
        };
        Ok(def)
    }

    fn field_error_key(field: Field) -> &'static str {
        match field {
            Field::Name => "notebook.params.editor.error_name",
            Field::Slider => "notebook.params.editor.error_slider",
            Field::Allowlist => "notebook.params.editor.error_allowlist",
            Field::Default => "notebook.params.editor.error_default",
            Field::AllowedList => "notebook.params.editor.error_allowed_list",
            Field::Query => "notebook.params.editor.error_query",
            Field::Limit => "notebook.params.editor.error_limit",
            Field::Validation => "notebook.params.editor.error_validation",
            Field::NamedPresets => "notebook.params.editor.error_named_presets",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParamEditorEvent {
    /// Save `def` in place of the parameter named `original` (a new one
    /// when `None`).
    Save {
        original: Option<String>,
        def: Box<ParameterDef>,
    },
    Delete(String),
    Cancel,
}

/// One connection slot the discovery query can run on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SlotChoice {
    pub id: Option<String>,
    pub label: String,
}

struct Inputs {
    name: Entity<InputState>,
    label: Entity<InputState>,
    slider_min: Entity<InputState>,
    slider_max: Entity<InputState>,
    slider_step: Entity<InputState>,
    time_zone: Entity<InputState>,
    allowlist: Entity<InputState>,
    default_text: Entity<InputState>,
    allowed_list: Entity<InputState>,
    query_sql: Entity<TextareaState>,
    value_column: Entity<InputState>,
    label_column: Entity<InputState>,
    limit: Entity<InputState>,
    min: Entity<InputState>,
    max: Entity<InputState>,
    min_length: Entity<InputState>,
    max_length: Entity<InputState>,
    named_presets: Entity<InputState>,
}

pub struct ParamEditor {
    original: Option<ParameterDef>,
    draft: Draft,
    inputs: Inputs,
    kind: Entity<SelectState<Vec<String>>>,
    source: Entity<SelectState<Vec<String>>>,
    slot: Entity<SelectState<Vec<String>>>,
    default_preset: Entity<SelectState<Vec<String>>>,
    slots: Vec<SlotChoice>,
    clock: std::rc::Rc<dyn Clock>,
    error: Option<(Field, String)>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl EventEmitter<ParamEditorEvent> for ParamEditor {}

fn input(
    window: &mut Window,
    cx: &mut Context<ParamEditor>,
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

fn source_index(source: Source) -> usize {
    match source {
        Source::None => 0,
        Source::List => 1,
        Source::Query => 2,
    }
}

impl ParamEditor {
    pub fn new(
        original: Option<ParameterDef>,
        slots: Vec<SlotChoice>,
        clock: std::rc::Rc<dyn Clock>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let draft = original
            .as_ref()
            .map(Draft::from_def)
            .unwrap_or_else(|| Draft {
                kind: "string".into(),
                slider_step: "1".into(),
                shareable: true,
                ..Draft::default()
            });
        let inputs = Inputs {
            name: input(
                window,
                cx,
                "notebook.params.editor.name_placeholder",
                &draft.name,
            ),
            label: input(
                window,
                cx,
                "notebook.params.editor.label_placeholder",
                &draft.label,
            ),
            slider_min: input(
                window,
                cx,
                "notebook.params.editor.min_placeholder",
                &draft.slider_min,
            ),
            slider_max: input(
                window,
                cx,
                "notebook.params.editor.max_placeholder",
                &draft.slider_max,
            ),
            slider_step: input(
                window,
                cx,
                "notebook.params.editor.step_placeholder",
                &draft.slider_step,
            ),
            time_zone: input(
                window,
                cx,
                "notebook.params.editor.time_zone_placeholder",
                &draft.time_zone,
            ),
            allowlist: input(
                window,
                cx,
                "notebook.params.editor.allowlist_placeholder",
                &draft.allowlist,
            ),
            default_text: input(
                window,
                cx,
                "notebook.params.editor.default_placeholder",
                &draft.default_text,
            ),
            allowed_list: input(
                window,
                cx,
                "notebook.params.editor.allowed_list_placeholder",
                &draft.allowed_list,
            ),
            query_sql: {
                let sql = draft.query_sql.clone();
                let state = cx.new(|cx| {
                    TextareaState::new(window, cx).auto_grow(3, 8).placeholder(
                        crate::localization::input_placeholder(
                            window,
                            cx,
                            "notebook.params.editor.query_placeholder",
                        ),
                    )
                });
                if !sql.is_empty() {
                    state.update(cx, |s, cx| s.set_value(sql, window, cx));
                }
                state
            },
            value_column: input(
                window,
                cx,
                "notebook.params.editor.value_column_placeholder",
                &draft.value_column,
            ),
            label_column: input(
                window,
                cx,
                "notebook.params.editor.label_column_placeholder",
                &draft.label_column,
            ),
            limit: input(
                window,
                cx,
                "notebook.params.editor.limit_placeholder",
                &draft.limit,
            ),
            min: input(
                window,
                cx,
                "notebook.params.editor.min_placeholder",
                &draft.min,
            ),
            max: input(
                window,
                cx,
                "notebook.params.editor.max_placeholder",
                &draft.max,
            ),
            min_length: input(
                window,
                cx,
                "notebook.params.editor.min_length_placeholder",
                &draft.min_length,
            ),
            max_length: input(
                window,
                cx,
                "notebook.params.editor.max_length_placeholder",
                &draft.max_length,
            ),
            named_presets: input(
                window,
                cx,
                "notebook.params.editor.named_presets_placeholder",
                &draft.named_presets,
            ),
        };
        let kind_ix = KINDS.iter().position(|k| *k == draft.kind).unwrap_or(0);
        let kind = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    KINDS.iter().map(|k| t(type_label_key(k))).collect()
                }),
                Some(gpui_component::IndexPath::new(kind_ix)),
                window,
                cx,
            )
        });
        let source_ix = source_index(draft.source);
        let source = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    vec![
                        t("notebook.params.editor.source_none"),
                        t("notebook.params.editor.source_list"),
                        t("notebook.params.editor.source_query"),
                    ]
                }),
                Some(gpui_component::IndexPath::new(source_ix)),
                window,
                cx,
            )
        });
        let slot_labels: Vec<String> = slots.iter().map(|s| s.label.clone()).collect();
        let slot_ix = slots
            .iter()
            .position(|s| s.id == draft.query_slot)
            .unwrap_or(0);
        let slot = cx.new(|cx| {
            SelectState::new(
                slot_labels,
                Some(gpui_component::IndexPath::new(slot_ix)),
                window,
                cx,
            )
        });
        let preset_ix = draft
            .default_preset
            .and_then(|p| DATE_PRESETS.iter().position(|d| *d == p))
            .map_or(0, |i| i + 1);
        let default_preset = cx.new(|cx| {
            SelectState::new(
                crate::localization::select_labels(window, cx, || {
                    std::iter::once(t("notebook.params.editor.default_preset_none"))
                        .chain(DATE_PRESETS.iter().map(|p| t(date_preset_label_key(*p))))
                        .collect()
                }),
                Some(gpui_component::IndexPath::new(preset_ix)),
                window,
                cx,
            )
        });
        let subs = vec![
            cx.subscribe(&kind, |this, s, _: &SelectEvent<Vec<String>>, cx| {
                if let Some(ix) = s.read(cx).selected_index(cx) {
                    this.draft.kind = KINDS.get(ix.row).copied().unwrap_or("string").to_string();
                    cx.notify();
                }
            }),
            cx.subscribe(&source, |this, s, _: &SelectEvent<Vec<String>>, cx| {
                if let Some(ix) = s.read(cx).selected_index(cx) {
                    this.draft.source = match ix.row {
                        1 => Source::List,
                        2 => Source::Query,
                        _ => Source::None,
                    };
                    cx.notify();
                }
            }),
            cx.subscribe(&slot, |this, s, _: &SelectEvent<Vec<String>>, cx| {
                if let Some(ix) = s.read(cx).selected_index(cx) {
                    this.draft.query_slot = this.slots.get(ix.row).and_then(|s| s.id.clone());
                }
            }),
            cx.subscribe(
                &default_preset,
                |this, s, _: &SelectEvent<Vec<String>>, cx| {
                    if let Some(ix) = s.read(cx).selected_index(cx) {
                        this.draft.default_preset = ix
                            .row
                            .checked_sub(1)
                            .and_then(|i| DATE_PRESETS.get(i).copied());
                    }
                },
            ),
        ];
        Self {
            original,
            draft,
            inputs,
            kind,
            source,
            slot,
            default_preset,
            slots,
            clock,
            error: None,
            _subscriptions: subs,
        }
    }

    /// Show an error the notebook found on save (a clashing name, say).
    pub fn set_error(&mut self, field: Field, message: String, cx: &mut Context<Self>) {
        self.error = Some((field, message));
        cx.notify();
    }

    /// The form as typed, read from its controls.
    pub fn draft(&self, cx: &gpui::App) -> Draft {
        let v = |e: &Entity<InputState>| e.read(cx).value().to_string();
        let i = &self.inputs;
        Draft {
            name: v(&i.name),
            label: v(&i.label),
            slider_min: v(&i.slider_min),
            slider_max: v(&i.slider_max),
            slider_step: v(&i.slider_step),
            time_zone: v(&i.time_zone),
            allowlist: v(&i.allowlist),
            default_text: v(&i.default_text),
            allowed_list: v(&i.allowed_list),
            query_sql: i.query_sql.read(cx).value().to_string(),
            value_column: v(&i.value_column),
            label_column: v(&i.label_column),
            limit: v(&i.limit),
            min: v(&i.min),
            max: v(&i.max),
            min_length: v(&i.min_length),
            max_length: v(&i.max_length),
            named_presets: v(&i.named_presets),
            ..self.draft.clone()
        }
    }

    /// Replace the form's flags and picks (tests, and the type picker).
    pub fn set_draft_flags(&mut self, update: impl FnOnce(&mut Draft), cx: &mut Context<Self>) {
        update(&mut self.draft);
        cx.notify();
    }

    /// Validate the form and ask the notebook to save it.
    pub fn save(&mut self, cx: &mut Context<Self>) {
        let draft = self.draft(cx);
        match draft.build(self.original.as_ref(), self.clock.as_ref()) {
            Ok(def) => {
                self.error = None;
                cx.emit(ParamEditorEvent::Save {
                    original: self.original.as_ref().map(|d| d.name.clone()),
                    def: Box::new(def),
                });
            }
            Err(error) => self.error = Some(error),
        }
        cx.notify();
    }

    fn row(&self, label_key: &str, control: AnyElement, field: Option<Field>) -> AnyElement {
        let s = Theme::scaled;
        let error = self
            .error
            .as_ref()
            .filter(|(f, _)| Some(*f) == field)
            .map(|(_, m)| m.clone());
        div()
            .flex()
            .flex_col()
            .gap(px(s(2.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(t(label_key)),
            )
            .child(control)
            .when_some(error, |d, message| {
                d.child(
                    div()
                        .text_size(px(Theme::font_size_ratio(0.8)))
                        .text_color(Theme::error())
                        .child(message),
                )
            })
            .into_any_element()
    }

    fn check(
        &self,
        id: &'static str,
        label_key: &str,
        checked: bool,
        cx: &mut Context<Self>,
        set: fn(&mut Draft, bool),
    ) -> AnyElement {
        let this = cx.entity();
        Checkbox::new(id)
            .label(t(label_key))
            .checked(checked)
            .on_click(move |on, _, app| {
                let on = *on;
                this.update(app, |e, cx| e.set_draft_flags(|d| set(d, on), cx));
            })
            .into_any_element()
    }
}

impl Render for ParamEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let i = &self.inputs;
        let kind = self.draft.kind.clone();
        let choices = matches!(
            kind.as_str(),
            "enum" | "multi_select" | "search" | "entity_lookup"
        );
        let dates = matches!(kind.as_str(), "date" | "date_range");
        let source = self.draft.source;
        let ty_for_presets: ParamType = match kind.as_str() {
            "date_range" => ParamType::DateRange,
            "date" => ParamType::Date,
            _ => ParamType::String,
        };
        let mut form = div()
            .flex()
            .flex_col()
            .gap(px(s(8.0)))
            .child(self.row(
                "notebook.params.editor.name",
                Input::new(&i.name).small().into_any_element(),
                Some(Field::Name),
            ))
            .child(self.row(
                "notebook.params.editor.label",
                Input::new(&i.label).small().into_any_element(),
                None,
            ))
            .child(self.row(
                "notebook.params.editor.type",
                Select::new(&self.kind).small().into_any_element(),
                None,
            ));
        if kind == "slider" {
            form = form.child(
                self.row(
                    "notebook.params.editor.slider_range",
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(s(4.0)))
                        .child(div().flex_1().child(Input::new(&i.slider_min).small()))
                        .child(div().flex_1().child(Input::new(&i.slider_max).small()))
                        .child(div().flex_1().child(Input::new(&i.slider_step).small()))
                        .into_any_element(),
                    Some(Field::Slider),
                ),
            );
        }
        if kind == "datetime" {
            form = form.child(self.row(
                "notebook.params.editor.time_zone",
                Input::new(&i.time_zone).small().into_any_element(),
                None,
            ));
        }
        if kind == "identifier" {
            form = form.child(self.row(
                "notebook.params.editor.allowlist",
                Input::new(&i.allowlist).small().into_any_element(),
                Some(Field::Allowlist),
            ));
        }
        if self.draft.sensitive || kind == "secret_ref" {
            form = form.child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.8)))
                    .text_color(Theme::text_dim())
                    .child(t("notebook.params.editor.sensitive_default_note")),
            );
        } else {
            form = form.child(self.row(
                "notebook.params.editor.default",
                Input::new(&i.default_text).small().into_any_element(),
                Some(Field::Default),
            ));
            if dates {
                form = form.child(self.row(
                    "notebook.params.editor.default_preset",
                    Select::new(&self.default_preset).small().into_any_element(),
                    None,
                ));
            }
        }
        if choices {
            form = form.child(self.row(
                "notebook.params.editor.source",
                Select::new(&self.source).small().into_any_element(),
                None,
            ));
            match source {
                Source::List => {
                    form = form.child(self.row(
                        "notebook.params.editor.allowed_list",
                        Input::new(&i.allowed_list).small().into_any_element(),
                        Some(Field::AllowedList),
                    ));
                }
                Source::Query => {
                    form = form
                        .child(self.row(
                            "notebook.params.editor.query",
                            Textarea::new(&i.query_sql).small().into_any_element(),
                            Some(Field::Query),
                        ))
                        .child(self.row(
                            "notebook.params.editor.query_connection",
                            Select::new(&self.slot).small().into_any_element(),
                            None,
                        ))
                        .child(
                            self.row(
                                "notebook.params.editor.query_columns",
                                div()
                                    .flex()
                                    .flex_row()
                                    .gap(px(s(4.0)))
                                    .child(
                                        div().flex_1().child(Input::new(&i.value_column).small()),
                                    )
                                    .child(
                                        div().flex_1().child(Input::new(&i.label_column).small()),
                                    )
                                    .child(div().w(px(s(90.0))).child(Input::new(&i.limit).small()))
                                    .into_any_element(),
                                Some(Field::Limit),
                            ),
                        )
                        .child(
                            div()
                                .text_size(px(Theme::font_size_ratio(0.8)))
                                .text_color(Theme::text_dim())
                                .child(t("notebook.params.editor.query_hint")),
                        );
                }
                Source::None => {}
            }
        }
        form = form.child(
            self.row(
                "notebook.params.editor.validation",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(self.check(
                        "nbp-ed-required",
                        "notebook.params.editor.required",
                        self.draft.required,
                        cx,
                        |d, on| d.required = on,
                    ))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap(px(s(4.0)))
                            .child(div().flex_1().child(Input::new(&i.min).small()))
                            .child(div().flex_1().child(Input::new(&i.max).small()))
                            .child(div().flex_1().child(Input::new(&i.min_length).small()))
                            .child(div().flex_1().child(Input::new(&i.max_length).small())),
                    )
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.75)))
                            .text_color(Theme::text_dim())
                            .child(t("notebook.params.editor.validation_hint")),
                    )
                    .into_any_element(),
                Some(Field::Validation),
            ),
        );
        form = form.child(
            self.row(
                "notebook.params.editor.behavior",
                div()
                    .flex()
                    .flex_col()
                    .gap(px(s(4.0)))
                    .child(self.check(
                        "nbp-ed-sensitive",
                        "notebook.params.editor.sensitive",
                        self.draft.sensitive,
                        cx,
                        |d, on| d.sensitive = on,
                    ))
                    .child(self.check(
                        "nbp-ed-shareable",
                        "notebook.params.editor.shareable",
                        self.draft.shareable && !self.draft.sensitive,
                        cx,
                        |d, on| d.shareable = on,
                    ))
                    .child(self.check(
                        "nbp-ed-personal",
                        "notebook.params.editor.personal",
                        self.draft.personal,
                        cx,
                        |d, on| d.personal = on,
                    ))
                    .child(self.check(
                        "nbp-ed-apply",
                        "notebook.params.editor.apply",
                        self.draft.apply,
                        cx,
                        |d, on| d.apply = on,
                    ))
                    .into_any_element(),
                None,
            ),
        );
        if dates {
            let this = cx.entity();
            let chosen = self.draft.date_presets.clone();
            let boxes: Vec<AnyElement> = DATE_PRESETS
                .iter()
                .copied()
                .filter(|p| preset_fits(&ty_for_presets, *p))
                .enumerate()
                .map(|(n, preset)| {
                    let this = this.clone();
                    Checkbox::new(ElementId::Name(format!("nbp-ed-preset-{n}").into()))
                        .label(t(date_preset_label_key(preset)))
                        .checked(chosen.contains(&preset))
                        .on_click(move |on, _, app| {
                            let on = *on;
                            this.update(app, |e, cx| {
                                e.set_draft_flags(
                                    |d| {
                                        d.date_presets.retain(|p| *p != preset);
                                        if on {
                                            d.date_presets.push(preset);
                                        }
                                    },
                                    cx,
                                )
                            });
                        })
                        .into_any_element()
                })
                .collect();
            form = form.child(
                self.row(
                    "notebook.params.editor.date_presets",
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap(px(s(6.0)))
                        .children(boxes)
                        .into_any_element(),
                    None,
                ),
            );
        }
        form = form.child(self.row(
            "notebook.params.editor.named_presets",
            Input::new(&i.named_presets).small().into_any_element(),
            Some(Field::NamedPresets),
        ));
        let title = if self.original.is_some() {
            t("notebook.params.editor.title_edit")
        } else {
            t("notebook.params.editor.title_new")
        };
        let original = self.original.as_ref().map(|d| d.name.clone());
        div()
            .id("nbp-editor")
            .w(px(s(520.0)))
            .max_h(px(s(640.0)))
            .flex()
            .flex_col()
            .gap(px(s(10.0)))
            .p(px(s(16.0)))
            .rounded(px(s(6.0)))
            .border_1()
            .border_color(Theme::popup_border())
            .bg(Theme::popup_bg())
            .child(div().text_color(Theme::text()).child(title))
            .child(
                div()
                    .id("nbp-editor-form")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(form),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .gap(px(s(8.0)))
                    .when_some(original, |d, name| {
                        d.child(
                            Button::new("nbp-ed-delete")
                                .ghost()
                                .small()
                                .label(t("notebook.params.editor.delete"))
                                .on_click(cx.listener(move |_, _, _, cx| {
                                    cx.emit(ParamEditorEvent::Delete(name.clone()))
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        Button::new("nbp-ed-cancel")
                            .ghost()
                            .small()
                            .label(t("notebook.params.editor.cancel"))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(ParamEditorEvent::Cancel))),
                    )
                    .child(
                        Button::new("nbp-ed-save")
                            .primary()
                            .small()
                            .label(t("notebook.params.editor.save"))
                            .on_click(cx.listener(|this, _, _, cx| this.save(cx))),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sqlly_services::notebook::params::FixedClock;

    fn clock() -> FixedClock {
        FixedClock(
            NaiveDate::from_ymd_opt(2026, 11, 18)
                .unwrap()
                .and_hms_opt(9, 0, 0)
                .unwrap(),
        )
    }

    fn draft(kind: &str) -> Draft {
        Draft {
            name: "p".into(),
            kind: kind.into(),
            slider_step: "1".into(),
            shareable: true,
            ..Draft::default()
        }
    }

    #[test]
    fn every_type_builds_and_round_trips_through_the_form() {
        for kind in KINDS {
            let mut d = draft(kind);
            d.slider_min = "0".into();
            d.slider_max = "100".into();
            d.slider_step = "5".into();
            d.allowlist = "dbo.Orders, dbo.Customers".into();
            d.time_zone = "UTC".into();
            let def = d
                .build(None, &clock())
                .unwrap_or_else(|e| panic!("{kind}: {e:?}"));
            assert_eq!(def.ty.kind(), kind);
            let again = Draft::from_def(&def).build(Some(&def), &clock()).unwrap();
            assert_eq!(again, def, "{kind}");
        }
    }

    #[test]
    fn the_form_fills_every_definition_field() {
        let mut d = draft("enum");
        d.name = "@region".into();
        d.label = "Region".into();
        d.source = Source::Query;
        d.query_sql = "SELECT code, name FROM regions".into();
        d.query_slot = Some("conn_2".into());
        d.limit = "200".into();
        d.default_text = "EU".into();
        d.required = true;
        d.personal = true;
        d.apply = true;
        d.named_presets = "Europe = EU; Americas = AM".into();
        let def = d.build(None, &clock()).unwrap();
        assert_eq!(def.name, "region");
        assert_eq!(def.label, "Region");
        assert!(def.validation.required);
        assert_eq!(def.scope, ParamScope::Personal);
        assert_eq!(def.reactivity, Reactivity::Apply);
        assert_eq!(def.default, Some(Value::from("EU")));
        let q = OptionsQuery::of(&def).unwrap();
        assert_eq!(q.connection_ref.as_deref(), Some("conn_2"));
        assert_eq!(q.limit, Some(200));
        assert_eq!(def.presets.len(), 2);
        let back = Draft::from_def(&def);
        assert_eq!(back.source, Source::Query);
        assert_eq!(back.named_presets, "Europe = EU; Americas = AM");
    }

    #[test]
    fn bad_fields_are_named() {
        let mut d = draft("slider");
        d.slider_min = "5".into();
        d.slider_max = "1".into();
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Slider);
        let mut d = draft("integer");
        d.default_text = "ten".into();
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Default);
        let mut d = draft("enum");
        d.source = Source::List;
        d.allowed_list = "a, b".into();
        d.default_text = "c".into();
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Default);
        let mut d = draft("enum");
        d.source = Source::Query;
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Query);
        let mut d = draft("identifier");
        d.allowlist = " ".into();
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Allowlist);
        let mut d = draft("string");
        d.name = " ".into();
        assert_eq!(d.build(None, &clock()).unwrap_err().0, Field::Name);
    }

    #[test]
    fn date_defaults_accept_phrases_and_presets() {
        let mut d = draft("date_range");
        d.default_text = "last month".into();
        let def = d.build(None, &clock()).unwrap();
        assert_eq!(
            def.default,
            Some(serde_json::json!({"start": "2026-10-01", "end": "2026-10-31"}))
        );
        d.default_preset = Some(DatePreset::PreviousQuarter);
        let def = d.build(None, &clock()).unwrap();
        assert_eq!(
            def.default,
            Some(serde_json::json!({"preset": "previous_quarter"}))
        );
        let mut d = draft("date");
        d.default_text = "yesterday".into();
        assert_eq!(
            d.build(None, &clock()).unwrap().default,
            Some(Value::from("2026-11-17"))
        );
    }

    #[test]
    fn sensitive_parameters_never_keep_a_default_or_share() {
        let mut d = draft("string");
        d.sensitive = true;
        d.default_text = "hunter2".into();
        let def = d.build(None, &clock()).unwrap();
        assert!(def.default.is_none());
        assert!(!def.shareable);
        let mut d = draft("secret_ref");
        d.default_text = "prod-key".into();
        assert!(d.build(None, &clock()).unwrap().default.is_none());
    }

    #[test]
    fn editing_keeps_the_id_and_fields_the_form_does_not_show() {
        let mut def = ParameterDef::new("region", ParamType::String);
        def.id = "p_region".into();
        def.extra.insert("owner".into(), Value::from("finance"));
        let mut d = Draft::from_def(&def);
        d.name = "area".into();
        let built = d.build(Some(&def), &clock()).unwrap();
        assert_eq!(built.id, "p_region");
        assert_eq!(built.name, "area");
        assert_eq!(built.extra.get("owner"), Some(&Value::from("finance")));
    }
}
