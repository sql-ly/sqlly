//! The `.sqllynb` document, format version 2.
//!
//! Serialization is canonical ([`NotebookDoc::to_canonical_json`]): struct
//! fields in declaration order, every free-form JSON object (cell `spec`,
//! preserved unknown fields) with sorted keys, 2-space indent, trailing
//! newline. Unknown fields and unknown cell kinds survive a load/save round
//! trip. Outputs are never part of the document.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use super::params::ParameterDef;

pub const FORMAT: &str = "sqlly.notebook";
pub const FORMAT_VERSION: u32 = 2;
/// Extension of the v2 format (no leading dot).
pub const EXTENSION: &str = "sqllynb";
/// Extension of the legacy v1 format.
pub const LEGACY_EXTENSION: &str = "sqlnb.json";
pub const IPYNB_EXTENSION: &str = "ipynb";

/// Unknown fields kept verbatim for round trips.
pub type Extra = BTreeMap<String, Value>;

/// A JSON value written with object keys sorted, at every depth.
pub(crate) struct Sorted<'a>(pub &'a Value);

impl Serialize for Sorted<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Object(map) => {
                let mut entries: Vec<(&String, &Value)> = map.iter().collect();
                entries.sort_by(|a, b| a.0.cmp(b.0));
                serializer.collect_map(entries.into_iter().map(|(k, v)| (k, Sorted(v))))
            }
            Value::Array(items) => serializer.collect_seq(items.iter().map(Sorted)),
            other => other.serialize(serializer),
        }
    }
}

pub(crate) fn ser_sorted<S: Serializer>(value: &Value, serializer: S) -> Result<S::Ok, S::Error> {
    Sorted(value).serialize(serializer)
}

pub(crate) fn ser_extra<S: Serializer>(extra: &Extra, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_map(extra.iter().map(|(k, v)| (k, Sorted(v))))
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn yes() -> bool {
    true
}

/// The notebook document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotebookDoc {
    /// Always [`FORMAT`].
    pub format: String,
    pub format_version: u32,
    /// Stable identity of the notebook (a UUID), independent of its path.
    pub notebook_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub connection_refs: Vec<ConnectionRef>,
    #[serde(default)]
    pub parameters: Vec<ParameterDef>,
    #[serde(default)]
    pub cells: Vec<Cell>,
    #[serde(default)]
    pub data_retention: DataRetention,
    #[serde(default)]
    pub settings: NotebookSettings,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

/// A named connection slot. The profile is referenced by opaque id only;
/// `None` means the notebook was opened somewhere the slot is not mapped yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DataRetention {
    /// Embed capped output snapshots in the file. Off by default.
    #[serde(default)]
    pub embed_outputs: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_embedded_rows: Option<u32>,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RunMode {
    /// Cells run only when asked.
    #[default]
    Manual,
    /// Stale cells re-run after an edit or parameter change (subject to each
    /// parameter's Apply gating).
    Reactive,
    /// Written by a newer SQLLY; runs like [`RunMode::Manual`] here and is
    /// saved back unchanged.
    Unknown(&'static str),
}

super::lenient::lenient_names!(RunMode {
    Manual => "manual",
    Reactive => "reactive",
});

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct NotebookSettings {
    #[serde(default)]
    pub run_mode: RunMode,
    /// The policy new cells start with.
    #[serde(default)]
    pub default_policy: ExecutionPolicy,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

/// When a cell's previous output may stand in for a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CachePolicy {
    /// Always execute.
    Off,
    /// Reuse while the fingerprint matches (this session).
    #[default]
    Reuse,
    /// Reuse while the fingerprint matches and the output is younger than
    /// `seconds`. Never proof the source data is unchanged.
    Ttl { seconds: u64 },
    /// Keep the output until the user runs this cell explicitly; bulk runs
    /// reuse it while the fingerprint matches.
    Pinned,
    /// A policy from a newer SQLLY (its JSON text): behaves like
    /// [`CachePolicy::Reuse`] here and is saved back unchanged.
    Unknown(&'static str),
}

/// The policies this build knows, as written.
#[derive(Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum KnownCachePolicy {
    Off,
    Reuse,
    Ttl { seconds: u64 },
    Pinned,
}

impl Serialize for CachePolicy {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let known = match *self {
            CachePolicy::Off => KnownCachePolicy::Off,
            CachePolicy::Reuse => KnownCachePolicy::Reuse,
            CachePolicy::Ttl { seconds } => KnownCachePolicy::Ttl { seconds },
            CachePolicy::Pinned => KnownCachePolicy::Pinned,
            CachePolicy::Unknown(text) => {
                let raw: Value = serde_json::from_str(text).unwrap_or(Value::Null);
                return Sorted(&raw).serialize(serializer);
            }
        };
        known.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for CachePolicy {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Value::deserialize(deserializer)?;
        Ok(
            match serde_json::from_value::<KnownCachePolicy>(raw.clone()) {
                Ok(KnownCachePolicy::Off) => CachePolicy::Off,
                Ok(KnownCachePolicy::Reuse) => CachePolicy::Reuse,
                Ok(KnownCachePolicy::Ttl { seconds }) => CachePolicy::Ttl { seconds },
                Ok(KnownCachePolicy::Pinned) => CachePolicy::Pinned,
                Err(_) => {
                    let text = serde_json::to_string(&Sorted(&raw)).unwrap_or_default();
                    CachePolicy::Unknown(super::lenient::intern(&text))
                }
            },
        )
    }
}

/// Per-cell safety and resource limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    /// Run with SELECT-only enforcement. New cells default to `true`; a
    /// write cell needs an explicit elevation.
    #[serde(default = "yes")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u32>,
    /// Rows kept in the output snapshot (default
    /// [`super::output::DEFAULT_OUTPUT_ROW_CAP`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_download_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "is_default_cache")]
    pub cache: CachePolicy,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

fn is_default_cache(policy: &CachePolicy) -> bool {
    *policy == CachePolicy::default()
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            read_only: true,
            timeout_seconds: None,
            max_rows: None,
            max_download_bytes: None,
            cache: CachePolicy::default(),
            extra: Extra::new(),
        }
    }
}

