//! Parameter cells and the parameter features built on
//! [`crate::notebook::params`]: the cell's spec, options filled by a
//! read-only discovery query, linked parameters, named parameter sets,
//! personal values kept outside the notebook file, shareable state,
//! `{{param.name}}` in Markdown, and which cells a change re-runs at once
//! versus only after Apply.
//!
//! Definitions stay F1 [`ParameterDef`]s; the discovery query lives in the
//! definition's `options_query` field and named sets in the notebook's
//! `parameter_sets` field, both kept by F1's unknown-field round trip.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_lang::DisplayMessage;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::notebook::document::{Cell, ExecClass, Extra, NotebookDoc, SourceLanguage, ser_extra};
use crate::notebook::graph::{Graph, Node};
use crate::notebook::params::{
    self, AllowedValues, BindError, Clock, ParamError, ParamScope, ParamType, ParamValue,
    ParameterDef, ResolvedParams,
};
use crate::notebook::references::{self, Dialects, NameError, RefTarget, find_references};
use crate::query::{QueryEvent, QueryExecutor};

/// The kind id of parameter cells.
pub const KIND: &str = "parameter";
/// Where a definition keeps its discovery query.
pub const OPTIONS_QUERY_KEY: &str = "options_query";
/// Where the notebook keeps its shared named parameter sets.
pub const SETS_KEY: &str = "parameter_sets";
/// Rows a discovery query reads unless it asks for fewer.
pub const DEFAULT_OPTIONS_LIMIT: u32 = 1_000;
/// The most rows a discovery query may ask for.
pub const MAX_OPTIONS_LIMIT: u32 = 10_000;
/// Shown instead of a sensitive or secret value.
pub const MASK: &str = "••••••";
/// The text an entity lookup is searching for, inside its query.
const SEARCH_REFERENCE: &str = "search";

/// A parameter cell's `spec`: the notebook parameter it shows.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ParameterCellSpec {
    #[serde(default)]
    pub parameter: String,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

impl ParameterCellSpec {
    pub fn for_parameter(name: impl Into<String>) -> Self {
        Self {
            parameter: name.into(),
            extra: Extra::new(),
        }
    }

