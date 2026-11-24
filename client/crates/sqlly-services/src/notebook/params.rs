//! Typed notebook parameters: definitions, value parsing and validation,
//! date presets over an injected [`Clock`], dialect-safe literals, and
//! binding into SQL.
//!
//! SQL reads a parameter as `:name`, `@name` or `{{param.name}}` (a date
//! range as `{{param.name.start}}`, `.end`, `.end_exclusive`). Only declared
//! names are ever replaced, and only in live code: strings, comments and
//! quoted names are left alone. Values become literals for the target
//! engine; identifiers come only from the parameter's allowlist and are
//! quoted; secret references are never rendered into text.

use std::collections::BTreeMap;

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use sqlly_lang::DisplayMessage;
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use super::document::{Extra, ser_extra, ser_sorted};
use super::scan::{is_ident_byte, mask_sql};

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_true(value: &bool) -> bool {
    *value
}

fn yes() -> bool {
    true
}

fn ser_sorted_list<S: Serializer>(values: &[Value], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(values.iter().map(super::document::Sorted))
}

fn ser_opt_sorted<S: Serializer>(value: &Option<Value>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(v) => ser_sorted(v, serializer),
        None => serializer.serialize_none(),
    }
}

/// A parameter's value type. Plain types serialize as a string (`"date"`),
/// configured ones as an object with a `kind` (`{"kind":"slider",…}`).
/// Types from a newer SQLLY load as [`ParamType::Unknown`] and save back
/// unchanged.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamType {
    String,
    Integer,
    Decimal,
    Boolean,
    Date,
    /// A wall-clock timestamp; `tz` is `UTC`, an offset (`+01:00`) or an
    /// IANA zone name.
    Datetime {
        tz: Option<String>,
    },
    DateRange,
    /// One of `allowed_values`.
    Enum,
    /// Any subset of `allowed_values`; binds as a parenthesized list.
    MultiSelect,
    /// Free text with suggestions.
    Search,
    /// A key picked from a lookup (text or integer).
    EntityLookup,
    Slider {
        min: f64,
        max: f64,
        step: f64,
    },
    /// The name of a secret held by the host; never rendered into SQL.
    SecretRef,
    /// A SQL identifier chosen from `allowlist` (dotted names allowed).
    Identifier {
        allowlist: Vec<String>,
    },
    Unknown(Value),
}

impl ParamType {
    pub fn kind(&self) -> &str {
        match self {
            ParamType::String => "string",
            ParamType::Integer => "integer",
            ParamType::Decimal => "decimal",
            ParamType::Boolean => "boolean",
            ParamType::Date => "date",
            ParamType::Datetime { .. } => "datetime",
            ParamType::DateRange => "date_range",
            ParamType::Enum => "enum",
            ParamType::MultiSelect => "multi_select",
            ParamType::Search => "search",
            ParamType::EntityLookup => "entity_lookup",
            ParamType::Slider { .. } => "slider",
            ParamType::SecretRef => "secret_ref",
            ParamType::Identifier { .. } => "identifier",
            ParamType::Unknown(_) => "unknown",
        }
    }

    fn plain(kind: &str) -> Option<ParamType> {
        Some(match kind {
            "string" => ParamType::String,
            "integer" => ParamType::Integer,
            "decimal" => ParamType::Decimal,
            "boolean" => ParamType::Boolean,
            "date" => ParamType::Date,
            "datetime" => ParamType::Datetime { tz: None },
            "date_range" => ParamType::DateRange,
            "enum" => ParamType::Enum,
            "multi_select" => ParamType::MultiSelect,
            "search" => ParamType::Search,
            "entity_lookup" => ParamType::EntityLookup,
            "secret_ref" => ParamType::SecretRef,
            _ => return None,
        })
    }
}

impl Serialize for ParamType {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let value = match self {
            ParamType::Datetime { tz: Some(tz) } => {
                serde_json::json!({"kind": "datetime", "tz": tz})
            }
            ParamType::Slider { min, max, step } => {
                serde_json::json!({"kind": "slider", "max": max, "min": min, "step": step})
            }
            ParamType::Identifier { allowlist } => {
                serde_json::json!({"allowlist": allowlist, "kind": "identifier"})
            }
            ParamType::Unknown(raw) => raw.clone(),
            plain => Value::String(plain.kind().to_string()),
        };
        ser_sorted(&value, serializer)
    }
}

impl<'de> Deserialize<'de> for ParamType {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        let kind = match &raw {
            Value::String(kind) => kind.as_str(),
            Value::Object(map) => map.get("kind").and_then(Value::as_str).unwrap_or(""),
            _ => "",
        };
        let field = |name: &str| raw.get(name);
        let number = |name: &str| field(name).and_then(Value::as_f64);
        let parsed = match kind {
            "datetime" => Some(ParamType::Datetime {
                tz: field("tz").and_then(Value::as_str).map(str::to_string),
            }),
            "slider" => match (number("min"), number("max"), number("step")) {
                (Some(min), Some(max), step) => Some(ParamType::Slider {
                    min,
                    max,
                    step: step.unwrap_or(1.0),
                }),
                _ => None,
            },
            "identifier" => {
                field("allowlist")
                    .and_then(Value::as_array)
                    .map(|list| ParamType::Identifier {
                        allowlist: list
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect(),
                    })
            }
            other => ParamType::plain(other),
        };
        Ok(parsed.unwrap_or(ParamType::Unknown(raw)))
    }
}

/// Where a parameter's default lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ParamScope {
    /// Saved in the notebook for everyone.
    #[default]
    Shared,
    /// Each user keeps their own value outside the file.
    Personal,
    /// From a newer SQLLY: treated as shared here, saved back unchanged.
    Unknown(&'static str),
}