/// Cell kinds. Unknown kinds (from a newer SQLLY) load as
/// [`CellKind::Unknown`] and save back unchanged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CellKind {
    Sql,
    Markdown,
    Parameter,
    Scalar,
    Transform,
    Chart,
    Table,
    Pivot,
    Kpi,
    EntityDetail,
    Plan,
    Profile,
    Assertion,
    Diff,
    Ai,
    Layout,
    Attachment,
    Export,
    /// Reserved (ADR-009): loads and saves, never runs.
    Python,
    Unknown(String),
}

/// How a kind executes, for the planner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecClass {
    /// Nothing to execute (Markdown, parameters, layout).
    Instant,
    /// Runs on the cell's connection.
    Remote,
    /// Runs in-process over upstream outputs.
    Local,
    /// Cannot run in this build.
    Unsupported,
}

/// What a cell's `source` text is, for reference scanning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLanguage {
    Sql,
    Markdown,
    /// `source` is not scanned; references come from `spec`.
    None,
}

impl CellKind {
    pub const ALL: [CellKind; 19] = [
        CellKind::Sql,
        CellKind::Markdown,
        CellKind::Parameter,
        CellKind::Scalar,
        CellKind::Transform,
        CellKind::Chart,
        CellKind::Table,
        CellKind::Pivot,
        CellKind::Kpi,
        CellKind::EntityDetail,
        CellKind::Plan,
        CellKind::Profile,
        CellKind::Assertion,
        CellKind::Diff,
        CellKind::Ai,
        CellKind::Layout,
        CellKind::Attachment,
        CellKind::Export,
        CellKind::Python,
    ];

    pub fn as_str(&self) -> &str {
        match self {
            CellKind::Sql => "sql",
            CellKind::Markdown => "markdown",
            CellKind::Parameter => "parameter",
            CellKind::Scalar => "scalar",
            CellKind::Transform => "transform",
            CellKind::Chart => "chart",
            CellKind::Table => "table",
            CellKind::Pivot => "pivot",
            CellKind::Kpi => "kpi",
            CellKind::EntityDetail => "entity_detail",
            CellKind::Plan => "plan",
            CellKind::Profile => "profile",
            CellKind::Assertion => "assertion",
            CellKind::Diff => "diff",
            CellKind::Ai => "ai",
            CellKind::Layout => "layout",
            CellKind::Attachment => "attachment",
            CellKind::Export => "export",
            CellKind::Python => "python",
            CellKind::Unknown(other) => other,
        }
    }