    /// The spec of `cell`; empty when it has none or it does not parse.
    pub fn of(cell: &Cell) -> Self {
        serde_json::from_value(cell.spec.clone()).unwrap_or_default()
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// Whether a type picks from a list of options.
pub fn takes_options(ty: &ParamType) -> bool {
    matches!(
        ty,
        ParamType::Enum | ParamType::MultiSelect | ParamType::EntityLookup | ParamType::Search
    )
}

/// A discovery query that fills a parameter's options: the first column
/// (or `value_column`) is the value, the second (or `label_column`) the
/// label. It runs read-only and reads at most [`OptionsQuery::limit`] rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionsQuery {
    pub sql: String,
    /// The notebook connection slot it runs on; the notebook's default
    /// connection when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_column: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

impl OptionsQuery {
    pub fn new(sql: impl Into<String>) -> Self {
        Self {
            sql: sql.into(),
            connection_ref: None,
            value_column: None,
            label_column: None,
            limit: None,
            extra: Extra::new(),
        }
    }

    /// `def`'s discovery query, if it has a non-empty one.
    pub fn of(def: &ParameterDef) -> Option<Self> {
        def.extra
            .get(OPTIONS_QUERY_KEY)
            .and_then(|v| serde_json::from_value::<Self>(v.clone()).ok())
            .filter(|q| !q.sql.trim().is_empty())
    }

    /// Set or clear `def`'s discovery query.
    pub fn store(def: &mut ParameterDef, query: Option<OptionsQuery>) {
        match query.filter(|q| !q.sql.trim().is_empty()) {
            Some(q) => {
                if let Ok(value) = serde_json::to_value(&q) {
                    def.extra.insert(OPTIONS_QUERY_KEY.to_string(), value);
                }
            }
            None => {
                def.extra.remove(OPTIONS_QUERY_KEY);
            }
        }
    }

    /// The row limit, clamped to `1..=MAX_OPTIONS_LIMIT`.
    pub fn limit(&self) -> u32 {
        self.limit
            .unwrap_or(DEFAULT_OPTIONS_LIMIT)
            .clamp(1, MAX_OPTIONS_LIMIT)
    }

    /// Whether the query reads the text being searched (`{{search}}`), so it
    /// re-runs as the person types instead of filtering loaded options.
    pub fn searches(&self, db: DatabaseType) -> bool {
        !search_ranges(db, &self.sql).is_empty()
    }
}

fn search_ranges(db: DatabaseType, sql: &str) -> Vec<(usize, usize)> {
    find_references(sql, SourceLanguage::Sql, db)
        .into_iter()
        .filter(
            |r| matches!(&r.target, RefTarget::Cell(t) if t.eq_ignore_ascii_case(SEARCH_REFERENCE)),
        )
        .map(|r| (r.start, r.end))
        .collect()
}

/// One choice a person can pick.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionItem {
    pub value: Value,
    pub label: String,
}

/// Text of a JSON scalar as a person reads it.
pub fn value_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The choices written into the definition: its allow-list for an
/// identifier, else its static allowed values.
pub fn static_options(def: &ParameterDef) -> Vec<OptionItem> {
    if let ParamType::Identifier { allowlist } = &def.ty {
        return allowlist
            .iter()
            .map(|name| OptionItem {
                value: Value::String(name.clone()),
                label: name.clone(),
            })
            .collect();
    }
    match &def.allowed_values {
        Some(AllowedValues::Static(values)) => values
            .iter()
            .map(|v| match v {
                Value::Object(map) => {
                    let value = map.get("value").cloned().unwrap_or(Value::Null);
                    let label = map
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .filter(|l| !l.is_empty())
                        .unwrap_or_else(|| value_text(&value));
                    OptionItem { value, label }
                }
                other => OptionItem {
                    value: other.clone(),
                    label: value_text(other),
                },
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The typed value of picking `values` for `def` (one value, or several for
/// a multi-select).
pub fn choice_value(def: &ParameterDef, values: &[Value]) -> Result<ParamValue, ParamError> {
    let value = match def.ty {
        ParamType::MultiSelect => params::from_json(def, &Value::Array(values.to_vec()))?,
        _ => match values.first() {
            Some(v) => params::from_json(def, v)?,
            None => ParamValue::Null,
        },
    };
    params::validate(def, &value)?;
    Ok(value)
}

/// Why a discovery query cannot run or its rows cannot be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OptionsError {
    /// Only a single read-only query may fill options.
    NotReadOnly,
    /// The query reads the parameter it fills.
    SelfReference,
    /// Another parameter it reads has no usable value.
    Bind(BindError),
    /// The database refused or failed the query.
    Query(String),
    NoColumns,
    UnknownColumn {
        column: String,
    },
}

impl OptionsError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            OptionsError::NotReadOnly => DisplayMessage::new(
                "notebook.params.options.not_read_only",
                "Options can only come from a single read-only query",
            ),
            OptionsError::SelfReference => DisplayMessage::new(
                "notebook.params.options.self_reference",
                "The options query cannot read the parameter it fills",
            ),
            OptionsError::Bind(error) => error.message(),
            OptionsError::Query(error) => DisplayMessage::new(
                "notebook.params.options.query_failed",
                format!("The options query failed: {error}"),
            )
            .with_arg("error", error.clone()),
            OptionsError::NoColumns => DisplayMessage::new(
                "notebook.params.options.no_columns",
                "The options query returned no columns",
            ),
            OptionsError::UnknownColumn { column } => DisplayMessage::new(
                "notebook.params.options.unknown_column",
                format!("The options query has no column named {column}"),
            )
            .with_arg("column", column.clone()),
        }
    }
}

/// Parameters `query` reads, other than `own`, in first-use order.
pub fn linked_parameters(
    db: DatabaseType,
    query: &OptionsQuery,
    defs: &[ParameterDef],
    own: &str,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in params::find_param_uses(db, &query.sql, defs) {
        if !u.name.eq_ignore_ascii_case(own) && !out.contains(&u.name) {
            out.push(u.name);
        }
    }
    out
}

/// Parameters whose options must be read again when `changed` change:
/// those whose query reads one of them, transitively, in declaration order.
pub fn linked_dependents(
    db: DatabaseType,
    defs: &[ParameterDef],
    changed: &[String],
) -> Vec<String> {
    let mut hit: BTreeSet<String> = changed.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut out: Vec<String> = Vec::new();
    loop {
        let mut grew = false;
        for def in defs {
            if out.contains(&def.name) {
                continue;
            }
            let Some(query) = OptionsQuery::of(def) else {
                continue;
            };
            if linked_parameters(db, &query, defs, &def.name)
                .iter()
                .any(|p| hit.contains(&p.to_ascii_lowercase()))
            {
                hit.insert(def.name.to_ascii_lowercase());
                out.push(def.name.clone());
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    let order: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    out.sort_by_key(|name| order.iter().position(|n| n == name));
    out
}

/// A discovery query ready to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundOptions {
    pub sql: String,
    /// The other parameters it read.
    pub linked: Vec<String>,
    pub limit: u32,
}

/// Bind `def`'s discovery query for `db`: other parameters become literals,
/// `{{search}}` becomes the text searched for, and anything but one
/// read-only query is refused.
pub fn bind_options(
    db: DatabaseType,
    def: &ParameterDef,
    query: &OptionsQuery,
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
    search: &str,
) -> Result<BoundOptions, OptionsError> {
    if params::find_param_uses(db, &query.sql, defs)
        .iter()
        .any(|u| u.name.eq_ignore_ascii_case(&def.name))
    {
        return Err(OptionsError::SelfReference);
    }
    let mut sql = String::with_capacity(query.sql.len());
    let mut last = 0;
    for (start, end) in search_ranges(db, &query.sql) {
        sql.push_str(&query.sql[last..start]);
        sql.push_str(&db.string_literal(search));
        last = end;
    }
    sql.push_str(&query.sql[last..]);
    let others: Vec<ParameterDef> = defs
        .iter()
        .filter(|d| !d.name.eq_ignore_ascii_case(&def.name))
        .cloned()
        .collect();
    let bound = params::bind_sql(db, &sql, &others, resolved).map_err(OptionsError::Bind)?;
    if !sqlly_intel::query::safety::classify_select_only_for(db, &bound.sql).select_only {
        return Err(OptionsError::NotReadOnly);
    }
    Ok(BoundOptions {
        sql: bound.sql,
        linked: bound.used,
        limit: query.limit(),
    })
}

/// The cache key of a bound discovery query on `connection` (an opaque
/// profile id and database, never credentials).
pub fn options_fingerprint(bound: &BoundOptions, connection: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [
        "sqlly.notebook.options.v1",
        connection,
        &bound.sql,
        &bound.limit.to_string(),
    ] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Options read from a query.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OptionsPage {
    pub items: Vec<OptionItem>,
    /// More rows than the limit came back; only the first ones are kept.
    pub truncated: bool,
}

fn cell_json(cell: &CellValue) -> Option<Value> {
    match cell {
        CellValue::Null | CellValue::Bytes(_) => None,
        CellValue::Bool(b) => Some(Value::Bool(*b)),
        CellValue::I64(n) => Some(Value::from(*n)),
        CellValue::F64(n) => serde_json::Number::from_f64(*n).map(Value::Number),
        CellValue::String(s) => Some(Value::String(s.clone())),
    }
}

/// Reads options from rows as they arrive, distinct values only, stopping
/// at the limit.
#[derive(Debug)]
pub struct OptionsCollector {
    value_ix: usize,
    label_ix: Option<usize>,
    limit: usize,
    seen: BTreeSet<String>,
    page: OptionsPage,
}

impl OptionsCollector {
    pub fn new(columns: &[String], query: &OptionsQuery) -> Result<Self, OptionsError> {
        if columns.is_empty() {
            return Err(OptionsError::NoColumns);
        }
        let find = |name: &str| {
            columns
                .iter()
                .position(|c| c.eq_ignore_ascii_case(name))
                .ok_or_else(|| OptionsError::UnknownColumn {
                    column: name.to_string(),
                })
        };
        let value_ix = match &query.value_column {
            Some(name) => find(name)?,
            None => 0,
        };
        let label_ix = match &query.label_column {
            Some(name) => Some(find(name)?),
            None => (columns.len() > 1).then_some(if value_ix == 0 { 1 } else { 0 }),
        };
        Ok(Self {
            value_ix,
            label_ix,
            limit: query.limit() as usize,
            seen: BTreeSet::new(),
            page: OptionsPage::default(),
        })
    }

    /// Add one row; returns `true` once the limit is passed and reading
    /// should stop.
    pub fn push(&mut self, row: &[CellValue]) -> bool {
        let Some(value) = row.get(self.value_ix).and_then(cell_json) else {
            return false;
        };
        let key = value.to_string();
        if self.seen.contains(&key) {
            return false;
        }
        if self.page.items.len() >= self.limit {
            self.page.truncated = true;
            return true;
        }
        let label = self
            .label_ix
            .and_then(|ix| row.get(ix))
            .and_then(cell_json)
            .map(|v| value_text(&v))
            .filter(|l| !l.is_empty())
            .unwrap_or_else(|| value_text(&value));
        self.seen.insert(key);
        self.page.items.push(OptionItem { value, label });
        false
    }

    pub fn finish(self) -> OptionsPage {
        self.page
    }
}

/// Run a bound discovery query and read its options. The query always runs
/// under the read-only mode (SELECT-only gate plus the database's own
/// read-only enforcement) on a clone of `profile`, whatever `profile` says,
/// and stops reading after the limit.
pub async fn collect_options(
    queries: &dyn QueryExecutor,
    profile: &ConnectionProfile,
    bound: &BoundOptions,
    query: &OptionsQuery,
) -> Result<OptionsPage, OptionsError> {
    let profile = read_only_profile(profile);
    sqlly_intel::query::safety::check_connection_policy_for(&profile, &bound.sql)
        .map_err(|_| OptionsError::NotReadOnly)?;
    // An option's value is bound into later queries, so it must be whole,
    // never a grid preview.
    let handle = queries
        .execute_full_values(&profile, None, &bound.sql, 256)
        .await
        .map_err(|e| OptionsError::Query(e.to_string()))?;
    let task_id = handle.task_id.clone();
    let mut events = handle.events;
    let mut limited = query.clone();
    limited.limit = Some(bound.limit);
    let mut collector: Option<OptionsCollector> = None;
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::ResultSet { index: 0, columns } => {
                let names: Vec<String> = columns.into_iter().map(|c| c.name).collect();
                collector = Some(OptionsCollector::new(&names, &limited)?);
            }
            QueryEvent::Rows { index: 0, rows } => {
                let Some(c) = collector.as_mut() else {
                    continue;
                };
                if rows.iter().any(|row| c.push(row)) {
                    let _ = queries.cancel(&task_id).await;
                    break;
                }
            }
            QueryEvent::ResultSet { .. } | QueryEvent::Rows { .. } => {
                let _ = queries.cancel(&task_id).await;
                break;
            }
            QueryEvent::Error { message } => return Err(OptionsError::Query(message)),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    collector
        .map(OptionsCollector::finish)
        .ok_or(OptionsError::NoColumns)
}

/// `profile` with the read-only mode every notebook discovery query runs
/// under.
pub fn read_only_profile(profile: &ConnectionProfile) -> ConnectionProfile {
    let mut profile = profile.clone();
    profile.block_all_but_select = true;
    profile.enforce_read_only = true;
    profile
}

/// Read a stored value (a default, a set entry, a personal value): a value
/// in [`ParamValue::to_json`] form or `{"preset": id}`, validated.
pub fn value_from_json(
    def: &ParameterDef,
    value: &Value,
    clock: &dyn Clock,
) -> Result<ParamValue, ParamError> {
    let parsed = match value {
        Value::Object(map) if map.len() == 1 && map.contains_key("preset") => {
            let id = map["preset"].as_str().unwrap_or_default();
            let preset = params::find_preset(def, id).ok_or_else(|| ParamError::UnknownPreset {
                preset: id.to_string(),
            })?;
            params::resolve_preset(def, &preset, clock)?
        }
        other => params::from_json(def, other)?,
    };
    params::validate(def, &parsed)?;
    Ok(parsed)
}

/// A named set of parameter values (a tenant, a region and a period, say).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParameterSet {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, serialize_with = "ser_extra")]
    pub values: Extra,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

/// Where a named set is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetTarget {
    /// In the notebook, for everyone.
    Shared,
    /// In this person's side file.
    Personal,
}

/// The notebook's shared sets; entries that do not parse are skipped.
pub fn shared_sets(doc: &NotebookDoc) -> Vec<ParameterSet> {
    doc.extra
        .get(SETS_KEY)
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

pub fn store_shared_sets(doc: &mut NotebookDoc, sets: &[ParameterSet]) {
    let values: Vec<Value> = sets
        .iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .collect();
    if values.is_empty() {
        doc.extra.remove(SETS_KEY);
    } else {
        doc.extra.insert(SETS_KEY.to_string(), Value::Array(values));
    }
}

/// Capture `values` as a named set. Sensitive and secret parameters are
/// never captured, and a shared set leaves out personal parameters.
pub fn capture_set(
    id: impl Into<String>,
    name: impl Into<String>,
    defs: &[ParameterDef],
    values: &BTreeMap<String, ParamValue>,
    target: SetTarget,
) -> ParameterSet {
    let mut captured = Extra::new();
    for def in defs {
        if def.is_redacted() || (target == SetTarget::Shared && def.scope == ParamScope::Personal) {
            continue;
        }
        if let Some((_, value)) = values
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(&def.name))
        {
            captured.insert(def.name.clone(), value.to_json());
        }
    }
    ParameterSet {
        id: id.into(),
        name: name.into(),
        values: captured,
        extra: Extra::new(),
    }
}

/// The values a set gives, and the entries that no longer fit their
/// parameter. Unknown names are ignored and sensitive parameters are never
/// filled from a stored set.
pub fn set_values(
    defs: &[ParameterDef],
    set: &ParameterSet,
    clock: &dyn Clock,
) -> (BTreeMap<String, ParamValue>, Vec<(String, ParamError)>) {
    let mut values = BTreeMap::new();
    let mut errors = Vec::new();
    for (name, raw) in &set.values {
        let Some(def) = defs
            .iter()
            .find(|d| d.name.eq_ignore_ascii_case(name) && !d.is_redacted())
        else {
            continue;
        };
        match value_from_json(def, raw, clock) {
            Ok(value) => {
                values.insert(def.name.clone(), value);
            }
            Err(error) => errors.push((def.name.clone(), error)),
        }
    }
    (values, errors)
}

/// What one person keeps for a notebook outside its file: values of
/// personal parameters, the secret each secret parameter names (secrets
/// live in this machine's keyring, so the pick is per machine), and
/// personal named sets.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct PersonalParams {
    #[serde(default)]
    pub notebook_id: String,
    #[serde(default, serialize_with = "ser_extra")]
    pub values: Extra,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sets: Vec<ParameterSet>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

impl PersonalParams {
    pub fn new(notebook_id: impl Into<String>) -> Self {
        Self {
            notebook_id: notebook_id.into(),
            ..Self::default()
        }
    }

    /// Keep (or clear) `def`'s value. Sensitive values are never kept; a
    /// secret parameter keeps only the secret's name. Returns whether
    /// anything changed.
    pub fn set_value(&mut self, def: &ParameterDef, value: Option<&ParamValue>) -> bool {
        let next = value.filter(|_| !def.sensitive).map(ParamValue::to_json);
        let before = self.values.get(&def.name).cloned();
        match next.clone() {
            Some(v) => {
                self.values.insert(def.name.clone(), v);
            }
            None => {
                self.values.remove(&def.name);
            }
        }
        before != next
    }

    /// Whether `def`'s value is kept here rather than in the notebook.
    pub fn keeps(def: &ParameterDef) -> bool {
        !def.sensitive && (def.scope == ParamScope::Personal || def.ty == ParamType::SecretRef)
    }

    /// The kept values that still fit their parameters.
    pub fn values_for(
        &self,
        defs: &[ParameterDef],
        clock: &dyn Clock,
    ) -> BTreeMap<String, ParamValue> {
        defs.iter()
            .filter(|d| Self::keeps(d))
            .filter_map(|d| {
                let raw = self.values.get(&d.name)?;
                value_from_json(d, raw, clock)
                    .ok()
                    .map(|v| (d.name.clone(), v))
            })
            .collect()
    }

    /// Pretty JSON with a trailing newline.
    pub fn to_json(&self) -> String {
        let mut text = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        text.push('\n');
        text
    }
}

/// Values that may go into a shareable link: shareable, non-sensitive
/// parameters only.
pub fn shareable_values(
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
) -> BTreeMap<String, Value> {
    defs.iter()
        .filter(|d| d.shareable && !d.is_redacted())
        .filter_map(|d| resolved.get(&d.name).map(|v| (d.name.clone(), v.to_json())))
        .collect()
}

fn date_text(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// A value as text for people (`field` picks a date range end), or `None`
/// when the field does not apply.
pub fn display_value(value: &ParamValue, field: Option<&str>) -> Option<String> {
    if let ParamValue::DateRange { start, end } = value {
        return match field {
            None => Some(format!("{} – {}", date_text(*start), date_text(*end))),
            Some("start") => Some(date_text(*start)),
            Some("end") => Some(date_text(*end)),
            Some("end_exclusive") => Some(date_text(*end + chrono::Duration::days(1))),
            Some(_) => None,
        };
    }
    if field.is_some() {
        return None;
    }
    Some(match value {
        ParamValue::Null => "NULL".to_string(),
        ParamValue::Text(s) | ParamValue::Identifier(s) | ParamValue::Decimal(s) => s.clone(),
        ParamValue::Integer(n) => n.to_string(),
        ParamValue::Boolean(b) => b.to_string(),
        ParamValue::Date(d) => date_text(*d),
        ParamValue::DateTime(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        ParamValue::List(items) => items
            .iter()
            .filter_map(|item| display_value(item, None))
            .collect::<Vec<_>>()
            .join(", "),
        ParamValue::SecretRef(_) => MASK.to_string(),
        ParamValue::DateRange { .. } => unreachable!(),
    })
}

/// `text` with Markdown punctuation escaped and line breaks folded, so a
/// value can never start a heading, a link or an HTML tag.
pub fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\r' | '\n' => out.push(' '),
            c if c.is_ascii_punctuation() => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// What `{{param.name}}` (or `.start`, `.end`, …) shows in Markdown: the
/// escaped value, the mask for a sensitive or secret parameter, or `None`
/// when the parameter or its value or field is missing.
pub fn param_text(
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
    name: &str,
    field: Option<&str>,
) -> Option<String> {
    let def = defs.iter().find(|d| d.name.eq_ignore_ascii_case(name))?;
    let value = resolved.get(&def.name)?;
    if def.is_redacted() {
        return Some(MASK.to_string());
    }
    display_value(value, field).map(|s| escape_markdown(&s))
}

/// Markdown `text` with each `{{param.name}}` (outside code) replaced by the
/// parameter's current value, escaped; sensitive and secret values are
/// masked. References without a value are left as written.
pub fn interpolate_markdown(
    text: &str,
    defs: &[ParameterDef],
    resolved: &ResolvedParams,
) -> String {
    let refs = find_references(text, SourceLanguage::Markdown, DatabaseType::default());
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for r in refs {
        let RefTarget::Param { name, field } = &r.target else {
            continue;
        };
        let shown = param_text(defs, resolved, name, field.as_deref());
        if let Some(shown) = shown {
            out.push_str(&text[last..r.start]);
            out.push_str(&shown);
            last = r.end;
        }
    }
    out.push_str(&text[last..]);
    out
}

/// What a parameter change re-runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReactivePlan {
    /// Local cells that re-run straight away in reactive mode.
    pub run_now: Vec<String>,
    /// Cells that wait for Apply: queries on a connection, and cells that
    /// read from one.
    pub after_apply: Vec<String>,
}

/// Split the cells that depend on `changed` parameters into those cheap
/// enough to re-run at once and those that wait for Apply.
pub fn reactive_plan(doc: &NotebookDoc, graph: &Graph, changed: &[String]) -> ReactivePlan {
    let affected: BTreeSet<Node> = changed
        .iter()
        .filter_map(|name| doc.parameter(name))
        .flat_map(|def| graph.downstream_closure(&Node::param(def.name.clone())))
        .collect();
    let mut gated: BTreeSet<String> = BTreeSet::new();
    let mut plan = ReactivePlan::default();
    for node in graph.in_order(affected) {
        let Some(id) = node.as_cell() else {
            continue;
        };
        let Some(cell) = doc.cell(id) else {
            continue;
        };
        let behind_gate = graph
            .upstream(&node)
            .iter()
            .any(|(up, _)| up.as_cell().is_some_and(|u| gated.contains(u)));
        let class = if super::local::targets_local(cell) {
            ExecClass::Local
        } else {
            cell.kind.exec_class()
        };
        match class {
            ExecClass::Remote => {
                gated.insert(id.to_string());
                plan.after_apply.push(id.to_string());
            }
            ExecClass::Local if behind_gate => {
                gated.insert(id.to_string());
                plan.after_apply.push(id.to_string());
            }
            ExecClass::Local => plan.run_now.push(id.to_string()),
            ExecClass::Instant if behind_gate => {
                gated.insert(id.to_string());
            }
            ExecClass::Instant | ExecClass::Unsupported => {}
        }
    }
    plan
}

/// Rename parameter `old` to `new` everywhere: SQL and Markdown (F1's
/// rename), parameter cells that show it, and other parameters' discovery
/// queries. Returns the ids of the cells changed.
pub fn rename_parameter(
    doc: &mut NotebookDoc,
    old: &str,
    new: &str,
    dialects: &Dialects,
) -> Result<Vec<String>, NameError> {
    let defs_before = doc.parameters.clone();
    let mut changed = references::rename_parameter(doc, old, new, dialects)?;
    for cell in &mut doc.cells {
        if cell.kind.as_str() != KIND {
            continue;
        }
        let mut spec = ParameterCellSpec::of(cell);
        if spec.parameter.eq_ignore_ascii_case(old) {
            spec.parameter = new.to_string();
            cell.spec = spec.to_value();
            if !changed.contains(&cell.id) {
                changed.push(cell.id.clone());
            }
        }
    }
    let db = dialects.default;
    for def in &mut doc.parameters {
        let Some(mut query) = OptionsQuery::of(def) else {
            continue;
        };
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        for u in params::find_param_uses(db, &query.sql, &defs_before) {
            if !u.name.eq_ignore_ascii_case(old) {
                continue;
            }
            let text = match (u.style, &u.field) {
                (params::ParamStyle::Colon, _) => format!(":{new}"),
                (params::ParamStyle::At, _) => format!("@{new}"),
                (params::ParamStyle::Template, Some(field)) => {
                    format!("{{{{param.{new}.{field}}}}}")
                }
                (params::ParamStyle::Template, None) => format!("{{{{param.{new}}}}}"),
            };
            edits.push((u.start, u.end, text));
        }
        if edits.is_empty() {
            continue;
        }
        for (start, end, text) in edits.into_iter().rev() {
            query.sql.replace_range(start..end, &text);
        }
        OptionsQuery::store(def, Some(query));
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CellKind, SeededIds};
    use crate::notebook::params::{DatePreset, FixedClock, Preset, Reactivity, SystemClock};
    use chrono::NaiveDate;

    fn doc_with(cells: &[(CellKind, &str)]) -> NotebookDoc {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(7));
        doc.cells.clear();
        let mut ids = SeededIds(11);
        for (kind, source) in cells {
            let i = doc.add_cell(kind.clone(), None, &mut ids);
            doc.cells[i].source = source.to_string();
        }
        doc
    }

    fn clock() -> FixedClock {
        FixedClock(
            NaiveDate::from_ymd_opt(2026, 11, 18)
                .unwrap()
                .and_hms_opt(9, 30, 0)
                .unwrap(),
        )
    }

    fn enum_def(name: &str, sql: &str) -> ParameterDef {
        let mut def = ParameterDef::new(name, ParamType::Enum);
        OptionsQuery::store(&mut def, Some(OptionsQuery::new(sql)));
        def
    }

    #[test]
    fn cell_spec_round_trips_and_keeps_unknown_fields() {
        let mut doc = doc_with(&[(CellKind::Parameter, "")]);
        doc.cells[0].spec = serde_json::json!({"parameter": "region", "layout": "wide"});
        let spec = ParameterCellSpec::of(&doc.cells[0]);
        assert_eq!(spec.parameter, "region");
        assert_eq!(spec.to_value(), doc.cells[0].spec);
        doc.cells[0].spec = Value::Null;
        assert_eq!(
            ParameterCellSpec::of(&doc.cells[0]),
            ParameterCellSpec::default()
        );
    }

    #[test]
    fn options_query_lives_in_the_definition_and_round_trips_through_the_file() {
        let mut doc = doc_with(&[]);
        let mut def = enum_def("region", "SELECT code, name FROM regions");
        let mut q = OptionsQuery::of(&def).unwrap();
        q.limit = Some(50_000);
        q.connection_ref = Some("conn_1".into());
        OptionsQuery::store(&mut def, Some(q));
        doc.parameters.push(def);
        let text = doc.to_canonical_json();
        let back = crate::notebook::load(None, &text, &mut SeededIds(1), None).unwrap();
        let q = OptionsQuery::of(&back.doc.parameters[0]).unwrap();
        assert_eq!(q.sql, "SELECT code, name FROM regions");
        assert_eq!(q.connection_ref.as_deref(), Some("conn_1"));
        assert_eq!(q.limit(), MAX_OPTIONS_LIMIT);
        assert_eq!(OptionsQuery::new("SELECT 1").limit(), DEFAULT_OPTIONS_LIMIT);
        let mut cleared = back.doc.parameters[0].clone();
        OptionsQuery::store(&mut cleared, None);
        assert!(OptionsQuery::of(&cleared).is_none());
        assert!(!cleared.extra.contains_key(OPTIONS_QUERY_KEY));
    }

    #[test]
    fn options_queries_must_be_read_only_and_cannot_read_their_own_parameter() {
        let db = DatabaseType::SqlServer;
        let region = ParameterDef::new("region", ParamType::String);
        let defs = vec![region.clone(), enum_def("city", "")];
        let resolved = params::resolve_all(
            &defs,
            &BTreeMap::from([("region".to_string(), ParamValue::Text("EU".into()))]),
            &SystemClock,
        );
        let bind = |sql: &str| {
            let def = enum_def("city", sql);
            let q = OptionsQuery::of(&def).unwrap();
            bind_options(db, &def, &q, &defs, &resolved, "")
        };
        assert_eq!(
            bind("DELETE FROM cities").unwrap_err(),
            OptionsError::NotReadOnly
        );
        assert_eq!(
            bind("SELECT 1; DROP TABLE cities").unwrap_err(),
            OptionsError::NotReadOnly
        );
        assert_eq!(
            bind("SELECT name FROM cities WHERE name = :city").unwrap_err(),
            OptionsError::SelfReference
        );
        let ok = bind("SELECT name FROM cities WHERE region = {{param.region}}").unwrap();
        assert_eq!(ok.sql, "SELECT name FROM cities WHERE region = N'EU'");
        assert_eq!(ok.linked, ["region"]);
        assert_eq!(ok.limit, DEFAULT_OPTIONS_LIMIT);
    }

    #[test]
    fn lookups_bind_the_search_text_as_a_literal() {
        let db = DatabaseType::Postgres;
        let mut def = ParameterDef::new("customer", ParamType::EntityLookup);
        OptionsQuery::store(
            &mut def,
            Some(OptionsQuery::new(
                "SELECT id, name FROM customers WHERE name ILIKE '%' || {{search}} || '%' -- {{search}}",
            )),
        );
        let q = OptionsQuery::of(&def).unwrap();
        assert!(q.searches(db));
        let bound = bind_options(
            db,
            &def,
            &q,
            std::slice::from_ref(&def),
            &ResolvedParams::default(),
            "O'Brien",
        )
        .unwrap();
        assert!(
            bound.sql.contains("'%' || 'O''Brien' || '%'"),
            "{}",
            bound.sql
        );
        assert!(bound.sql.ends_with("-- {{search}}"));
        assert!(!OptionsQuery::new("SELECT 1").searches(db));
    }

    #[test]
    fn collector_keeps_distinct_values_labels_and_stops_at_the_limit() {
        let mut q = OptionsQuery::new("SELECT id, name FROM t");
        q.limit = Some(3);
        let mut c = OptionsCollector::new(&["ID".into(), "Name".into()], &q).unwrap();
        let row = |id: i64, name: &str| vec![CellValue::I64(id), CellValue::String(name.into())];
        assert!(!c.push(&row(1, "One")));
        assert!(!c.push(&row(1, "Again")));
        assert!(!c.push(&[CellValue::Null, CellValue::String("skip".into())]));
        assert!(!c.push(&row(2, "")));
        assert!(!c.push(&row(3, "Three")));
        assert!(c.push(&row(4, "Four")));
        let page = c.finish();
        assert!(page.truncated);
        assert_eq!(
            page.items,
            vec![
                OptionItem {
                    value: Value::from(1),
                    label: "One".into()
                },
                OptionItem {
                    value: Value::from(2),
                    label: "2".into()
                },
                OptionItem {
                    value: Value::from(3),
                    label: "Three".into()
                },
            ]
        );
        q.value_column = Some("name".into());
        q.label_column = Some("missing".into());
        assert_eq!(
            OptionsCollector::new(&["id".into(), "name".into()], &q).unwrap_err(),
            OptionsError::UnknownColumn {
                column: "missing".into()
            }
        );
        assert_eq!(
            OptionsCollector::new(&[], &q).unwrap_err(),
            OptionsError::NoColumns
        );
    }

    struct Rows {
        sent: std::sync::Mutex<Vec<(String, bool, bool)>>,
        canceled: std::sync::atomic::AtomicBool,
        rows: usize,
    }

    #[async_trait::async_trait]
    impl QueryExecutor for Rows {
        async fn execute(
            &self,
            profile: &ConnectionProfile,
            sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            self.sent.lock().unwrap().push((
                sql.to_string(),
                profile.block_all_but_select,
                profile.enforce_read_only,
            ));
            let (tx, rx) = tokio::sync::mpsc::channel(4096);
            let columns = vec![sqlly_core::ResultColumn {
                name: "v".into(),
                data_type: "int".into(),
                nullable: false,
            }];
            let _ = tx.try_send(QueryEvent::ResultSet { index: 0, columns });
            for chunk in (0..self.rows as i64).collect::<Vec<_>>().chunks(500) {
                let rows = chunk.iter().map(|n| vec![CellValue::I64(*n)]).collect();
                let _ = tx.try_send(QueryEvent::Rows { index: 0, rows });
            }
            let _ = tx.try_send(QueryEvent::Completed { elapsed_ms: 1 });
            Ok(crate::query::QueryHandle {
                task_id: "t1".into(),
                events: rx,
            })
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            self.canceled
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// One long text option: cut to the 8 KiB preview on a grid run, whole
    /// on the full-values path.
    struct LongOption(String);

    #[async_trait::async_trait]
    impl QueryExecutor for LongOption {
        async fn execute(
            &self,
            _profile: &ConnectionProfile,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            Ok(self.reply(false))
        }

        async fn execute_full_values(
            &self,
            _profile: &ConnectionProfile,
            _session: Option<&str>,
            _sql: &str,
            _row_buffer: usize,
        ) -> sqlly_core::StudioResult<crate::query::QueryHandle> {
            Ok(self.reply(true))
        }

        async fn cancel(&self, _task_id: &str) -> sqlly_core::StudioResult<()> {
            Ok(())
        }
    }

    impl LongOption {
        fn reply(&self, full: bool) -> crate::query::QueryHandle {
            let mut rows = vec![vec![CellValue::String(self.0.clone())]];
            if !full {
                crate::query::truncate_rows(&mut rows, 0, &crate::query::CellLimits::default());
            }
            let (tx, rx) = tokio::sync::mpsc::channel(8);
            let columns = vec![sqlly_core::ResultColumn {
                name: "v".into(),
                data_type: "nvarchar".into(),
                nullable: false,
            }];
            let _ = tx.try_send(QueryEvent::ResultSet { index: 0, columns });
            let _ = tx.try_send(QueryEvent::Rows { index: 0, rows });
            let _ = tx.try_send(QueryEvent::Completed { elapsed_ms: 1 });
            crate::query::QueryHandle {
                task_id: "t1".into(),
                events: rx,
            }
        }
    }

    #[tokio::test]
    async fn an_option_longer_than_the_cell_preview_is_offered_whole() {
        let long = "x".repeat(9 * 1024);
        let profile = ConnectionProfile::sql_login(
            "dev",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        let q = OptionsQuery::new("SELECT v FROM docs");
        let bound = BoundOptions {
            sql: q.sql.clone(),
            linked: Vec::new(),
            limit: q.limit(),
        };
        let page = collect_options(&LongOption(long.clone()), &profile, &bound, &q)
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].value, Value::String(long));
    }

    #[tokio::test]
    async fn discovery_queries_run_read_only_and_stop_at_the_limit() {
        let queries = Rows {
            sent: Default::default(),
            canceled: Default::default(),
            rows: 5_000,
        };
        let mut profile = ConnectionProfile::sql_login(
            "dev",
            "localhost",
            "sa",
            sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "k".into(),
            },
        );
        profile.block_all_but_select = false;
        profile.enforce_read_only = false;
        let q = OptionsQuery::new("SELECT v FROM numbers");
        let bound = BoundOptions {
            sql: q.sql.clone(),
            linked: Vec::new(),
            limit: q.limit(),
        };
        let page = collect_options(&queries, &profile, &bound, &q)
            .await
            .unwrap();
        assert_eq!(page.items.len(), DEFAULT_OPTIONS_LIMIT as usize);
        assert!(page.truncated);
        assert!(queries.canceled.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            queries.sent.lock().unwrap().as_slice(),
            [("SELECT v FROM numbers".to_string(), true, true)]
        );
        let small = Rows {
            sent: Default::default(),
            canceled: Default::default(),
            rows: 3,
        };
        let page = collect_options(&small, &profile, &bound, &q).await.unwrap();
        assert_eq!(page.items.len(), 3);
        assert!(!page.truncated);
        assert!(!small.canceled.load(std::sync::atomic::Ordering::SeqCst));
        let refused = BoundOptions {
            sql: "DELETE FROM numbers".into(),
            ..bound
        };
        assert_eq!(
            collect_options(&small, &profile, &refused, &q)
                .await
                .unwrap_err(),
            OptionsError::NotReadOnly
        );
        assert_eq!(small.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn linked_dependents_follow_options_queries_transitively() {
        let db = DatabaseType::SqlServer;
        let defs = vec![
            ParameterDef::new("region", ParamType::String),
            enum_def("store", "SELECT id FROM stores WHERE region = :region"),
            enum_def("till", "SELECT id FROM tills WHERE store = {{param.store}}"),
            enum_def("other", "SELECT 1"),
        ];
        assert_eq!(
            linked_dependents(db, &defs, &["region".into()]),
            ["store", "till"]
        );
        assert_eq!(linked_dependents(db, &defs, &["store".into()]), ["till"]);
        assert!(linked_dependents(db, &defs, &["other".into()]).is_empty());
    }

    #[test]
    fn choices_become_typed_values_and_static_labels_are_kept() {
        let mut def = ParameterDef::new("tier", ParamType::Enum);
        def.allowed_values = Some(AllowedValues::Static(vec![
            serde_json::json!({"value": 1, "label": "Gold"}),
            serde_json::json!(2),
        ]));
        let options = static_options(&def);
        assert_eq!(options[0].label, "Gold");
        assert_eq!(options[1].label, "2");
        assert_eq!(
            choice_value(&def, &[Value::from(1)]),
            Ok(ParamValue::Integer(1))
        );
        assert_eq!(
            choice_value(&def, &[Value::from(3)]),
            Err(ParamError::NotAllowed)
        );
        let mut multi = def.clone();
        multi.ty = ParamType::MultiSelect;
        assert_eq!(
            choice_value(&multi, &[Value::from(1), Value::from(2)]),
            Ok(ParamValue::List(vec![
                ParamValue::Integer(1),
                ParamValue::Integer(2)
            ]))
        );
        let ident = ParameterDef::new(
            "tbl",
            ParamType::Identifier {
                allowlist: vec!["dbo.Orders".into()],
            },
        );
        assert_eq!(static_options(&ident)[0].label, "dbo.Orders");
        assert_eq!(
            choice_value(&ident, &[Value::from("dbo.Orders")]),
            Ok(ParamValue::Identifier("dbo.Orders".into()))
        );
    }

    #[test]
    fn date_presets_resolve_on_the_injected_clock() {
        let mut def = ParameterDef::new("period", ParamType::DateRange);
        def.presets = vec![Preset::Date(DatePreset::Last7Days)];
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let cases = [
            ("last_7_days", d(2026, 11, 12), d(2026, 11, 18)),
            ("previous_month", d(2026, 10, 1), d(2026, 10, 31)),
            ("current_quarter", d(2026, 10, 1), d(2026, 12, 31)),
            ("year_to_date", d(2026, 1, 1), d(2026, 11, 18)),
        ];
        for (id, start, end) in cases {
            assert_eq!(
                value_from_json(&def, &serde_json::json!({"preset": id}), &clock()),
                Ok(ParamValue::DateRange { start, end }),
                "{id}"
            );
        }
        assert_eq!(
            value_from_json(&def, &serde_json::json!({"preset": "nope"}), &clock()),
            Err(ParamError::UnknownPreset {
                preset: "nope".into()
            })
        );
    }

    #[test]
    fn sets_leave_out_sensitive_and_personal_values() {
        let mut token = ParameterDef::new("token", ParamType::String);
        token.sensitive = true;
        let mut mine = ParameterDef::new("mine", ParamType::String);
        mine.scope = ParamScope::Personal;
        let region = ParameterDef::new("region", ParamType::String);
        let defs = vec![token, mine, region];
        let values = BTreeMap::from([
            ("token".to_string(), ParamValue::Text("s3cret".into())),
            ("mine".to_string(), ParamValue::Text("me".into())),
            ("region".to_string(), ParamValue::Text("EU".into())),
        ]);
        let shared = capture_set("s1", "EU", &defs, &values, SetTarget::Shared);
        assert_eq!(shared.values.keys().collect::<Vec<_>>(), ["region"]);
        let personal = capture_set("s2", "Me", &defs, &values, SetTarget::Personal);
        assert_eq!(
            personal.values.keys().collect::<Vec<_>>(),
            ["mine", "region"]
        );
        let mut doc = doc_with(&[]);
        doc.parameters = defs.clone();
        store_shared_sets(&mut doc, std::slice::from_ref(&shared));
        let text = doc.to_canonical_json();
        assert!(!text.contains("s3cret") && !text.contains("\"me\""));
        let back = crate::notebook::load(None, &text, &mut SeededIds(1), None).unwrap();
        assert_eq!(shared_sets(&back.doc), vec![shared.clone()]);
        let mut forged = shared.clone();
        forged.values.insert("token".into(), Value::from("x"));
        forged.values.insert("gone".into(), Value::from("x"));
        let (applied, errors) = set_values(&defs, &forged, &SystemClock);
        assert_eq!(applied.keys().collect::<Vec<_>>(), ["region"]);
        assert!(errors.is_empty());
        store_shared_sets(&mut doc, &[]);
        assert!(!doc.extra.contains_key(SETS_KEY));
    }

    #[test]
    fn personal_values_never_keep_sensitive_ones_and_only_fill_personal_parameters() {
        let secret = ParameterDef::new("key", ParamType::SecretRef);
        let mut pin = ParameterDef::new("pin", ParamType::String);
        pin.sensitive = true;
        pin.scope = ParamScope::Personal;
        let mut mine = ParameterDef::new("mine", ParamType::Integer);
        mine.scope = ParamScope::Personal;
        let shared = ParameterDef::new("shared", ParamType::Integer);
        let mut p = PersonalParams::new("nb1");
        assert!(!p.set_value(&pin, Some(&ParamValue::Text("1234".into()))));
        assert!(p.set_value(&secret, Some(&ParamValue::SecretRef("prod-key".into()))));
        assert!(p.set_value(&mine, Some(&ParamValue::Integer(4))));
        assert!(!p.set_value(&mine, Some(&ParamValue::Integer(4))));
        p.values.insert("shared".into(), Value::from(9));
        let text = p.to_json();
        assert!(!text.contains("1234"));
        let back: PersonalParams = serde_json::from_str(&text).unwrap();
        let values = back.values_for(&[secret, pin, mine.clone(), shared], &SystemClock);
        assert_eq!(
            values,
            BTreeMap::from([
                ("key".to_string(), ParamValue::SecretRef("prod-key".into())),
                ("mine".to_string(), ParamValue::Integer(4)),
            ])
        );
        let mut p = back;
        assert!(p.set_value(&mine, None));
        assert!(!p.values.contains_key("mine"));
    }

    #[test]
    fn shareable_state_excludes_sensitive_secret_and_unshareable_values() {
        let mut a = ParameterDef::new("a", ParamType::String);
        a.sensitive = true;
        let b = ParameterDef::new("b", ParamType::SecretRef);
        let mut c = ParameterDef::new("c", ParamType::String);
        c.shareable = false;
        let d = ParameterDef::new("d", ParamType::String);
        let defs = vec![a, b, c, d];
        let supplied = BTreeMap::from([
            ("a".to_string(), ParamValue::Text("x".into())),
            ("b".to_string(), ParamValue::SecretRef("k".into())),
            ("c".to_string(), ParamValue::Text("y".into())),
            ("d".to_string(), ParamValue::Text("z".into())),
        ]);
        let resolved = params::resolve_all(&defs, &supplied, &SystemClock);
        assert_eq!(
            shareable_values(&defs, &resolved),
            BTreeMap::from([("d".to_string(), Value::from("z"))])
        );
    }

    #[test]
    fn markdown_shows_escaped_values_masks_sensitive_ones_and_skips_code() {
        let mut pin = ParameterDef::new("pin", ParamType::String);
        pin.sensitive = true;
        let defs = vec![
            ParameterDef::new("name", ParamType::String),
            ParameterDef::new("period", ParamType::DateRange),
            pin,
            ParameterDef::new("unset", ParamType::String),
        ];
        let supplied = BTreeMap::from([
            (
                "name".to_string(),
                ParamValue::Text("# <b>[x](y)</b>\nhi".into()),
            ),
            (
                "period".to_string(),
                ParamValue::DateRange {
                    start: NaiveDate::from_ymd_opt(2026, 1, 1).unwrap(),
                    end: NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
                },
            ),
            ("pin".to_string(), ParamValue::Text("1234".into())),
        ]);
        let resolved = params::resolve_all(&defs, &supplied, &SystemClock);
        let out = interpolate_markdown(
            "Hi {{param.name}} for {{param.period}} to {{param.period.end}}, pin {{param.pin}} \
             `{{param.name}}` {{param.unset}} {{param.period.bogus}}",
            &defs,
            &resolved,
        );
        assert_eq!(
            out,
            "Hi \\# \\<b\\>\\[x\\]\\(y\\)\\<\\/b\\> hi for 2026\\-01\\-01 – 2026\\-01\\-31 to \
             2026\\-01\\-31, pin •••••• `{{param.name}}` {{param.unset}} {{param.period.bogus}}"
        );
        assert!(!out.contains("1234"));
    }

    #[test]
    fn remote_cells_wait_for_apply_and_local_ones_run_at_once() {
        let mut doc = doc_with(&[
            (CellKind::Sql, "SELECT * FROM orders WHERE region = :region"),
            (CellKind::Chart, ""),
            (CellKind::Markdown, "Region {{param.region}}"),
            (CellKind::Sql, "SELECT 1"),
        ]);
        let mut local = Cell::new("loc", CellKind::Transform, Default::default());
        local.spec = serde_json::json!({});
        doc.cells.push(local);
        doc.parameters
            .push(ParameterDef::new("region", ParamType::String));
        let sql = doc.cells[0].id.clone();
        doc.cells[1].spec = serde_json::json!({"input": sql});
        // A local transform that reads the parameter directly through a
        // parameter cell is not a thing; drive one with a direct edge instead.
        let mut param_cell = Cell::new("pc", CellKind::Parameter, Default::default());
        param_cell.spec = ParameterCellSpec::for_parameter("region").to_value();
        doc.cells.push(param_cell);
        doc.cells[4].spec = serde_json::json!({"input": "pc"});
        let mut duck = Cell::new("duck", CellKind::Sql, Default::default());
        duck.source = "SELECT * FROM t WHERE region = :region".into();
        duck.spec = serde_json::json!({"target": "local"});
        doc.cells.push(duck);
        let graph = Graph::build(&doc, &Dialects::single(DatabaseType::SqlServer));
        let plan = reactive_plan(&doc, &graph, &["region".into()]);
        assert_eq!(plan.after_apply, [sql.clone(), doc.cells[1].id.clone()]);
        assert_eq!(plan.run_now, ["loc", "duck"]);
        assert_eq!(
            reactive_plan(&doc, &graph, &["nope".into()]),
            ReactivePlan::default()
        );
    }

    #[test]
    fn renames_reach_parameter_cells_and_other_options_queries() {
        let mut doc = doc_with(&[(CellKind::Sql, "SELECT :region")]);
        doc.parameters
            .push(ParameterDef::new("region", ParamType::String));
        let mut store = enum_def(
            "store",
            "SELECT id FROM s WHERE r = :region OR r = {{param.region}}",
        );
        store.reactivity = Reactivity::Apply;
        doc.parameters.push(store);
        let mut pc = Cell::new("pc", CellKind::Parameter, Default::default());
        pc.spec = ParameterCellSpec::for_parameter("region").to_value();
        doc.cells.push(pc);
        let changed = rename_parameter(
            &mut doc,
            "region",
            "area",
            &Dialects::single(DatabaseType::SqlServer),
        )
        .unwrap();
        assert!(changed.contains(&"pc".to_string()));
        assert_eq!(doc.cells[0].source, "SELECT :area");
        assert_eq!(ParameterCellSpec::of(&doc.cells[1]).parameter, "area");
        assert_eq!(
            OptionsQuery::of(&doc.parameters[1]).unwrap().sql,
            "SELECT id FROM s WHERE r = :area OR r = {{param.area}}"
        );
        assert_eq!(doc.parameters[1].reactivity, Reactivity::Apply);
    }
}