super::lenient::lenient_names!(ParamScope {
    Shared => "shared",
    Personal => "personal",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Reactivity {
    /// A change re-runs dependents in reactive mode right away.
    #[default]
    Auto,
    /// A change waits for an explicit Apply.
    Apply,
    /// From a newer SQLLY: waits for Apply here (never runs on its own),
    /// saved back unchanged.
    Unknown(&'static str),
}

super::lenient::lenient_names!(Reactivity {
    Auto => "auto",
    Apply => "apply",
});

impl Reactivity {
    /// A change waits for Apply before anything re-runs.
    pub fn waits_for_apply(self) -> bool {
        self != Reactivity::Auto
    }
}

/// Static choices, or choices read from an upstream cell's output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AllowedValues {
    /// Scalars, or `{"value": …, "label": …}` objects.
    Static(#[serde(serialize_with = "ser_sorted_list")] Vec<Value>),
    FromCell {
        from_cell: String,
        value_column: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label_column: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<u32>,
    },
    /// A shape from a newer SQLLY: offers no choices here, saved back
    /// unchanged.
    Unknown(#[serde(serialize_with = "ser_sorted")] Value),
}

impl AllowedValues {
    /// The static choices' values (labels dropped); empty for `FromCell`.
    pub fn static_values(&self) -> Vec<&Value> {
        match self {
            AllowedValues::Static(values) => values
                .iter()
                .map(|v| match v {
                    Value::Object(map) => map.get("value").unwrap_or(v),
                    other => other,
                })
                .collect(),
            AllowedValues::FromCell { .. } | AllowedValues::Unknown(_) => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Validation {
    /// Reject NULL and empty text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    /// Lower bound: a number, or an ISO date / datetime for date types.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_sorted"
    )]
    pub min: Option<Value>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_sorted"
    )]
    pub max: Option<Value>,
    /// Characters of text, or number of selections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

impl Validation {
    fn is_empty(&self) -> bool {
        *self == Validation::default()
    }
}

/// Built-in relative date presets. Weeks start on Monday; ranges are
/// inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatePreset {
    Today,
    Yesterday,
    #[serde(rename = "last_7_days")]
    Last7Days,
    #[serde(rename = "last_30_days")]
    Last30Days,
    #[serde(rename = "last_90_days")]
    Last90Days,
    WeekToDate,
    PreviousWeek,
    MonthToDate,
    PreviousMonth,
    CurrentQuarter,
    QuarterToDate,
    PreviousQuarter,
    YearToDate,
    PreviousYear,
}

/// A saved value with a name (tenant, region, environment, cohort…).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedPreset {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(serialize_with = "ser_sorted")]
    pub value: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Preset {
    Date(DatePreset),
    Named(NamedPreset),
    /// A preset from a newer SQLLY (a date preset this build lacks, say):
    /// not offered here, saved back unchanged.
    Unknown(#[serde(serialize_with = "ser_sorted")] Value),
}

/// A notebook parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterDef {
    /// Stable id; graph node `param:<name>` is keyed by name.
    pub id: String,
    /// SQL-identifier name used in `:name`, `@name`, `{{param.name}}`.
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
    #[serde(rename = "type")]
    pub ty: ParamType,
    /// A value in [`ParamValue::to_json`] form, or `{"preset": "<id>"}`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "ser_opt_sorted"
    )]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_values: Option<AllowedValues>,
    #[serde(default, skip_serializing_if = "Validation::is_empty")]
    pub validation: Validation,
    /// Never written to history, exports or URLs; redacted in run records.
    #[serde(default, skip_serializing_if = "is_false")]
    pub sensitive: bool,
    /// May appear in shareable links.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub shareable: bool,
    #[serde(default, skip_serializing_if = "is_shared")]
    pub scope: ParamScope,
    #[serde(default, skip_serializing_if = "is_auto")]
    pub reactivity: Reactivity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub presets: Vec<Preset>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

fn is_shared(scope: &ParamScope) -> bool {
    *scope == ParamScope::Shared
}

fn is_auto(reactivity: &Reactivity) -> bool {
    *reactivity == Reactivity::Auto
}

impl ParameterDef {
    pub fn new(name: impl Into<String>, ty: ParamType) -> Self {
        let name = name.into();
        Self {
            id: name.clone(),
            label: String::new(),
            name,
            ty,
            default: None,
            allowed_values: None,
            validation: Validation::default(),
            sensitive: false,
            shareable: true,
            scope: ParamScope::Shared,
            reactivity: Reactivity::Auto,
            presets: Vec::new(),
            extra: Extra::new(),
        }
    }

    /// Whether the value may leave the machine in text (history, exports,
    /// URLs, run records).
    pub fn is_redacted(&self) -> bool {
        self.sensitive || self.ty == ParamType::SecretRef
    }
}

/// A typed parameter value.
#[derive(Debug, Clone, PartialEq)]
pub enum ParamValue {
    Null,
    Text(String),
    Integer(i64),
    /// Validated decimal text, kept exact.
    Decimal(String),
    Boolean(bool),
    Date(NaiveDate),
    DateTime(NaiveDateTime),
    /// Inclusive on both ends.
    DateRange {
        start: NaiveDate,
        end: NaiveDate,
    },
    List(Vec<ParamValue>),
    Identifier(String),
    /// The host's name for a secret.
    SecretRef(String),
}

impl ParamValue {
    /// Short type tag, part of the fingerprint.
    pub fn type_tag(&self) -> &'static str {
        match self {
            ParamValue::Null => "null",
            ParamValue::Text(_) => "text",
            ParamValue::Integer(_) => "integer",
            ParamValue::Decimal(_) => "decimal",
            ParamValue::Boolean(_) => "boolean",
            ParamValue::Date(_) => "date",
            ParamValue::DateTime(_) => "datetime",
            ParamValue::DateRange { .. } => "date_range",
            ParamValue::List(_) => "list",
            ParamValue::Identifier(_) => "identifier",
            ParamValue::SecretRef(_) => "secret_ref",
        }
    }

    /// The JSON form saved as a default and in run records.
    pub fn to_json(&self) -> Value {
        match self {
            ParamValue::Null => Value::Null,
            ParamValue::Text(s) | ParamValue::Identifier(s) | ParamValue::SecretRef(s) => {
                Value::String(s.clone())
            }
            ParamValue::Decimal(s) => Value::String(s.clone()),
            ParamValue::Integer(n) => Value::from(*n),
            ParamValue::Boolean(b) => Value::Bool(*b),
            ParamValue::Date(d) => Value::String(d.format("%Y-%m-%d").to_string()),
            ParamValue::DateTime(dt) => {
                Value::String(dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
            }
            ParamValue::DateRange { start, end } => serde_json::json!({
                "end": end.format("%Y-%m-%d").to_string(),
                "start": start.format("%Y-%m-%d").to_string(),
            }),
            ParamValue::List(items) => Value::Array(items.iter().map(Self::to_json).collect()),
        }
    }
}

/// Why a value is not acceptable. `message()` gives the localizable text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParamError {
    /// No value and no default.
    Missing,
    Required,
    /// The text does not parse as the parameter's type.
    InvalidValue {
        expected: String,
    },
    NotAllowed,
    OutOfRange,
    TooShort,
    TooLong,
    OffStep,
    IdentifierNotAllowed,
    UnknownPreset {
        preset: String,
    },
    PresetNotApplicable {
        preset: String,
    },
    UnsupportedType {
        kind: String,
    },
}

impl ParamError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            ParamError::Missing => {
                DisplayMessage::new("notebook.core.param.missing", "A value is required")
            }
            ParamError::Required => DisplayMessage::new(
                "notebook.core.param.required",
                "This parameter cannot be empty",
            ),
            ParamError::InvalidValue { expected } => DisplayMessage::new(
                "notebook.core.param.invalid_value",
                format!("Enter a valid {expected} value"),
            )
            .with_arg("expected", expected.clone()),
            ParamError::NotAllowed => DisplayMessage::new(
                "notebook.core.param.not_allowed",
                "Choose one of the allowed values",
            ),
            ParamError::OutOfRange => DisplayMessage::new(
                "notebook.core.param.out_of_range",
                "The value is outside the allowed range",
            ),
            ParamError::TooShort => {
                DisplayMessage::new("notebook.core.param.too_short", "The value is too short")
            }
            ParamError::TooLong => {
                DisplayMessage::new("notebook.core.param.too_long", "The value is too long")
            }
            ParamError::OffStep => DisplayMessage::new(
                "notebook.core.param.off_step",
                "The value does not match the slider's step",
            ),
            ParamError::IdentifierNotAllowed => DisplayMessage::new(
                "notebook.core.param.identifier_not_allowed",
                "Choose a name from the parameter's allow-list",
            ),
            ParamError::UnknownPreset { preset } => DisplayMessage::new(
                "notebook.core.param.unknown_preset",
                format!("Unknown preset {preset}"),
            )
            .with_arg("preset", preset.clone()),
            ParamError::PresetNotApplicable { preset } => DisplayMessage::new(
                "notebook.core.param.preset_not_applicable",
                format!("The preset {preset} does not fit this parameter's type"),
            )
            .with_arg("preset", preset.clone()),
            ParamError::UnsupportedType { kind } => DisplayMessage::new(
                "notebook.core.param.unsupported_type",
                format!("This version of SQLLY cannot use parameters of type {kind}"),
            )
            .with_arg("kind", kind.clone()),
        }
    }
}

/// The current date and time, injected so presets are testable.
pub trait Clock {
    fn now(&self) -> NaiveDateTime;
    fn today(&self) -> NaiveDate {
        self.now().date()
    }
}

/// A clock frozen at one instant.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(pub NaiveDateTime);

impl Clock for FixedClock {
    fn now(&self) -> NaiveDateTime {
        self.0
    }
}

/// The local wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> NaiveDateTime {
        chrono::Local::now().naive_local()
    }
}

fn first_of_month(d: NaiveDate) -> NaiveDate {
    d.with_day(1).unwrap_or(d)
}

fn add_months(d: NaiveDate, months: i32) -> NaiveDate {
    let total = d.year() * 12 + d.month0() as i32 + months;
    NaiveDate::from_ymd_opt(total.div_euclid(12), total.rem_euclid(12) as u32 + 1, 1).unwrap_or(d)
}

fn quarter_start(d: NaiveDate) -> NaiveDate {
    let month0 = d.month0() / 3 * 3;
    NaiveDate::from_ymd_opt(d.year(), month0 + 1, 1).unwrap_or(d)
}

/// The inclusive range a date preset covers on `today`.
pub fn date_preset_range(preset: DatePreset, today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let day = Duration::days(1);
    let week_start = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    match preset {
        DatePreset::Today => (today, today),
        DatePreset::Yesterday => (today - day, today - day),
        DatePreset::Last7Days => (today - Duration::days(6), today),
        DatePreset::Last30Days => (today - Duration::days(29), today),
        DatePreset::Last90Days => (today - Duration::days(89), today),
        DatePreset::WeekToDate => (week_start, today),
        DatePreset::PreviousWeek => (week_start - Duration::days(7), week_start - day),
        DatePreset::MonthToDate => (first_of_month(today), today),
        DatePreset::PreviousMonth => {
            let this = first_of_month(today);
            (add_months(this, -1), this - day)
        }
        DatePreset::CurrentQuarter => {
            let start = quarter_start(today);
            (start, add_months(start, 3) - day)
        }
        DatePreset::QuarterToDate => (quarter_start(today), today),
        DatePreset::PreviousQuarter => {
            let start = quarter_start(today);
            (add_months(start, -3), start - day)
        }
        DatePreset::YearToDate => (
            NaiveDate::from_ymd_opt(today.year(), 1, 1).unwrap_or(today),
            today,
        ),
        DatePreset::PreviousYear => (
            NaiveDate::from_ymd_opt(today.year() - 1, 1, 1).unwrap_or(today),
            NaiveDate::from_ymd_opt(today.year() - 1, 12, 31).unwrap_or(today),
        ),
    }
}

fn preset_id(preset: &Preset) -> String {
    match preset {
        Preset::Date(p) => serde_json::to_value(p)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default(),
        Preset::Named(named) => named.id.clone(),
        Preset::Unknown(raw) => raw
            .as_str()
            .or_else(|| raw.get("id").and_then(Value::as_str))
            .unwrap_or_default()
            .to_string(),
    }
}

/// Resolve a preset for `def` on `clock`'s date.
pub fn resolve_preset(
    def: &ParameterDef,
    preset: &Preset,
    clock: &dyn Clock,
) -> Result<ParamValue, ParamError> {
    match preset {
        Preset::Named(named) => from_json(def, &named.value),
        Preset::Unknown(_) => Err(ParamError::UnknownPreset {
            preset: preset_id(preset),
        }),
        Preset::Date(p) => {
            let (start, end) = date_preset_range(*p, clock.today());
            match (&def.ty, p) {
                (ParamType::DateRange, _) => Ok(ParamValue::DateRange { start, end }),
                (ParamType::Date, DatePreset::Today | DatePreset::Yesterday) => {
                    Ok(ParamValue::Date(start))
                }
                _ => Err(ParamError::PresetNotApplicable {
                    preset: preset_id(preset),
                }),
            }
        }
    }
}

/// Look a preset up by id: a built-in date preset name, or a named preset
/// declared on `def`.
pub fn find_preset(def: &ParameterDef, id: &str) -> Option<Preset> {
    def.presets
        .iter()
        .find(|p| preset_id(p) == id)
        .cloned()
        .or_else(|| {
            serde_json::from_value::<DatePreset>(Value::String(id.to_string()))
                .ok()
                .map(Preset::Date)
        })
}

fn invalid(def: &ParameterDef) -> ParamError {
    ParamError::InvalidValue {
        expected: def.ty.kind().to_string(),
    }
}

fn parse_date(text: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d").ok()
}