    pub fn parse(text: &str) -> CellKind {
        CellKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .unwrap_or_else(|| CellKind::Unknown(text.to_string()))
    }

    pub fn exec_class(&self) -> ExecClass {
        match self {
            CellKind::Markdown | CellKind::Parameter | CellKind::Layout => ExecClass::Instant,
            CellKind::Sql | CellKind::Plan | CellKind::EntityDetail => ExecClass::Remote,
            CellKind::Scalar
            | CellKind::Transform
            | CellKind::Chart
            | CellKind::Table
            | CellKind::Pivot
            | CellKind::Kpi
            | CellKind::Profile
            | CellKind::Assertion
            | CellKind::Diff
            | CellKind::Ai
            | CellKind::Attachment
            | CellKind::Export => ExecClass::Local,
            CellKind::Python | CellKind::Unknown(_) => ExecClass::Unsupported,
        }
    }

    pub fn source_language(&self) -> SourceLanguage {
        match self {
            CellKind::Sql => SourceLanguage::Sql,
            CellKind::Markdown | CellKind::Ai => SourceLanguage::Markdown,
            _ => SourceLanguage::None,
        }
    }
}

impl Serialize for CellKind {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for CellKind {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(CellKind::parse(&String::deserialize(deserializer)?))
    }
}

/// One cell.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// Immutable opaque id ([`new_cell_id`] for new cells, `cell_N` for v1).
    pub id: String,
    /// Optional unique SQL-identifier alias used in `{{name}}` references.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(alias = "type")]
    pub kind: CellKind,
    #[serde(default)]
    pub source: String,
    /// A [`ConnectionRef::id`]; `None` uses the notebook default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_ref: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub collapsed: bool,
    #[serde(default)]
    pub execution_policy: ExecutionPolicy,
    /// Cells sharing a group run serially on one pinned session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_group: Option<String>,
    /// Kind-specific configuration, typed by the owning lane.
    #[serde(
        default,
        skip_serializing_if = "Value::is_null",
        serialize_with = "ser_sorted"
    )]
    pub spec: Value,
    #[serde(flatten, serialize_with = "ser_extra")]
    pub extra: Extra,
}

impl Cell {
    pub fn new(id: impl Into<String>, kind: CellKind, policy: ExecutionPolicy) -> Self {
        Self {
            id: id.into(),
            name: None,
            kind,
            source: String::new(),
            connection_ref: None,
            collapsed: false,
            execution_policy: policy,
            session_group: None,
            spec: Value::Null,
            extra: Extra::new(),
        }
    }

    /// Where the cell runs: its kind's class, except that a SQL cell
    /// targeting the notebook's local data runs in-process.
    pub fn exec_class(&self) -> ExecClass {
        if super::cells::local::targets_local(self) {
            ExecClass::Local
        } else {
            self.kind.exec_class()
        }
    }

    /// The name SQL uses for this cell's relation: its name, else its id.
    pub fn relation_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }

    /// Cell ids named by `spec.input` and `spec.inputs` (strings, or objects
    /// with a `cell` field), in order, deduplicated.
    pub fn spec_inputs(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |value: &Value| {
            let id = match value {
                Value::String(s) => Some(s.as_str()),
                Value::Object(map) => map.get("cell").and_then(Value::as_str),
                _ => None,
            };
            if let Some(id) = id
                && !id.is_empty()
                && !out.iter().any(|seen| seen == id)
            {
                out.push(id.to_string());
            }
        };
        if let Some(input) = self.spec.get("input") {
            push(input);
        }
        if let Some(Value::Array(inputs)) = self.spec.get("inputs") {
            inputs.iter().for_each(&mut push);
        }
        out
    }
}

/// Supplies random bytes for ids. [`RandomIds`] in the app, [`SeededIds`] in
/// tests.
pub trait IdSource {
    fn fill(&mut self, bytes: &mut [u8]);
}

/// Ids from the platform's randomness (UUID v4 bytes, which also works in
/// the browser).
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomIds;

impl IdSource for RandomIds {
    fn fill(&mut self, bytes: &mut [u8]) {
        for chunk in bytes.chunks_mut(16) {
            let random = uuid::Uuid::new_v4().into_bytes();
            chunk.copy_from_slice(&random[..chunk.len()]);
        }
    }
}

/// Deterministic ids (splitmix64) for tests and reproducible fixtures.
#[derive(Debug, Clone)]
pub struct SeededIds(pub u64);

impl IdSource for SeededIds {
    fn fill(&mut self, bytes: &mut [u8]) {
        for b in bytes {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            *b = (z ^ (z >> 31)) as u8;
        }
    }
}

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// `prefix` + `len` lowercase base32 characters.
pub(crate) fn random_token(ids: &mut dyn IdSource, prefix: &str, len: usize) -> String {
    let mut bytes = vec![0u8; len];
    ids.fill(&mut bytes);
    let mut out = String::with_capacity(prefix.len() + len);
    out.push_str(prefix);
    out.extend(bytes.iter().map(|b| BASE32[(*b & 31) as usize] as char));
    out
}

/// A new cell id: `c_` + 10 base32 characters, retried until `taken` says
/// it is free.
pub fn new_cell_id(ids: &mut dyn IdSource, taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = random_token(ids, "c_", 10);
        if !taken(&id) {
            return id;
        }
    }
}

/// A new notebook id (UUID v4 text).
pub fn new_notebook_id(ids: &mut dyn IdSource) -> String {
    let mut bytes = [0u8; 16];
    ids.fill(&mut bytes);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// Whether `id` has the shape of an id SQLLY writes (`c_` + base32, or v1
/// `cell_N`).
pub fn is_sqlly_cell_id(id: &str) -> bool {
    if let Some(rest) = id.strip_prefix("cell_") {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit());
    }
    id.strip_prefix("c_")
        .is_some_and(|rest| rest.len() == 10 && rest.bytes().all(|b| BASE32.contains(&b)))
}

/// The on-disk format of a notebook file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileFormat {
    /// `.sqllynb` (v2 and later).
    SqllyNotebook,
    /// `.sqlnb.json` v1.
    LegacyV1,
    Ipynb,
}

impl FileFormat {
    /// The format a path's extension names, if any.
    pub fn from_path(path: &str) -> Option<FileFormat> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(&format!(".{LEGACY_EXTENSION}")) {
            Some(FileFormat::LegacyV1)
        } else if lower.ends_with(&format!(".{EXTENSION}")) {
            Some(FileFormat::SqllyNotebook)
        } else if lower.ends_with(&format!(".{IPYNB_EXTENSION}")) {
            Some(FileFormat::Ipynb)
        } else {
            None
        }
    }

    /// The format of parsed JSON content, by its markers: `format` for v2,
    /// `nbformat` for Jupyter, a `cells` array for v1.
    pub fn sniff(value: &Value) -> Option<FileFormat> {
        let object = value.as_object()?;
        if object.get("format").and_then(Value::as_str) == Some(FORMAT) {
            Some(FileFormat::SqllyNotebook)
        } else if object.contains_key("nbformat") {
            Some(FileFormat::Ipynb)
        } else if object.get("cells").is_some_and(Value::is_array) && !object.contains_key("format")
        {
            Some(FileFormat::LegacyV1)
        } else {
            None
        }
    }
}

impl NotebookDoc {
    /// An empty notebook with one SQL cell.
    pub fn new(title: impl Into<String>, ids: &mut dyn IdSource) -> Self {
        let mut doc = Self::empty(new_notebook_id(ids), title);
        doc.add_cell(CellKind::Sql, None, ids);
        doc
    }