fn parse_datetime(text: &str) -> Option<NaiveDateTime> {
    let text = text.trim().trim_end_matches('Z');
    for format in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(text, format) {
            return Some(dt);
        }
    }
    parse_date(text).map(|d| d.and_time(NaiveTime::MIN))
}

/// A decimal number in plain or exponent form (`-12.50`, `1e-3`).
fn is_decimal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut i = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let int_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
    i += int_digits;
    let mut frac_digits = 0;
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        frac_digits = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        i += frac_digits;
    }
    if int_digits + frac_digits == 0 {
        return false;
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        let exp = bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count();
        if exp == 0 || exp > 4 {
            return false;
        }
        i += exp;
    }
    i == bytes.len() && text.parse::<f64>().is_ok_and(f64::is_finite)
}

fn slider_value(min: f64, max: f64, step: f64, n: f64) -> Result<ParamValue, ParamError> {
    if !(min..=max).contains(&n) {
        return Err(ParamError::OutOfRange);
    }
    if step > 0.0 {
        let steps = (n - min) / step;
        if (steps - steps.round()).abs() > 1e-9 {
            return Err(ParamError::OffStep);
        }
    }
    let integral = [min, max, step, n].iter().all(|v| v.fract() == 0.0);
    Ok(if integral && n.abs() < 9.0e15 {
        ParamValue::Integer(n as i64)
    } else {
        ParamValue::Decimal(n.to_string())
    })
}

fn allowed_match(def: &ParameterDef, text: &str) -> Result<ParamValue, ParamError> {
    let Some(allowed) = &def.allowed_values else {
        return Ok(ParamValue::Text(text.to_string()));
    };
    if matches!(allowed, AllowedValues::FromCell { .. }) {
        return Ok(ParamValue::Text(text.to_string()));
    }
    allowed
        .static_values()
        .into_iter()
        .find(|v| match v {
            Value::String(s) => s == text,
            Value::Number(n) => text.parse::<serde_json::Number>().ok().as_ref() == Some(n),
            Value::Bool(b) => text.parse::<bool>() == Ok(*b),
            _ => false,
        })
        .map(scalar_value)
        .ok_or(ParamError::NotAllowed)
}

fn scalar_value(value: &Value) -> ParamValue {
    match value {
        Value::Null => ParamValue::Null,
        Value::Bool(b) => ParamValue::Boolean(*b),
        Value::Number(n) => match n.as_i64() {
            Some(i) => ParamValue::Integer(i),
            None => ParamValue::Decimal(n.to_string()),
        },
        Value::String(s) => ParamValue::Text(s.clone()),
        other => ParamValue::Text(other.to_string()),
    }
}

/// Parse what a person typed into a parameter input. `NULL` (any case) is
/// SQL NULL for scalar types.
pub fn parse_text(def: &ParameterDef, raw: &str) -> Result<ParamValue, ParamError> {
    let text = raw.trim();
    let scalar = !matches!(
        def.ty,
        ParamType::DateRange
            | ParamType::MultiSelect
            | ParamType::SecretRef
            | ParamType::Identifier { .. }
    );
    if scalar && text.eq_ignore_ascii_case("null") {
        return Ok(ParamValue::Null);
    }
    match &def.ty {
        ParamType::String | ParamType::Search => Ok(ParamValue::Text(raw.to_string())),
        ParamType::Integer => text
            .parse::<i64>()
            .map(ParamValue::Integer)
            .map_err(|_| invalid(def)),
        ParamType::Decimal => is_decimal(text)
            .then(|| ParamValue::Decimal(text.to_string()))
            .ok_or_else(|| invalid(def)),
        ParamType::Boolean => match text.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(ParamValue::Boolean(true)),
            "false" | "0" | "no" => Ok(ParamValue::Boolean(false)),
            _ => Err(invalid(def)),
        },
        ParamType::Date => parse_date(text)
            .map(ParamValue::Date)
            .ok_or_else(|| invalid(def)),
        ParamType::Datetime { .. } => parse_datetime(text)
            .map(ParamValue::DateTime)
            .ok_or_else(|| invalid(def)),
        ParamType::DateRange => {
            let (start, end) = text
                .split_once("..")
                .or_else(|| text.split_once('/'))
                .ok_or_else(|| invalid(def))?;
            match (parse_date(start), parse_date(end)) {
                (Some(start), Some(end)) if start <= end => {
                    Ok(ParamValue::DateRange { start, end })
                }
                (Some(_), Some(_)) => Err(ParamError::OutOfRange),
                _ => Err(invalid(def)),
            }
        }
        ParamType::Enum => allowed_match(def, text),
        ParamType::MultiSelect => {
            let items: Vec<String> = match serde_json::from_str::<Vec<Value>>(text) {
                Ok(list) => list
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect(),
                Err(_) if text.is_empty() => Vec::new(),
                Err(_) => text.split(',').map(|s| s.trim().to_string()).collect(),
            };
            items
                .iter()
                .map(|item| allowed_match(def, item))
                .collect::<Result<Vec<_>, _>>()
                .map(ParamValue::List)
        }
        ParamType::EntityLookup => Ok(text
            .parse::<i64>()
            .map(ParamValue::Integer)
            .unwrap_or_else(|_| ParamValue::Text(raw.to_string()))),
        ParamType::Slider { min, max, step } => {
            if !is_decimal(text) {
                return Err(invalid(def));
            }
            slider_value(
                *min,
                *max,
                *step,
                text.parse::<f64>().map_err(|_| invalid(def))?,
            )
        }
        ParamType::SecretRef => (!text.is_empty())
            .then(|| ParamValue::SecretRef(text.to_string()))
            .ok_or(ParamError::Missing),
        ParamType::Identifier { allowlist } => allowlist
            .iter()
            .find(|entry| entry.as_str() == text)
            .or_else(|| allowlist.iter().find(|e| e.eq_ignore_ascii_case(text)))
            .map(|entry| ParamValue::Identifier(entry.clone()))
            .ok_or(ParamError::IdentifierNotAllowed),
        ParamType::Unknown(_) => Err(ParamError::UnsupportedType {
            kind: unknown_kind(&def.ty),
        }),
    }
}

fn unknown_kind(ty: &ParamType) -> String {
    match ty {
        ParamType::Unknown(Value::String(s)) => s.clone(),
        ParamType::Unknown(raw) => raw
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        other => other.kind().to_string(),
    }
}

/// Read a saved JSON value (a default, a preset value, a run record).
pub fn from_json(def: &ParameterDef, value: &Value) -> Result<ParamValue, ParamError> {
    match (&def.ty, value) {
        (ParamType::Unknown(_), _) => Err(ParamError::UnsupportedType {
            kind: unknown_kind(&def.ty),
        }),
        (_, Value::Null) => Ok(ParamValue::Null),
        (ParamType::DateRange, Value::Object(map)) => {
            let part = |k: &str| map.get(k).and_then(Value::as_str).and_then(parse_date);
            match (part("start"), part("end")) {
                (Some(start), Some(end)) if start <= end => {
                    Ok(ParamValue::DateRange { start, end })
                }
                _ => Err(invalid(def)),
            }
        }
        (ParamType::MultiSelect, Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(s) => allowed_match(def, s),
                other => allowed_match(def, &other.to_string()),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(ParamValue::List),
        (ParamType::Boolean, Value::Bool(b)) => Ok(ParamValue::Boolean(*b)),
        (ParamType::Integer | ParamType::EntityLookup, Value::Number(n)) => n
            .as_i64()
            .map(ParamValue::Integer)
            .ok_or_else(|| invalid(def)),
        (ParamType::Decimal | ParamType::Slider { .. }, Value::Number(n)) => {
            parse_text(def, &n.to_string())
        }
        (ParamType::Enum, other) if !other.is_string() => allowed_match(def, &other.to_string()),
        (_, Value::String(s)) => parse_text(def, s),
        _ => Err(invalid(def)),
    }
}

fn bound(def: &ParameterDef, value: &Value) -> Option<ParamValue> {
    match def.ty {
        ParamType::Date | ParamType::DateRange => {
            value.as_str().and_then(parse_date).map(ParamValue::Date)
        }
        ParamType::Datetime { .. } => value
            .as_str()
            .and_then(parse_datetime)
            .map(ParamValue::DateTime),
        _ => value.as_f64().map(|n| ParamValue::Decimal(n.to_string())),
    }
}

fn compare(a: &ParamValue, b: &ParamValue) -> Option<std::cmp::Ordering> {
    let number = |v: &ParamValue| match v {
        ParamValue::Integer(n) => Some(*n as f64),
        ParamValue::Decimal(s) => s.parse::<f64>().ok(),
        _ => None,
    };
    match (a, b) {
        (ParamValue::Date(x), ParamValue::Date(y)) => Some(x.cmp(y)),
        (ParamValue::DateTime(x), ParamValue::DateTime(y)) => Some(x.cmp(y)),
        _ => number(a)?.partial_cmp(&number(b)?),
    }
}

/// Whether a value has a shape `ty` can hold.
fn fits(ty: &ParamType, value: &ParamValue) -> bool {
    use ParamValue as V;
    let choice = |v: &V| {
        matches!(
            v,
            V::Text(_) | V::Integer(_) | V::Decimal(_) | V::Boolean(_)
        )
    };
    match (ty, value) {
        (_, V::Null) => true,
        (ParamType::String | ParamType::Search, V::Text(_))
        | (ParamType::Integer, V::Integer(_))
        | (ParamType::Decimal | ParamType::Slider { .. }, V::Integer(_) | V::Decimal(_))
        | (ParamType::Boolean, V::Boolean(_))
        | (ParamType::Date, V::Date(_))
        | (ParamType::Datetime { .. }, V::DateTime(_))
        | (ParamType::DateRange, V::DateRange { .. })
        | (ParamType::EntityLookup, V::Text(_) | V::Integer(_))
        | (ParamType::SecretRef, V::SecretRef(_))
        | (ParamType::Identifier { .. }, V::Identifier(_)) => true,
        (ParamType::Enum, v) => choice(v),
        (ParamType::MultiSelect, V::List(items)) => items.iter().all(choice),
        _ => false,
    }
}

/// Check a value against `def`'s type, allowed values and validation rules.
pub fn validate(def: &ParameterDef, value: &ParamValue) -> Result<(), ParamError> {
    use std::cmp::Ordering;
    let empty = match value {
        ParamValue::Null => true,
        ParamValue::Text(s) => s.trim().is_empty(),
        ParamValue::List(items) => items.is_empty(),
        _ => false,
    };
    if empty && def.validation.required {
        return Err(ParamError::Required);
    }
    if let ParamValue::Null = value {
        return Ok(());
    }
    if let ParamType::Unknown(_) = def.ty {
        return Err(ParamError::UnsupportedType {
            kind: unknown_kind(&def.ty),
        });
    }
    match (&def.ty, value) {
        (ParamType::Identifier { allowlist }, ParamValue::Identifier(name)) => {
            if !allowlist.iter().any(|entry| entry == name) {
                return Err(ParamError::IdentifierNotAllowed);
            }
        }
        (ParamType::Identifier { .. }, _) => return Err(ParamError::IdentifierNotAllowed),
        (ty, value) if !fits(ty, value) => return Err(invalid(def)),
        (ParamType::Slider { min, max, step }, ParamValue::Integer(_) | ParamValue::Decimal(_)) => {
            let n = match value {
                ParamValue::Integer(n) => *n as f64,
                ParamValue::Decimal(s) => s.parse::<f64>().map_err(|_| invalid(def))?,
                _ => unreachable!(),
            };
            slider_value(*min, *max, *step, n)?;
        }
        _ => {}
    }
    if let Some(allowed) = &def.allowed_values
        && matches!(def.ty, ParamType::Enum | ParamType::MultiSelect)
        && matches!(allowed, AllowedValues::Static(_))
    {
        let choices: Vec<ParamValue> = allowed
            .static_values()
            .into_iter()
            .map(scalar_value)
            .collect();
        let items: Vec<&ParamValue> = match value {
            ParamValue::List(items) => items.iter().collect(),
            single => vec![single],
        };
        if items.iter().any(|item| !choices.contains(item)) {
            return Err(ParamError::NotAllowed);
        }
    }
    let ends: Vec<ParamValue> = match value {
        ParamValue::DateRange { start, end } => {
            vec![ParamValue::Date(*start), ParamValue::Date(*end)]
        }
        other => vec![other.clone()],
    };
    for end in &ends {
        if let Some(min) = def.validation.min.as_ref().and_then(|v| bound(def, v))
            && compare(end, &min) == Some(Ordering::Less)
        {
            return Err(ParamError::OutOfRange);
        }
        if let Some(max) = def.validation.max.as_ref().and_then(|v| bound(def, v))
            && compare(end, &max) == Some(Ordering::Greater)
        {
            return Err(ParamError::OutOfRange);
        }
    }
    let length = match value {
        ParamValue::Text(s) => Some(s.chars().count()),
        ParamValue::List(items) => Some(items.len()),
        _ => None,
    };
    if let Some(length) = length {
        if def.validation.min_length.is_some_and(|min| length < min) {
            return Err(ParamError::TooShort);
        }
        if def.validation.max_length.is_some_and(|max| length > max) {
            return Err(ParamError::TooLong);
        }
    }
    Ok(())
}

/// The value a parameter takes: `supplied` if any, else its default
/// (a value or `{"preset": id}`), validated.
pub fn effective_value(
    def: &ParameterDef,
    supplied: Option<&ParamValue>,
    clock: &dyn Clock,
) -> Result<ParamValue, ParamError> {
    let value = match supplied {
        Some(value) => value.clone(),
        None => match &def.default {
            None => return Err(ParamError::Missing),
            Some(Value::Object(map)) if map.len() == 1 && map.contains_key("preset") => {
                let id = map["preset"].as_str().unwrap_or_default();
                let preset = find_preset(def, id).ok_or_else(|| ParamError::UnknownPreset {
                    preset: id.to_string(),
                })?;
                resolve_preset(def, &preset, clock)?
            }
            Some(raw) => from_json(def, raw)?,
        },
    };
    validate(def, &value)?;
    Ok(value)
}

/// Every parameter's effective value or error, keyed by declared name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResolvedParams {
    pub values: BTreeMap<String, ParamValue>,
    pub errors: BTreeMap<String, ParamError>,
}

impl ResolvedParams {
    /// Case-insensitive lookup by name.
    pub fn get(&self, name: &str) -> Option<&ParamValue> {
        self.values
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }

    pub fn error(&self, name: &str) -> Option<&ParamError> {
        self.errors
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    }
}

/// Resolve every parameter. `supplied` is keyed by name (case-insensitive).
pub fn resolve_all(
    defs: &[ParameterDef],
    supplied: &BTreeMap<String, ParamValue>,
    clock: &dyn Clock,
) -> ResolvedParams {
    let mut out = ResolvedParams::default();
    for def in defs {
        let given = supplied
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&def.name))
            .map(|(_, v)| v);
        match effective_value(def, given, clock) {
            Ok(value) => {
                out.values.insert(def.name.clone(), value);
            }
            Err(error) => {
                out.errors.insert(def.name.clone(), error);
            }
        }
    }
    out
}

/// Why a parameter could not be bound into SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindError {
    Param {
        name: String,
        error: ParamError,
    },
    /// Secret values travel only as native driver parameters.
    SecretNeedsNativeBinding {
        name: String,
    },
    /// A date range needs `.start`, `.end` or `.end_exclusive`.
    NeedsField {
        name: String,
    },
    UnknownField {
        name: String,
        field: String,
    },
    TimezoneUnsupported {
        name: String,
        tz: String,
    },
}

impl BindError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            BindError::Param { name, error } => {
                let inner = error.message();
                DisplayMessage::new(
                    "notebook.core.bind.param",
                    format!("{name}: {}", inner.fallback()),
                )
                .with_arg("name", name.clone())
                .with_message_arg("error", inner)
            }
            BindError::SecretNeedsNativeBinding { name } => DisplayMessage::new(
                "notebook.core.bind.secret_needs_native_binding",
                format!(
                    "{name} is a secret, so it can only be sent as a driver parameter, never written into the SQL"
                ),
            )
            .with_arg("name", name.clone()),
            BindError::NeedsField { name } => DisplayMessage::new(
                "notebook.core.bind.needs_field",
                format!("{name} is a date range: use {name}.start, {name}.end or {name}.end_exclusive"),
            )
            .with_arg("name", name.clone()),
            BindError::UnknownField { name, field } => DisplayMessage::new(
                "notebook.core.bind.unknown_field",
                format!("{name} has no field {field}"),
            )
            .with_arg("name", name.clone())
            .with_arg("field", field.clone()),
            BindError::TimezoneUnsupported { name, tz } => DisplayMessage::new(
                "notebook.core.bind.timezone_unsupported",
                format!("{name}: this database cannot read the time zone {tz} in a literal; use a UTC offset"),
            )
            .with_arg("name", name.clone())
            .with_arg("tz", tz.clone()),
        }
    }
}

fn number_literal(text: &str) -> String {
    // A negative number spliced after a `-` would form `--`.
    if text.starts_with('-') {
        format!("({text})")
    } else {
        text.to_string()
    }
}

fn date_literal(db: DatabaseType, d: NaiveDate) -> String {
    let iso = d.format("%Y-%m-%d");
    match db.connection_kind() {
        ConnectionKind::Tds => format!("CAST('{}' AS date)", d.format("%Y%m%d")),
        ConnectionKind::ClickHouseHttp => format!("toDate('{iso}')"),
        ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => format!("'{iso}'"),
        ConnectionKind::Odbc => format!("#{iso}#"),
        _ => format!("DATE '{iso}'"),
    }
}

/// `+hh:mm` for `UTC`, `Z` or an explicit offset; `None` for a zone name.
fn utc_offset(tz: &str) -> Option<String> {
    let tz = tz.trim();
    if tz.eq_ignore_ascii_case("utc") || tz == "Z" {
        return Some("+00:00".to_string());
    }
    let bytes = tz.as_bytes();
    let ok = bytes.len() == 6
        && matches!(bytes[0], b'+' | b'-')
        && bytes[1..3].iter().all(u8::is_ascii_digit)
        && bytes[3] == b':'
        && bytes[4..6].iter().all(u8::is_ascii_digit);
    ok.then(|| tz.to_string())
}

fn is_zone_name(tz: &str) -> bool {
    !tz.is_empty()
        && tz.len() <= 64
        && tz
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'/' | b'+' | b'-'))
}