    /// A notebook with no cells.
    pub fn empty(notebook_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            format: FORMAT.to_string(),
            format_version: FORMAT_VERSION,
            notebook_id: notebook_id.into(),
            title: title.into(),
            connection_refs: Vec::new(),
            parameters: Vec::new(),
            cells: Vec::new(),
            data_retention: DataRetention::default(),
            settings: NotebookSettings::default(),
            extra: Extra::new(),
        }
    }

    /// Insert a new cell (fresh id, the notebook's default policy) after
    /// index `after`, or at the end. Returns its index.
    pub fn add_cell(
        &mut self,
        kind: CellKind,
        after: Option<usize>,
        ids: &mut dyn IdSource,
    ) -> usize {
        let id = new_cell_id(ids, |id| self.cell(id).is_some());
        let cell = Cell::new(id, kind, self.settings.default_policy.clone());
        let index = match after {
            Some(i) if i < self.cells.len() => i + 1,
            _ => self.cells.len(),
        };
        self.cells.insert(index, cell);
        index
    }

    pub fn remove_cell(&mut self, index: usize) -> Option<Cell> {
        (index < self.cells.len()).then(|| self.cells.remove(index))
    }

    pub fn move_cell(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.cells.len() || to >= self.cells.len() {
            return false;
        }
        let cell = self.cells.remove(from);
        self.cells.insert(to, cell);
        true
    }

    pub fn cell_index(&self, id: &str) -> Option<usize> {
        self.cells.iter().position(|c| c.id == id)
    }

    pub fn cell(&self, id: &str) -> Option<&Cell> {
        self.cells.iter().find(|c| c.id == id)
    }

    pub fn cell_mut(&mut self, id: &str) -> Option<&mut Cell> {
        self.cells.iter_mut().find(|c| c.id == id)
    }

    /// The cell a `{{token}}` names: a cell whose name matches
    /// (case-insensitively), else the cell with that exact id.
    pub fn resolve(&self, token: &str) -> Option<&Cell> {
        self.cells
            .iter()
            .find(|c| {
                c.name
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(token))
            })
            .or_else(|| self.cell(token))
    }

    pub fn parameter(&self, name: &str) -> Option<&ParameterDef> {
        self.parameters
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// The connection slot a cell runs on: its own, else the first declared
    /// slot. `None` means the host's current connection.
    pub fn effective_connection_ref<'a>(&'a self, cell: &'a Cell) -> Option<&'a str> {
        cell.connection_ref
            .as_deref()
            .or_else(|| self.connection_refs.first().map(|c| c.id.as_str()))
    }

    pub fn connection(&self, id: &str) -> Option<&ConnectionRef> {
        self.connection_refs.iter().find(|c| c.id == id)
    }

    /// Canonical, deterministic JSON (2-space indent, trailing newline).
    pub fn to_canonical_json(&self) -> String {
        let mut out = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into());
        out.push('\n');
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[test]
    fn new_notebook_has_one_sql_cell_with_a_random_style_id() {
        let doc = NotebookDoc::new("Revenue", &mut SeededIds(1));
        assert_eq!(doc.format, FORMAT);
        assert_eq!(doc.format_version, 2);
        assert_eq!(doc.cells.len(), 1);
        assert_eq!(doc.cells[0].kind, CellKind::Sql);
        assert!(is_sqlly_cell_id(&doc.cells[0].id), "{}", doc.cells[0].id);
        assert!(doc.cells[0].execution_policy.read_only);
        assert!(uuid::Uuid::parse_str(&doc.notebook_id).is_ok());
    }

    #[test]
    fn seeded_ids_are_deterministic_and_distinct() {
        let a = NotebookDoc::new("x", &mut SeededIds(7));
        let b = NotebookDoc::new("x", &mut SeededIds(7));
        assert_eq!(a, b);
        let mut ids = SeededIds(7);
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..2000 {
            assert!(seen.insert(new_cell_id(&mut ids, |_| false)));
        }
    }

    #[test]
    fn new_cell_id_retries_on_collision() {
        let mut ids = SeededIds(3);
        let first = new_cell_id(&mut ids.clone(), |_| false);
        let second = new_cell_id(&mut ids, |id| id == first);
        assert_ne!(first, second);
        assert!(is_sqlly_cell_id(&second));
    }

    #[test]
    fn random_ids_have_the_contract_shape() {
        let id = new_cell_id(&mut RandomIds, |_| false);
        assert_eq!(id.len(), 12);
        assert!(is_sqlly_cell_id(&id));
    }

    #[test]
    fn add_remove_move_keep_ids_stable() {
        let mut ids = SeededIds(5);
        let mut doc = NotebookDoc::new("t", &mut ids);
        let first = doc.cells[0].id.clone();
        let i = doc.add_cell(CellKind::Markdown, Some(0), &mut ids);
        assert_eq!(i, 1);
        let second = doc.cells[1].id.clone();
        assert!(doc.move_cell(0, 1));
        assert_eq!(doc.cells[0].id, second);
        assert_eq!(doc.cells[1].id, first);
        assert!(!doc.move_cell(0, 9));
        assert_eq!(doc.remove_cell(0).unwrap().id, second);
        assert!(doc.remove_cell(4).is_none());
    }

    #[test]
    fn kinds_round_trip_including_unknown() {
        for kind in CellKind::ALL {
            assert_eq!(CellKind::parse(kind.as_str()), kind);
            let json = serde_json::to_string(&kind).unwrap();
            assert_eq!(serde_json::from_str::<CellKind>(&json).unwrap(), kind);
        }
        let future: CellKind = serde_json::from_str("\"sankey\"").unwrap();
        assert_eq!(future, CellKind::Unknown("sankey".into()));
        assert_eq!(serde_json::to_string(&future).unwrap(), "\"sankey\"");
        assert_eq!(future.exec_class(), ExecClass::Unsupported);
        assert_eq!(CellKind::Python.exec_class(), ExecClass::Unsupported);
    }

    #[test]
    fn canonical_json_is_sorted_stable_and_ends_with_newline() {
        let text = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","title":"T",
            "zeta":{"b":1,"a":[{"y":1,"x":2}]},"alpha":true,
            "cells":[{"id":"c_1","kind":"chart","source":"","spec":{"y":"Total","x":"Day","input":"c_0"},"future_field":{"k2":1,"k1":2}}]}"#;
        let doc: NotebookDoc = serde_json::from_str(text).unwrap();
        let out = doc.to_canonical_json();
        assert!(out.ends_with("}\n"));
        assert!(out.contains("\n  \"format\""), "2-space indent:\n{out}");
        let spec_at = |k: &str| out.find(&format!("\"{k}\": ")).unwrap();
        assert!(spec_at("input") < spec_at("x") && spec_at("x") < spec_at("y"));
        assert!(out.find("\"k1\"").unwrap() < out.find("\"k2\"").unwrap());
        assert!(out.find("\"alpha\"").unwrap() < out.find("\"zeta\"").unwrap());
        assert!(out.find("\"a\"").unwrap() < out.find("\"b\"").unwrap());
        assert!(out.find("\"x\": 2").unwrap() < out.find("\"y\": 1").unwrap());
        let again: NotebookDoc = serde_json::from_str(&out).unwrap();
        assert_eq!(again, doc);
        assert_eq!(again.to_canonical_json(), out);
        assert_eq!(doc.cells[0].extra["future_field"]["k1"], 2);
    }

    #[test]
    fn struct_fields_keep_declaration_order() {
        let doc = NotebookDoc::new("T", &mut SeededIds(2));
        let out = doc.to_canonical_json();
        let order = [
            "\"format\"",
            "\"format_version\"",
            "\"notebook_id\"",
            "\"title\"",
            "\"cells\"",
        ];
        let positions: Vec<usize> = order.iter().map(|k| out.find(k).unwrap()).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{out}");
    }

    #[test]
    fn missing_policy_defaults_to_read_only() {
        let cell: Cell = serde_json::from_str(r#"{"id":"c","kind":"sql"}"#).unwrap();
        assert!(cell.execution_policy.read_only);
        let cell: Cell =
            serde_json::from_str(r#"{"id":"c","kind":"sql","execution_policy":{}}"#).unwrap();
        assert!(cell.execution_policy.read_only);
        let cell: Cell =
            serde_json::from_str(r#"{"id":"c","type":"sql","execution_policy":{"read_only":false,"cache":{"mode":"ttl","seconds":60}}}"#)
                .unwrap();
        assert!(!cell.execution_policy.read_only);
        assert_eq!(
            cell.execution_policy.cache,
            CachePolicy::Ttl { seconds: 60 }
        );
    }

    #[test]
    fn spec_inputs_accept_strings_and_objects() {
        let mut cell = Cell::new("c", CellKind::Diff, ExecutionPolicy::default());
        cell.spec = serde_json::json!({"input": "a", "inputs": ["b", {"cell": "c2"}, "a", 3]});
        assert_eq!(cell.spec_inputs(), vec!["a", "b", "c2"]);
        cell.spec = Value::Null;
        assert!(cell.spec_inputs().is_empty());
    }

    #[test]
    fn resolve_prefers_names_then_ids() {
        let mut ids = SeededIds(9);
        let mut doc = NotebookDoc::new("t", &mut ids);
        doc.add_cell(CellKind::Sql, None, &mut ids);
        let (a, b) = (doc.cells[0].id.clone(), doc.cells[1].id.clone());
        doc.cells[1].name = Some("Orders".into());
        assert_eq!(doc.resolve("orders").unwrap().id, b);
        assert_eq!(doc.resolve(&a).unwrap().id, a);
        assert!(doc.resolve("nope").is_none());
    }

    #[test]
    fn effective_connection_falls_back_to_the_first_slot() {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(4));
        assert_eq!(doc.effective_connection_ref(&doc.cells[0].clone()), None);
        doc.connection_refs.push(ConnectionRef {
            id: "primary".into(),
            profile_id: Some("p-1".into()),
            label: "Prod".into(),
            database: None,
            extra: Extra::new(),
        });
        let cell = doc.cells[0].clone();
        assert_eq!(doc.effective_connection_ref(&cell), Some("primary"));
    }

    #[test]
    fn sniffs_formats_by_content_and_path() {
        let v2 = serde_json::json!({"format": FORMAT, "format_version": 2});
        let v1 = serde_json::json!({"version": 1, "cells": []});
        let nb = serde_json::json!({"nbformat": 4, "cells": []});
        assert_eq!(FileFormat::sniff(&v2), Some(FileFormat::SqllyNotebook));
        assert_eq!(FileFormat::sniff(&v1), Some(FileFormat::LegacyV1));
        assert_eq!(FileFormat::sniff(&nb), Some(FileFormat::Ipynb));
        assert_eq!(FileFormat::sniff(&serde_json::json!([1])), None);
        assert_eq!(
            FileFormat::from_path("a/b.SQLLYNB"),
            Some(FileFormat::SqllyNotebook)
        );
        assert_eq!(
            FileFormat::from_path(r"C:\x\old.sqlnb.json"),
            Some(FileFormat::LegacyV1)
        );
        assert_eq!(FileFormat::from_path("n.ipynb"), Some(FileFormat::Ipynb));
        assert_eq!(FileFormat::from_path("n.sql"), None);
    }

    /// A random document for property tests: every kind, odd text, specs
    /// and unknown fields at several depths.
    pub(crate) fn random_doc(seed: u64) -> NotebookDoc {
        let mut ids = SeededIds(seed);
        let mut next = |n: usize| {
            let mut b = [0u8; 1];
            ids.fill(&mut b);
            b[0] as usize % n
        };
        let texts = [
            "",
            "SELECT 1",
            "a\r\nb",
            "é \u{1F600} \"q\" \\",
            "{{x}}",
            "  \n\t",
        ];
        let mut doc = NotebookDoc::empty(format!("nb-{seed}"), texts[next(texts.len())]);
        for i in 0..next(8) {
            let kinds = CellKind::ALL;
            let kind = if next(10) == 0 {
                CellKind::Unknown(format!("future_{i}"))
            } else {
                kinds[next(kinds.len())].clone()
            };
            let mut cell = Cell::new(format!("c_{i:0>10}"), kind, ExecutionPolicy::default());
            cell.source = texts[next(texts.len())].to_string();
            cell.collapsed = next(2) == 0;
            cell.execution_policy.read_only = next(3) != 0;
            if next(2) == 0 {
                cell.name = Some(format!("n{i}"));
            }
            if next(2) == 0 {
                cell.spec = serde_json::json!({"z": i, "a": {"y": [1, {"k": texts[next(texts.len())]}], "b": null}});
            }
            if next(3) == 0 {
                cell.extra
                    .insert("zz_future".into(), serde_json::json!({"b": 1, "a": [true]}));
            }
            if next(3) == 0 {
                cell.execution_policy.cache = CachePolicy::Ttl {
                    seconds: next(100) as u64,
                };
            }
            doc.cells.push(cell);
        }
        if next(2) == 0 {
            doc.extra.insert(
                "layout".into(),
                serde_json::json!([{"cell_id": "c_0", "w": 6}]),
            );
        }
        doc
    }

    #[test]
    fn randomized_documents_round_trip_canonically() {
        for seed in 0..300 {
            let doc = random_doc(seed);
            let text = doc.to_canonical_json();
            let back: NotebookDoc = serde_json::from_str(&text).unwrap();
            assert_eq!(back, doc, "seed {seed}");
            assert_eq!(back.to_canonical_json(), text, "seed {seed}");
            // Re-ordering keys in the input never changes the output.
            let shuffled: serde_json::Value = serde_json::from_str(&text).unwrap();
            let reparsed: NotebookDoc = serde_json::from_value(shuffled).unwrap();
            assert_eq!(reparsed.to_canonical_json(), text);
            assert!(!text.contains("\"rows\""));
        }
    }
}