fn datetime_literal(db: DatabaseType, dt: NaiveDateTime, tz: Option<&str>) -> Result<String, ()> {
    let spaced = dt.format("%Y-%m-%d %H:%M:%S%.f").to_string();
    let iso = dt.format("%Y-%m-%dT%H:%M:%S%.f").to_string();
    let offset = tz.and_then(utc_offset);
    let zone = match (tz, &offset) {
        (Some(tz), None) if is_zone_name(tz) => Some(tz),
        (Some(_), None) => return Err(()),
        _ => None,
    };
    let kind = db.connection_kind();
    Ok(match kind {
        ConnectionKind::Tds => match (&offset, zone) {
            (None, None) => format!("CAST('{iso}' AS datetime2)"),
            (Some(off), _) => format!("CAST('{iso}{off}' AS datetimeoffset)"),
            (None, Some(_)) => return Err(()),
        },
        ConnectionKind::PostgresWire | ConnectionKind::DuckDbFile => match (&offset, zone) {
            (None, None) => format!("TIMESTAMP '{spaced}'"),
            (Some(off), _) => format!("TIMESTAMPTZ '{spaced}{off}'"),
            (None, Some(zone)) => {
                format!(
                    "(TIMESTAMP '{spaced}' AT TIME ZONE {})",
                    db.string_literal(zone)
                )
            }
        },
        ConnectionKind::OracleNet => match (&offset, zone) {
            (None, None) => format!("TIMESTAMP '{spaced}'"),
            (Some(off), _) => format!("TIMESTAMP '{spaced} {off}'"),
            (None, Some(zone)) => format!("TIMESTAMP '{spaced} {zone}'"),
        },
        ConnectionKind::MySqlWire => match (&offset, zone) {
            (None, None) => format!("TIMESTAMP '{spaced}'"),
            (Some(off), _) => format!("TIMESTAMP '{spaced}{off}'"),
            (None, Some(_)) => return Err(()),
        },
        ConnectionKind::ClickHouseHttp => match (&offset, zone) {
            (None, None) => format!("toDateTime64('{spaced}', 6)"),
            (Some(off), _) if off == "+00:00" => format!("toDateTime64('{spaced}', 6, 'UTC')"),
            (Some(_), _) => return Err(()),
            (None, Some(zone)) => {
                format!("toDateTime64('{spaced}', 6, {})", db.string_literal(zone))
            }
        },
        ConnectionKind::Odbc => match tz {
            None => format!("#{spaced}#"),
            Some(_) => return Err(()),
        },
        _ => match (&offset, zone) {
            (None, None) => format!("'{spaced}'"),
            (Some(off), _) => format!("'{spaced}{off}'"),
            (None, Some(_)) => return Err(()),
        },
    })
}

/// The SQL literal for `value` on `db`. `field` selects a date range end.
pub fn render_literal(
    db: DatabaseType,
    def: &ParameterDef,
    value: &ParamValue,
    field: Option<&str>,
) -> Result<String, BindError> {
    let name = || def.name.clone();
    if let ParamValue::DateRange { start, end } = value {
        let day = match field {
            Some("start") => *start,
            Some("end") => *end,
            Some("end_exclusive") => *end + Duration::days(1),
            Some(other) => {
                return Err(BindError::UnknownField {
                    name: name(),
                    field: other.to_string(),
                });
            }
            None => return Err(BindError::NeedsField { name: name() }),
        };
        return Ok(date_literal(db, day));
    }
    if let Some(field) = field {
        return Err(BindError::UnknownField {
            name: name(),
            field: field.to_string(),
        });
    }
    Ok(match value {
        ParamValue::Null => "NULL".to_string(),
        ParamValue::Text(s) => db.string_literal(s),
        ParamValue::Integer(n) => number_literal(&n.to_string()),
        ParamValue::Decimal(s) => {
            if !is_decimal(s) {
                return Err(BindError::Param {
                    name: name(),
                    error: invalid(def),
                });
            }
            number_literal(s)
        }
        ParamValue::Boolean(b) => db.bool_literal(*b).to_string(),
        ParamValue::Date(d) => date_literal(db, *d),
        ParamValue::DateTime(dt) => {
            let tz = match &def.ty {
                ParamType::Datetime { tz } => tz.as_deref(),
                _ => None,
            };
            datetime_literal(db, *dt, tz).map_err(|()| BindError::TimezoneUnsupported {
                name: name(),
                tz: tz.unwrap_or_default().to_string(),
            })?
        }
        ParamValue::List(items) if items.is_empty() => "(NULL)".to_string(),
        ParamValue::List(items) => {
            let parts = items
                .iter()
                .map(|item| render_literal(db, def, item, None))
                .collect::<Result<Vec<_>, _>>()?;
            format!("({})", parts.join(", "))
        }
        ParamValue::Identifier(ident) => {
            let allowed =
                matches!(&def.ty, ParamType::Identifier { allowlist } if allowlist.contains(ident));
            if !allowed {
                return Err(BindError::Param {
                    name: name(),
                    error: ParamError::IdentifierNotAllowed,
                });
            }
            ident
                .split('.')
                .map(|part| db.quote_identifier(part))
                .collect::<Vec<_>>()
                .join(".")
        }
        ParamValue::SecretRef(_) => {
            return Err(BindError::SecretNeedsNativeBinding { name: name() });
        }
        ParamValue::DateRange { .. } => unreachable!(),
    })
}

/// How a parameter is written in SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamStyle {
    Colon,
    At,
    Template,
}

/// One use of a declared parameter in SQL text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamUse {
    /// The declared name (declaration spelling).
    pub name: String,
    pub field: Option<String>,
    pub style: ParamStyle,
    /// Byte range of the whole token.
    pub start: usize,
    pub end: usize,
}

fn declared<'a>(defs: &'a [ParameterDef], name: &str) -> Option<&'a ParameterDef> {
    defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))
}

/// Parse `param.name[.field]` inside `{{ … }}`.
pub(crate) fn template_param(inner: &str) -> Option<(&str, Option<&str>)> {
    let rest = inner.trim().strip_prefix("param.")?;
    let (name, field) = match rest.split_once('.') {
        Some((name, field)) => (name, Some(field)),
        None => (rest, None),
    };
    let ident = |s: &str| super::scan::is_identifier(s);
    (ident(name) && field.is_none_or(ident)).then_some((name, field))
}

/// Every use of a declared parameter in live SQL code, in order: `:name`
/// (not after a name, `:` or `]`, so casts and slices are left alone),
/// `@name` (not `@@name`), and `{{param.name}}`.
pub fn find_param_uses(db: DatabaseType, sql: &str, defs: &[ParameterDef]) -> Vec<ParamUse> {
    let masked = mask_sql(db, sql);
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let prev = i.checked_sub(1).map(|p| bytes[p]);
        if b == b'{' && bytes.get(i + 1) == Some(&b'{') {
            if let Some(close) = masked[i + 2..].find("}}") {
                let inner = &masked[i + 2..i + 2 + close];
                if !inner.contains('{')
                    && let Some((name, field)) = template_param(inner)
                    && let Some(def) = declared(defs, name)
                {
                    let end = i + 2 + close + 2;
                    out.push(ParamUse {
                        name: def.name.clone(),
                        field: field.map(str::to_string),
                        style: ParamStyle::Template,
                        start: i,
                        end,
                    });
                    i = end;
                    continue;
                }
            }
            i += 2;
            continue;
        }
        let sigil = match b {
            b':' if prev.is_none_or(|p| !is_ident_byte(p) && p != b':' && p != b']') => {
                Some(ParamStyle::Colon)
            }
            b'@' if prev.is_none_or(|p| !is_ident_byte(p) && p != b'@') => Some(ParamStyle::At),
            _ => None,
        };
        if let Some(style) = sigil {
            let len = bytes[i + 1..]
                .iter()
                .take_while(|b| b.is_ascii_alphanumeric() || **b == b'_')
                .count();
            let after = bytes.get(i + 1 + len).copied();
            let name = &masked[i + 1..i + 1 + len];
            if len > 0
                && !bytes[i + 1].is_ascii_digit()
                && after.is_none_or(|a| !is_ident_byte(a))
                && let Some(def) = declared(defs, name)
            {
                out.push(ParamUse {
                    name: def.name.clone(),
                    field: None,
                    style,
                    start: i,
                    end: i + 1 + len,
                });
                i += 1 + len;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// SQL with every declared parameter replaced by its literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSql {
    pub sql: String,
    /// Parameters used, declaration spelling, first use first.
    pub used: Vec<String>,
}

/// Replace every declared parameter in `sql` with its value's literal for
/// `db`. Every problem is reported before any text is rewritten.
pub fn bind_sql(
    db: DatabaseType,
    sql: &str,
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
) -> Result<BoundSql, BindError> {
    let uses = find_param_uses(db, sql, defs);
    let mut literals = Vec::with_capacity(uses.len());
    let mut used: Vec<String> = Vec::new();
    for u in &uses {
        let def = declared(defs, &u.name).expect("uses are declared");
        if def.ty == ParamType::SecretRef {
            return Err(BindError::SecretNeedsNativeBinding {
                name: def.name.clone(),
            });
        }
        let value = match resolved.get(&def.name) {
            Some(value) => value,
            None => {
                return Err(BindError::Param {
                    name: def.name.clone(),
                    error: resolved
                        .error(&def.name)
                        .cloned()
                        .unwrap_or(ParamError::Missing),
                });
            }
        };
        literals.push(render_literal(db, def, value, u.field.as_deref())?);
        if !used.contains(&def.name) {
            used.push(def.name.clone());
        }
    }
    let mut out = String::with_capacity(sql.len());
    let mut last = 0;
    for (u, literal) in uses.iter().zip(literals) {
        out.push_str(&sql[last..u.start]);
        out.push_str(&literal);
        last = u.end;
    }
    out.push_str(&sql[last..]);
    Ok(BoundSql { sql: out, used })
}

/// What may be kept or hashed for a run of `sql` bound as `bound`: the
/// bound text when no sensitive parameter is used, else `sql` with only the
/// other parameters bound (a hash over a short secret such as a PIN can be
/// brute-forced), else [`UNRECORDED_SQL`].
pub fn shareable_sql(
    db: DatabaseType,
    sql: &str,
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
    bound: &BoundSql,
) -> String {
    let sensitive = bound
        .used
        .iter()
        .any(|name| declared(defs, name).is_some_and(ParameterDef::is_redacted));
    if !sensitive {
        return bound.sql.clone();
    }
    let shown: Vec<ParameterDef> = defs.iter().filter(|d| !d.is_redacted()).cloned().collect();
    bind_sql(db, sql, &shown, resolved)
        .map(|b| b.sql)
        .unwrap_or_else(|_| UNRECORDED_SQL.to_string())
}

/// Stands in for SQL whose sensitive values could not be left out. Saved
/// as is, so never translated.
pub const UNRECORDED_SQL: &str = "-- Not recorded: this query uses a sensitive parameter.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shareable_sql_leaves_sensitive_values_unbound() {
        let mut pin = ParameterDef::new("pin", ParamType::String);
        pin.sensitive = true;
        let region = ParameterDef::new("region", ParamType::String);
        let defs = vec![pin, region];
        let mut resolved = ResolvedParams::default();
        resolved
            .values
            .insert("pin".into(), ParamValue::Text("8675".into()));
        resolved
            .values
            .insert("region".into(), ParamValue::Text("EU".into()));
        let sql = "SELECT * FROM t WHERE pin = :pin AND region = :region";
        let bound = bind_sql(DatabaseType::Postgres, sql, &defs, &resolved).unwrap();
        assert!(bound.sql.contains("8675"));
        let kept = shareable_sql(DatabaseType::Postgres, sql, &defs, &resolved, &bound);
        assert!(
            !kept.contains("8675") && kept.contains(":pin") && kept.contains("'EU'"),
            "{kept}"
        );
        let plain = "SELECT * FROM t WHERE region = :region";
        let bound = bind_sql(DatabaseType::Postgres, plain, &defs, &resolved).unwrap();
        assert_eq!(
            shareable_sql(DatabaseType::Postgres, plain, &defs, &resolved, &bound),
            bound.sql
        );
    }

    fn def(name: &str, ty: ParamType) -> ParameterDef {
        ParameterDef::new(name, ty)
    }

    fn clock(y: i32, m: u32, d: u32) -> FixedClock {
        FixedClock(
            NaiveDate::from_ymd_opt(y, m, d)
                .unwrap()
                .and_hms_opt(10, 30, 0)
                .unwrap(),
        )
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn resolved(pairs: &[(&str, ParamValue)]) -> ResolvedParams {
        ResolvedParams {
            values: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
            errors: BTreeMap::new(),
        }
    }

    #[test]
    fn types_serialize_plain_or_configured_and_round_trip() {
        let cases = vec![
            (ParamType::Date, serde_json::json!("date")),
            (
                ParamType::Datetime { tz: None },
                serde_json::json!("datetime"),
            ),
            (
                ParamType::Datetime {
                    tz: Some("UTC".into()),
                },
                serde_json::json!({"kind": "datetime", "tz": "UTC"}),
            ),
            (
                ParamType::Slider {
                    min: 0.0,
                    max: 10.0,
                    step: 0.5,
                },
                serde_json::json!({"kind": "slider", "min": 0.0, "max": 10.0, "step": 0.5}),
            ),
            (
                ParamType::Identifier {
                    allowlist: vec!["dbo.Orders".into()],
                },
                serde_json::json!({"kind": "identifier", "allowlist": ["dbo.Orders"]}),
            ),
        ];
        for (ty, json) in cases {
            assert_eq!(serde_json::to_value(&ty).unwrap(), json);
            assert_eq!(serde_json::from_value::<ParamType>(json).unwrap(), ty);
        }
        for kind in [
            "string",
            "integer",
            "decimal",
            "boolean",
            "date_range",
            "enum",
            "multi_select",
            "search",
            "entity_lookup",
            "secret_ref",
        ] {
            let ty: ParamType = serde_json::from_value(serde_json::json!(kind)).unwrap();
            assert_eq!(ty.kind(), kind);
            assert_eq!(serde_json::to_value(&ty).unwrap(), serde_json::json!(kind));
        }
        let future = serde_json::json!({"kind": "geo_point", "srid": 4326});
        let ty: ParamType = serde_json::from_value(future.clone()).unwrap();
        assert!(matches!(ty, ParamType::Unknown(_)));
        assert_eq!(serde_json::to_value(&ty).unwrap(), future);
    }

    #[test]
    fn definitions_round_trip_with_defaults_presets_and_unknown_fields() {
        let json = serde_json::json!({
            "id": "p1", "name": "period", "label": "Period", "type": "date_range",
            "default": {"preset": "last_7_days"},
            "presets": ["last_7_days", "previous_month", {"id": "fy", "label": "FY", "value": {"start": "2026-04-01", "end": "2027-03-31"}}],
            "reactivity": "apply", "scope": "personal", "sensitive": true, "shareable": false,
            "validation": {"min": "2020-01-01"},
            "widget_hint": {"compact": true}
        });
        let def: ParameterDef = serde_json::from_value(json.clone()).unwrap();
        assert_eq!(def.reactivity, Reactivity::Apply);
        assert_eq!(def.scope, ParamScope::Personal);
        assert_eq!(def.presets.len(), 3);
        assert!(def.is_redacted());
        let back = serde_json::to_value(&def).unwrap();
        assert_eq!(back, json);
        let minimal: ParameterDef =
            serde_json::from_value(serde_json::json!({"id": "x", "name": "x", "type": "integer"}))
                .unwrap();
        assert!(minimal.shareable);
        assert_eq!(
            serde_json::to_value(&minimal).unwrap(),
            serde_json::json!({"id": "x", "name": "x", "type": "integer"})
        );
    }

    #[test]
    fn parses_each_type_from_text() {
        let ok = |ty: ParamType, raw: &str| parse_text(&def("p", ty), raw).unwrap();
        assert_eq!(ok(ParamType::Integer, " 42 "), ParamValue::Integer(42));
        assert_eq!(ok(ParamType::Integer, "null"), ParamValue::Null);
        assert_eq!(
            ok(ParamType::Decimal, "-12.50"),
            ParamValue::Decimal("-12.50".into())
        );
        assert_eq!(
            ok(ParamType::Decimal, "1e-3"),
            ParamValue::Decimal("1e-3".into())
        );
        assert_eq!(ok(ParamType::Boolean, "Yes"), ParamValue::Boolean(true));
        assert_eq!(
            ok(ParamType::Date, "2026-02-28"),
            ParamValue::Date(date(2026, 2, 28))
        );
        assert_eq!(
            ok(ParamType::Datetime { tz: None }, "2026-02-28 13:05"),
            ParamValue::DateTime(date(2026, 2, 28).and_hms_opt(13, 5, 0).unwrap())
        );
        assert_eq!(
            ok(ParamType::DateRange, "2026-01-01..2026-01-31"),
            ParamValue::DateRange {
                start: date(2026, 1, 1),
                end: date(2026, 1, 31)
            }
        );
        assert_eq!(ok(ParamType::EntityLookup, "17"), ParamValue::Integer(17));
        assert_eq!(
            ok(ParamType::EntityLookup, "ACME"),
            ParamValue::Text("ACME".into())
        );
        assert_eq!(
            ok(ParamType::Search, " a b "),
            ParamValue::Text(" a b ".into())
        );
        let bad = |ty: ParamType, raw: &str| parse_text(&def("p", ty), raw).unwrap_err();
        assert!(matches!(
            bad(ParamType::Integer, "4.2"),
            ParamError::InvalidValue { .. }
        ));
        for raw in ["1.2.3", "--1", "1e", "e5", ".", "1e99999", "0x10", "1,5"] {
            assert!(
                matches!(
                    bad(ParamType::Decimal, raw),
                    ParamError::InvalidValue { .. }
                ),
                "{raw}"
            );
        }
        assert!(matches!(
            bad(ParamType::Date, "2026-02-30"),
            ParamError::InvalidValue { .. }
        ));
        assert!(matches!(
            bad(ParamType::Date, "02/03/2026"),
            ParamError::InvalidValue { .. }
        ));
        assert_eq!(
            bad(ParamType::DateRange, "2026-02-01..2026-01-01"),
            ParamError::OutOfRange
        );
        assert!(matches!(
            bad(ParamType::Boolean, "maybe"),
            ParamError::InvalidValue { .. }
        ));
    }

    fn bad_unknown(ty: ParamType) -> ParamError {
        parse_text(&def("p", ty), "x").unwrap_err()
    }

    #[test]
    fn unknown_types_never_bind() {
        assert!(matches!(
            bad_unknown(ParamType::Unknown(serde_json::json!({"kind": "geo"}))),
            ParamError::UnsupportedType { kind } if kind == "geo"
        ));
    }

    #[test]
    fn enums_and_multi_selects_use_allowed_values() {
        let mut region = def("region", ParamType::Enum);
        region.allowed_values = Some(AllowedValues::Static(vec![
            serde_json::json!("EMEA"),
            serde_json::json!({"value": "APAC", "label": "Asia Pacific"}),
            serde_json::json!(7),
        ]));
        assert_eq!(
            parse_text(&region, "APAC").unwrap(),
            ParamValue::Text("APAC".into())
        );
        assert_eq!(parse_text(&region, "7").unwrap(), ParamValue::Integer(7));
        assert_eq!(
            parse_text(&region, "US").unwrap_err(),
            ParamError::NotAllowed
        );
        assert_eq!(
            validate(&region, &ParamValue::Text("US".into())),
            Err(ParamError::NotAllowed)
        );
        let mut many = region.clone();
        many.ty = ParamType::MultiSelect;
        assert_eq!(
            parse_text(&many, "EMEA, 7").unwrap(),
            ParamValue::List(vec![
                ParamValue::Text("EMEA".into()),
                ParamValue::Integer(7)
            ])
        );
        assert_eq!(
            parse_text(&many, r#"["APAC"]"#).unwrap(),
            ParamValue::List(vec![ParamValue::Text("APAC".into())])
        );
        assert_eq!(parse_text(&many, "").unwrap(), ParamValue::List(vec![]));
        many.validation.max_length = Some(1);
        assert_eq!(
            effective_value(
                &many,
                Some(&parse_text(&many, "EMEA,APAC").unwrap()),
                &clock(2026, 1, 1)
            ),
            Err(ParamError::TooLong)
        );
    }

    #[test]
    fn sliders_check_bounds_and_step() {
        let s = def(
            "s",
            ParamType::Slider {
                min: 0.0,
                max: 100.0,
                step: 5.0,
            },
        );
        assert_eq!(parse_text(&s, "15").unwrap(), ParamValue::Integer(15));
        assert_eq!(parse_text(&s, "17").unwrap_err(), ParamError::OffStep);
        assert_eq!(parse_text(&s, "105").unwrap_err(), ParamError::OutOfRange);
        let f = def(
            "f",
            ParamType::Slider {
                min: 0.0,
                max: 1.0,
                step: 0.25,
            },
        );
        assert_eq!(
            parse_text(&f, "0.75").unwrap(),
            ParamValue::Decimal("0.75".into())
        );
        assert_eq!(
            validate(&f, &ParamValue::Decimal("0.3".into())),
            Err(ParamError::OffStep)
        );
    }

    #[test]
    fn identifiers_come_only_from_the_allowlist() {
        let t = def(
            "t",
            ParamType::Identifier {
                allowlist: vec!["dbo.Orders".into(), "Sales".into()],
            },
        );
        assert_eq!(
            parse_text(&t, "dbo.orders").unwrap(),
            ParamValue::Identifier("dbo.Orders".into())
        );
        assert_eq!(
            parse_text(&t, "Orders; DROP TABLE x").unwrap_err(),
            ParamError::IdentifierNotAllowed
        );
        assert_eq!(
            validate(&t, &ParamValue::Identifier("evil".into())),
            Err(ParamError::IdentifierNotAllowed)
        );
        assert_eq!(
            validate(&t, &ParamValue::Text("Sales".into())),
            Err(ParamError::IdentifierNotAllowed)
        );
        assert_eq!(
            render_literal(
                DatabaseType::SqlServer,
                &t,
                &ParamValue::Identifier("dbo.Orders".into()),
                None
            )
            .unwrap(),
            "[dbo].[Orders]"
        );
        assert_eq!(
            render_literal(
                DatabaseType::Postgres,
                &t,
                &ParamValue::Identifier("Sales".into()),
                None
            )
            .unwrap(),
            "\"Sales\""
        );
        assert!(
            render_literal(
                DatabaseType::MySql,
                &t,
                &ParamValue::Identifier("x".into()),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn validation_bounds_lengths_and_required() {
        let mut n = def("n", ParamType::Integer);
        n.validation.min = Some(serde_json::json!(1));
        n.validation.max = Some(serde_json::json!(10));
        assert_eq!(
            validate(&n, &ParamValue::Integer(0)),
            Err(ParamError::OutOfRange)
        );
        assert_eq!(
            validate(&n, &ParamValue::Integer(11)),
            Err(ParamError::OutOfRange)
        );
        assert!(validate(&n, &ParamValue::Integer(10)).is_ok());
        assert!(validate(&n, &ParamValue::Null).is_ok());
        n.validation.required = true;
        assert_eq!(validate(&n, &ParamValue::Null), Err(ParamError::Required));
        let mut d = def("d", ParamType::DateRange);
        d.validation.min = Some(serde_json::json!("2026-01-01"));
        assert_eq!(
            validate(
                &d,
                &ParamValue::DateRange {
                    start: date(2025, 12, 31),
                    end: date(2026, 1, 5)
                }
            ),
            Err(ParamError::OutOfRange)
        );
        let mut s = def("s", ParamType::String);
        s.validation.min_length = Some(2);
        s.validation.max_length = Some(3);
        assert_eq!(
            validate(&s, &ParamValue::Text("é".into())),
            Err(ParamError::TooShort)
        );
        assert_eq!(
            validate(&s, &ParamValue::Text("abcd".into())),
            Err(ParamError::TooLong)
        );
        assert!(validate(&s, &ParamValue::Text("éé".into())).is_ok());
    }

    #[test]
    fn date_presets_resolve_against_the_clock() {
        let today = date(2026, 11, 18); // a Wednesday
        let r = |p| date_preset_range(p, today);
        assert_eq!(r(DatePreset::Today), (today, today));
        assert_eq!(
            r(DatePreset::Yesterday),
            (date(2026, 11, 17), date(2026, 11, 17))
        );
        assert_eq!(r(DatePreset::Last7Days), (date(2026, 11, 12), today));
        assert_eq!(r(DatePreset::Last30Days), (date(2026, 10, 20), today));
        assert_eq!(r(DatePreset::WeekToDate), (date(2026, 11, 16), today));
        assert_eq!(
            r(DatePreset::PreviousWeek),
            (date(2026, 11, 9), date(2026, 11, 15))
        );
        assert_eq!(r(DatePreset::MonthToDate), (date(2026, 11, 1), today));
        assert_eq!(
            r(DatePreset::PreviousMonth),
            (date(2026, 10, 1), date(2026, 10, 31))
        );
        assert_eq!(
            r(DatePreset::CurrentQuarter),
            (date(2026, 10, 1), date(2026, 12, 31))
        );
        assert_eq!(r(DatePreset::QuarterToDate), (date(2026, 10, 1), today));
        assert_eq!(
            r(DatePreset::PreviousQuarter),
            (date(2026, 7, 1), date(2026, 9, 30))
        );
        assert_eq!(r(DatePreset::YearToDate), (date(2026, 1, 1), today));
        assert_eq!(
            r(DatePreset::PreviousYear),
            (date(2025, 1, 1), date(2025, 12, 31))
        );
        // Year and leap boundaries.
        assert_eq!(
            date_preset_range(DatePreset::PreviousMonth, date(2026, 1, 15)),
            (date(2025, 12, 1), date(2025, 12, 31))
        );
        assert_eq!(
            date_preset_range(DatePreset::PreviousMonth, date(2024, 3, 31)),
            (date(2024, 2, 1), date(2024, 2, 29))
        );
        assert_eq!(
            date_preset_range(DatePreset::PreviousQuarter, date(2026, 2, 1)),
            (date(2025, 10, 1), date(2025, 12, 31))
        );
    }

    #[test]
    fn preset_ranges_are_well_formed_every_day_for_years() {
        let presets = [
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
        let mut day = date(2023, 1, 1);
        while day < date(2027, 1, 1) {
            for p in presets {
                let (start, end) = date_preset_range(p, day);
                assert!(start <= end, "{p:?} on {day}");
                let to_date = matches!(
                    p,
                    DatePreset::Today
                        | DatePreset::Last7Days
                        | DatePreset::Last30Days
                        | DatePreset::Last90Days
                        | DatePreset::WeekToDate
                        | DatePreset::MonthToDate
                        | DatePreset::QuarterToDate
                        | DatePreset::YearToDate
                );
                if to_date {
                    assert_eq!(end, day);
                } else if p != DatePreset::CurrentQuarter {
                    assert!(end < day, "{p:?} on {day}");
                }
                if matches!(
                    p,
                    DatePreset::PreviousMonth
                        | DatePreset::CurrentQuarter
                        | DatePreset::PreviousQuarter
                ) {
                    assert_eq!(start.day(), 1);
                    assert_eq!((end + Duration::days(1)).day(), 1);
                }
            }
            day += Duration::days(1);
        }
    }

    #[test]
    fn defaults_and_presets_drive_effective_values() {
        let c = clock(2026, 11, 18);
        let mut period = def("period", ParamType::DateRange);
        period.default = Some(serde_json::json!({"preset": "previous_month"}));
        assert_eq!(
            effective_value(&period, None, &c).unwrap(),
            ParamValue::DateRange {
                start: date(2026, 10, 1),
                end: date(2026, 10, 31)
            }
        );
        period.presets.push(Preset::Named(NamedPreset {
            id: "fy".into(),
            label: "FY".into(),
            value: serde_json::json!({"start": "2026-04-01", "end": "2027-03-31"}),
        }));
        period.default = Some(serde_json::json!({"preset": "fy"}));
        assert_eq!(
            effective_value(&period, None, &c).unwrap(),
            ParamValue::DateRange {
                start: date(2026, 4, 1),
                end: date(2027, 3, 31)
            }
        );
        period.default = Some(serde_json::json!({"preset": "someday"}));
        assert!(matches!(
            effective_value(&period, None, &c),
            Err(ParamError::UnknownPreset { .. })
        ));
        let mut day = def("day", ParamType::Date);
        day.default = Some(serde_json::json!({"preset": "yesterday"}));
        assert_eq!(
            effective_value(&day, None, &c).unwrap(),
            ParamValue::Date(date(2026, 11, 17))
        );
        day.default = Some(serde_json::json!({"preset": "last_7_days"}));
        assert!(matches!(
            effective_value(&day, None, &c),
            Err(ParamError::PresetNotApplicable { .. })
        ));
        let bare = def("x", ParamType::Integer);
        assert_eq!(effective_value(&bare, None, &c), Err(ParamError::Missing));
        assert_eq!(
            effective_value(&bare, Some(&ParamValue::Integer(3)), &c).unwrap(),
            ParamValue::Integer(3)
        );
        let all = resolve_all(&[period.clone(), bare.clone()], &BTreeMap::new(), &c);
        assert!(all.error("PERIOD").is_some());
        assert_eq!(all.error("x"), Some(&ParamError::Missing));
    }

    #[test]
    fn json_values_round_trip_per_type() {
        let cases: Vec<(ParamType, ParamValue)> = vec![
            (ParamType::String, ParamValue::Text("O'Brien".into())),
            (ParamType::Integer, ParamValue::Integer(-5)),
            (ParamType::Decimal, ParamValue::Decimal("12.50".into())),
            (ParamType::Boolean, ParamValue::Boolean(false)),
            (ParamType::Date, ParamValue::Date(date(2026, 1, 2))),
            (
                ParamType::Datetime { tz: None },
                ParamValue::DateTime(date(2026, 1, 2).and_hms_milli_opt(3, 4, 5, 600).unwrap()),
            ),
            (
                ParamType::DateRange,
                ParamValue::DateRange {
                    start: date(2026, 1, 1),
                    end: date(2026, 2, 1),
                },
            ),
            (
                ParamType::SecretRef,
                ParamValue::SecretRef("vault.api".into()),
            ),
        ];
        for (ty, value) in cases {
            let d = def("p", ty);
            assert_eq!(from_json(&d, &value.to_json()).unwrap(), value);
        }
    }

    #[test]
    fn literals_per_dialect() {
        let lit =
            |db, ty: ParamType, v: ParamValue| render_literal(db, &def("p", ty), &v, None).unwrap();
        use DatabaseType::*;
        assert_eq!(
            lit(
                SqlServer,
                ParamType::String,
                ParamValue::Text("O'Neil".into())
            ),
            "N'O''Neil'"
        );
        assert_eq!(
            lit(Postgres, ParamType::String, ParamValue::Text("a\\b".into())),
            "E'a\\\\b'"
        );
        assert_eq!(
            lit(MySql, ParamType::String, ParamValue::Text("a\\'".into())),
            "'a\\\\'''"
        );
        assert_eq!(
            lit(SqlServer, ParamType::Integer, ParamValue::Integer(-3)),
            "(-3)"
        );
        assert_eq!(
            lit(SqlServer, ParamType::Boolean, ParamValue::Boolean(true)),
            "1"
        );
        assert_eq!(
            lit(Postgres, ParamType::Boolean, ParamValue::Boolean(true)),
            "TRUE"
        );
        let d = ParamValue::Date(date(2026, 11, 1));
        assert_eq!(
            lit(SqlServer, ParamType::Date, d.clone()),
            "CAST('20261101' AS date)"
        );
        assert_eq!(
            lit(Postgres, ParamType::Date, d.clone()),
            "DATE '2026-11-01'"
        );
        assert_eq!(lit(MySql, ParamType::Date, d.clone()), "DATE '2026-11-01'");
        assert_eq!(lit(Oracle, ParamType::Date, d.clone()), "DATE '2026-11-01'");
        assert_eq!(lit(Sqlite, ParamType::Date, d.clone()), "'2026-11-01'");
        assert_eq!(
            lit(ClickHouse, ParamType::Date, d.clone()),
            "toDate('2026-11-01')"
        );
        assert_eq!(lit(Access, ParamType::Date, d), "#2026-11-01#");
        let dt = ParamValue::DateTime(date(2026, 11, 1).and_hms_opt(8, 0, 0).unwrap());
        let naive = ParamType::Datetime { tz: None };
        assert_eq!(
            lit(SqlServer, naive.clone(), dt.clone()),
            "CAST('2026-11-01T08:00:00' AS datetime2)"
        );
        assert_eq!(
            lit(Postgres, naive.clone(), dt.clone()),
            "TIMESTAMP '2026-11-01 08:00:00'"
        );
        let utc = ParamType::Datetime {
            tz: Some("UTC".into()),
        };
        assert_eq!(
            lit(SqlServer, utc.clone(), dt.clone()),
            "CAST('2026-11-01T08:00:00+00:00' AS datetimeoffset)"
        );
        assert_eq!(
            lit(Postgres, utc, dt.clone()),
            "TIMESTAMPTZ '2026-11-01 08:00:00+00:00'"
        );
        let zone = ParamType::Datetime {
            tz: Some("Europe/Berlin".into()),
        };
        assert_eq!(
            lit(Postgres, zone.clone(), dt.clone()),
            "(TIMESTAMP '2026-11-01 08:00:00' AT TIME ZONE 'Europe/Berlin')"
        );
        assert_eq!(
            lit(Oracle, zone.clone(), dt.clone()),
            "TIMESTAMP '2026-11-01 08:00:00 Europe/Berlin'"
        );
        assert!(matches!(
            render_literal(SqlServer, &def("p", zone), &dt, None),
            Err(BindError::TimezoneUnsupported { .. })
        ));
        let hostile = ParamType::Datetime {
            tz: Some("x' OR 1=1 --".into()),
        };
        assert!(render_literal(Postgres, &def("p", hostile), &dt, None).is_err());
        assert_eq!(
            lit(
                SqlServer,
                ParamType::MultiSelect,
                ParamValue::List(vec![ParamValue::Text("a".into()), ParamValue::Integer(2)])
            ),
            "(N'a', 2)"
        );
        assert_eq!(
            lit(Postgres, ParamType::MultiSelect, ParamValue::List(vec![])),
            "(NULL)"
        );
        assert!(matches!(
            render_literal(
                SqlServer,
                &def("k", ParamType::SecretRef),
                &ParamValue::SecretRef("k".into()),
                None
            ),
            Err(BindError::SecretNeedsNativeBinding { .. })
        ));
    }

    #[test]
    fn date_ranges_bind_through_fields() {
        let d = def("period", ParamType::DateRange);
        let v = ParamValue::DateRange {
            start: date(2026, 1, 1),
            end: date(2026, 1, 31),
        };
        let db = DatabaseType::Postgres;
        assert_eq!(
            render_literal(db, &d, &v, Some("start")).unwrap(),
            "DATE '2026-01-01'"
        );
        assert_eq!(
            render_literal(db, &d, &v, Some("end")).unwrap(),
            "DATE '2026-01-31'"
        );
        assert_eq!(
            render_literal(db, &d, &v, Some("end_exclusive")).unwrap(),
            "DATE '2026-02-01'"
        );
        assert!(matches!(
            render_literal(db, &d, &v, None),
            Err(BindError::NeedsField { .. })
        ));
        assert!(matches!(
            render_literal(db, &d, &v, Some("mid")),
            Err(BindError::UnknownField { .. })
        ));
        let sql = "SELECT * FROM t WHERE d >= {{param.period.start}} AND d < {{ param.period.end_exclusive }}";
        let bound = bind_sql(db, sql, &[d], &resolved(&[("period", v)])).unwrap();
        assert_eq!(
            bound.sql,
            "SELECT * FROM t WHERE d >= DATE '2026-01-01' AND d < DATE '2026-02-01'"
        );
        assert_eq!(bound.used, vec!["period"]);
    }

    #[test]
    fn binding_replaces_only_declared_names_in_code() {
        let defs = vec![
            def("id", ParamType::Integer),
            def("name", ParamType::String),
        ];
        let values = resolved(&[
            ("id", ParamValue::Integer(7)),
            ("name", ParamValue::Text("x'y".into())),
        ]);
        let sql = "SELECT a::int, b[1:2], ':id', \"@id\" /* :id */ -- @id\nFROM t WHERE id = :id AND n = @NAME AND z = @idx AND w = @@id AND q = {{param.id}} AND r = :other";
        let bound = bind_sql(DatabaseType::Postgres, sql, &defs, &values).unwrap();
        assert_eq!(
            bound.sql,
            "SELECT a::int, b[1:2], ':id', \"@id\" /* :id */ -- @id\nFROM t WHERE id = 7 AND n = 'x''y' AND z = @idx AND w = @@id AND q = 7 AND r = :other"
        );
        assert_eq!(bound.used, vec!["id", "name"]);
        let tsql = "SELECT [:id], N'@id', @id FROM t";
        let bound = bind_sql(DatabaseType::SqlServer, tsql, &defs, &values).unwrap();
        assert_eq!(bound.sql, "SELECT [:id], N'@id', 7 FROM t");
        let pg_body = "SELECT $$ :id $$, :id";
        assert_eq!(
            bind_sql(DatabaseType::Postgres, pg_body, &defs, &values)
                .unwrap()
                .sql,
            "SELECT $$ :id $$, 7"
        );
    }

    #[test]
    fn binding_reports_missing_and_invalid_before_rewriting() {
        let defs = vec![def("a", ParamType::Integer), def("b", ParamType::Integer)];
        let mut values = resolved(&[("a", ParamValue::Integer(1))]);
        values.errors.insert(
            "b".into(),
            ParamError::InvalidValue {
                expected: "integer".into(),
            },
        );
        let err = bind_sql(DatabaseType::SqlServer, "SELECT :a, :b", &defs, &values).unwrap_err();
        assert!(matches!(err, BindError::Param { ref name, .. } if name == "b"));
        let err = bind_sql(
            DatabaseType::SqlServer,
            "SELECT :b",
            &defs[1..],
            &resolved(&[]),
        )
        .unwrap_err();
        assert_eq!(
            err,
            BindError::Param {
                name: "b".into(),
                error: ParamError::Missing
            }
        );
        let secret = vec![def("token", ParamType::SecretRef)];
        let err = bind_sql(
            DatabaseType::SqlServer,
            "SELECT :token",
            &secret,
            &resolved(&[("token", ParamValue::SecretRef("k".into()))]),
        )
        .unwrap_err();
        assert!(matches!(err, BindError::SecretNeedsNativeBinding { .. }));
        assert_eq!(
            err.message().key(),
            Some("notebook.core.bind.secret_needs_native_binding")
        );
    }

    #[test]
    fn hostile_text_values_never_escape_their_literal() {
        let defs = vec![def("v", ParamType::String)];
        let hostile = [
            "'; DROP TABLE t; --",
            "\\'; DROP TABLE t; --",
            "x\\",
            "O''Brien",
            "*/ DROP /*",
            "$$; DROP TABLE t; $$",
            "\u{0}",
            "\n--",
            "`]\"",
        ];
        for db in [
            DatabaseType::SqlServer,
            DatabaseType::Postgres,
            DatabaseType::MySql,
            DatabaseType::Sqlite,
            DatabaseType::Oracle,
        ] {
            for text in hostile {
                let values = resolved(&[("v", ParamValue::Text(text.into()))]);
                let bound = bind_sql(db, "SELECT :v AS x", &defs, &values).unwrap();
                let masked = mask_sql(db, &bound.sql);
                assert_eq!(
                    masked.split_whitespace().collect::<Vec<_>>(),
                    vec!["SELECT", "AS", "x"],
                    "{db:?} {text:?} -> {}",
                    bound.sql
                );
            }
        }
    }
}
