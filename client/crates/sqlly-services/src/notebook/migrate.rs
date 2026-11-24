//! Loading every notebook format SQLLY has written or reads, and writing
//! Jupyter notebooks.
//!
//! - `.sqllynb` (v2+): migrated forward through [`MIGRATIONS`], one step per
//!   version, so a v3 only adds a step. A file from a newer SQLLY is refused
//!   with [`LoadError::NewerFormat`].
//! - `.sqlnb.json` v1: cells keep their `cell_N` ids and get `name = id`;
//!   SQL cells that change data become `read_only: false` so they keep
//!   working (ADR-014). `@name` globals become string parameters.
//! - `.ipynb` (nbformat 4): code cells become SQL (a `%%sql` / `%sql` magic
//!   is dropped), markdown and raw cells Markdown; outputs are ignored.
//!   SQLLY's own cell and notebook details ride in `metadata.sqlly`; other
//!   tools' metadata (papermill tags, Azure Data Studio cell guids) and raw
//!   cells are kept and written back.
//!
//! Content decides the format; the path's extension only breaks ties.

use std::collections::HashSet;

use serde_json::{Map, Value, json};
use sqlly_intel::query::safety::{classify_select_only, classify_select_only_for};
use sqlly_lang::DisplayMessage;
use sqlly_lang::sqlmodel::DatabaseType;

use super::document::{
    Cell, CellKind, ExecutionPolicy, Extra, FORMAT, FORMAT_VERSION, FileFormat, IdSource,
    NotebookDoc, SourceLanguage, is_sqlly_cell_id, new_notebook_id,
};
use super::params::{ParamType, ParameterDef};
use super::references::{RefTarget, find_references};
use super::scan::{is_identifier, mask_sql};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    InvalidJson {
        detail: String,
    },
    NotANotebook,
    /// Written by a newer SQLLY.
    NewerFormat {
        found: u32,
        supported: u32,
    },
    UnsupportedIpynb {
        nbformat: u64,
    },
    InvalidDocument {
        detail: String,
    },
}

impl LoadError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            LoadError::InvalidJson { detail } => DisplayMessage::new(
                "notebook.core.load.invalid_json",
                format!("The notebook file is not valid JSON: {detail}"),
            )
            .with_arg("detail", detail.clone()),
            LoadError::NotANotebook => DisplayMessage::new(
                "notebook.core.load.not_a_notebook",
                "This file is not a notebook SQLLY can open",
            ),
            LoadError::NewerFormat { found, supported } => DisplayMessage::new(
                "notebook.core.load.newer_format",
                format!(
                    "This notebook was written by a newer SQLLY (format {found}; this version reads up to {supported}). Update SQLLY to open it."
                ),
            )
            .with_arg("found", found.to_string())
            .with_arg("supported", supported.to_string()),
            LoadError::UnsupportedIpynb { nbformat } => DisplayMessage::new(
                "notebook.core.load.unsupported_ipynb",
                format!("Jupyter notebook format {nbformat} is not supported; save it as format 4"),
            )
            .with_arg("nbformat", nbformat.to_string()),
            LoadError::InvalidDocument { detail } => DisplayMessage::new(
                "notebook.core.load.invalid_document",
                format!("The notebook file is damaged: {detail}"),
            )
            .with_arg("detail", detail.clone()),
        }
    }
}

/// A loaded notebook.
#[derive(Debug, Clone, PartialEq)]
pub struct Loaded {
    pub doc: NotebookDoc,
    pub source_format: FileFormat,
    /// The version it was migrated from, when it was not current.
    pub migrated_from: Option<u32>,
}

/// What a migration step may need besides the JSON.
pub struct MigrateContext<'a> {
    pub ids: &'a mut dyn IdSource,
    /// Engine of the notebook's connection, for write detection; `None`
    /// judges SQL under every engine (stricter).
    pub db: Option<DatabaseType>,
    pub title: String,
}

type Step = fn(Value, &mut MigrateContext<'_>) -> Result<Value, LoadError>;

/// `MIGRATIONS[n]` turns version `n + 1` into version `n + 2`.
pub const MIGRATIONS: &[Step] = &[v1_to_v2];

/// The file name without its notebook extension.
pub fn title_from_path(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let lower = name.to_ascii_lowercase();
    for ext in [".sqlnb.json", ".sqllynb", ".ipynb"] {
        if lower.ends_with(ext) {
            return name[..name.len() - ext.len()].to_string();
        }
    }
    name.to_string()
}

/// Load a notebook file. `path` (when known) gives the title for formats
/// that carry none and breaks format ties.
pub fn load(
    path: Option<&str>,
    text: &str,
    ids: &mut dyn IdSource,
    db: Option<DatabaseType>,
) -> Result<Loaded, LoadError> {
    let value: Value = serde_json::from_str(text).map_err(|e| LoadError::InvalidJson {
        detail: e.to_string(),
    })?;
    let format = FileFormat::sniff(&value)
        .or_else(|| {
            path.and_then(FileFormat::from_path)
                .filter(|_| value.is_object())
        })
        .ok_or(LoadError::NotANotebook)?;
    let mut ctx = MigrateContext {
        ids,
        db,
        title: path.map(title_from_path).unwrap_or_default(),
    };
    if format == FileFormat::Ipynb {
        let mut doc = from_ipynb_value(&value, &mut ctx)?;
        super::cells::local::canonicalize_local_targets(&mut doc);
        return Ok(Loaded {
            doc,
            source_format: format,
            migrated_from: None,
        });
    }
    let version = version_of(&value)?;
    if version > FORMAT_VERSION {
        return Err(LoadError::NewerFormat {
            found: version,
            supported: FORMAT_VERSION,
        });
    }
    let mut value = value;
    for step in &MIGRATIONS[(version as usize - 1)..] {
        value = step(value, &mut ctx)?;
    }
    let mut doc: NotebookDoc =
        serde_json::from_value(value).map_err(|e| LoadError::InvalidDocument {
            detail: e.to_string(),
        })?;
    check(&doc)?;
    super::cells::local::canonicalize_local_targets(&mut doc);
    Ok(Loaded {
        doc,
        source_format: format,
        migrated_from: (version < FORMAT_VERSION).then_some(version),
    })
}

fn version_of(value: &Value) -> Result<u32, LoadError> {
    let field = if value.get("format").and_then(Value::as_str) == Some(FORMAT) {
        "format_version"
    } else {
        "version"
    };
    match value.get(field) {
        None if field == "version" => Ok(1),
        Some(v) => match v.as_u64() {
            Some(0) if field == "version" => Ok(1),
            Some(n) if n >= 1 => Ok(n.min(u32::MAX as u64) as u32),
            _ => Err(LoadError::InvalidDocument {
                detail: format!("{field} is not a positive integer"),
            }),
        },
        None => Err(LoadError::InvalidDocument {
            detail: format!("{field} is missing"),
        }),
    }
}

fn check(doc: &NotebookDoc) -> Result<(), LoadError> {
    let mut seen = HashSet::new();
    for cell in &doc.cells {
        if cell.id.is_empty() || !seen.insert(cell.id.as_str()) {
            return Err(LoadError::InvalidDocument {
                detail: format!("duplicate or empty cell id '{}'", cell.id),
            });
        }
    }
    Ok(())
}

/// Whether SQL changes data, judged for `db` (or every engine).
/// `{{cell}}` references are read as plain table names.
pub fn writes_data(db: Option<DatabaseType>, sql: &str) -> bool {
    let lex = db.unwrap_or_default();
    if mask_sql(lex, sql).trim().is_empty() {
        return false;
    }
    let mut text = String::with_capacity(sql.len());
    let mut last = 0;
    for r in find_references(sql, SourceLanguage::Sql, lex) {
        if let RefTarget::Cell(token) = &r.target {
            text.push_str(&sql[last..r.start]);
            text.push_str(token);
            last = r.end;
        }
    }
    text.push_str(&sql[last..]);
    let verdict = match db {
        Some(db) => classify_select_only_for(db, &text),
        None => classify_select_only(&text),
    };
    !verdict.select_only
}

fn policy_for(kind: &CellKind, source: &str, db: Option<DatabaseType>) -> ExecutionPolicy {
    ExecutionPolicy {
        read_only: !(*kind == CellKind::Sql && writes_data(db, source)),
        ..ExecutionPolicy::default()
    }
}

fn cell_number(id: &str) -> Option<u64> {
    id.strip_prefix("cell_")?.parse().ok()
}

/// Allocates `cell_N` ids after the highest one kept.
struct LegacyIds {
    next: u64,
    taken: HashSet<String>,
}

impl LegacyIds {
    fn new(kept: impl Iterator<Item = String>) -> Self {
        let taken: HashSet<String> = kept.collect();
        let next = taken
            .iter()
            .filter_map(|id| cell_number(id))
            .max()
            .unwrap_or(0)
            + 1;
        Self { next, taken }
    }

    fn alloc(&mut self) -> String {
        loop {
            let id = format!("cell_{}", self.next);
            self.next += 1;
            if self.taken.insert(id.clone()) {
                return id;
            }
        }
    }
}

fn v1_params(params: Option<&Value>) -> Vec<ParameterDef> {
    let pairs: Vec<(String, String)> = params
        .cloned()
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();
    let mut out: Vec<ParameterDef> = Vec::new();
    for (name, value) in pairs {
        let name = name.trim_start_matches('@').to_string();
        if name.is_empty() || out.iter().any(|p| p.name.eq_ignore_ascii_case(&name)) {
            continue;
        }
        let mut def = ParameterDef::new(name, ParamType::String);
        if !value.is_empty() {
            def.default = Some(Value::String(value));
        }
        out.push(def);
    }
    out
}

fn extra_of(object: &Map<String, Value>, known: &[&str]) -> Extra {
    object
        .iter()
        .filter(|(k, _)| !known.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// v1 (`.sqlnb.json`) to v2.
fn v1_to_v2(value: Value, ctx: &mut MigrateContext<'_>) -> Result<Value, LoadError> {
    let object = value.as_object().ok_or(LoadError::NotANotebook)?;
    let raw_cells: Vec<Value> = match object.get("cells") {
        None => Vec::new(),
        Some(Value::Array(cells)) => cells.clone(),
        Some(_) => {
            return Err(LoadError::InvalidDocument {
                detail: "cells is not a list".into(),
            });
        }
    };
    let kept: Vec<Option<String>> = {
        let mut seen = HashSet::new();
        raw_cells
            .iter()
            .map(|c| {
                c.get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty() && seen.insert(id.to_string()))
                    .map(str::to_string)
            })
            .collect()
    };
    let mut legacy = LegacyIds::new(kept.iter().flatten().cloned());
    let mut doc = NotebookDoc::empty(new_notebook_id(ctx.ids), ctx.title.clone());
    for (raw, kept) in raw_cells.iter().zip(kept) {
        let cell_object = raw.as_object().cloned().unwrap_or_default();
        let kind = CellKind::parse(raw.get("kind").and_then(Value::as_str).unwrap_or("sql"));
        let source = raw
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = kept.unwrap_or_else(|| legacy.alloc());
        let mut cell = Cell::new(id.clone(), kind.clone(), policy_for(&kind, &source, ctx.db));
        cell.name = is_identifier(&id).then_some(id);
        cell.source = source;
        cell.collapsed = raw
            .get("collapsed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        cell.extra = extra_of(&cell_object, &["id", "kind", "source", "collapsed"]);
        doc.cells.push(cell);
    }
    if raw_cells.is_empty() {
        let id = legacy.alloc();
        let mut cell = Cell::new(id.clone(), CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some(id);
        doc.cells.push(cell);
    }
    doc.parameters = v1_params(object.get("params"));
    doc.extra = extra_of(object, &["version", "cells", "params", "next_cell_id"]);
    serde_json::to_value(&doc).map_err(|e| LoadError::InvalidDocument {
        detail: e.to_string(),
    })
}

/// nbformat `source`: one string, or a list of lines carrying their `\n`.
fn ipynb_source(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(Value::as_str).collect(),
        _ => String::new(),
    }
}

/// Drop the Jupyter SQL magics (`%%sql` cell magic line, `%sql ` line
/// prefix) that ipython-sql / jupysql put in front of the query.
fn strip_sql_magic(source: &str) -> String {
    let trimmed = source.trim_start();
    if let Some(rest) = trimmed.strip_prefix("%%sql") {
        return match rest.find('\n') {
            Some(newline) => rest[newline + 1..].to_string(),
            None => String::new(),
        };
    }
    source
        .split_inclusive('\n')
        .map(|line| line.strip_prefix("%sql ").unwrap_or(line))
        .collect()
}

fn ipynb_lines(source: &str) -> Vec<Value> {
    source
        .split_inclusive('\n')
        .map(|line| Value::String(line.to_string()))
        .collect()
}

/// Read a Jupyter notebook (nbformat 4.x).
pub fn from_ipynb(
    text: &str,
    ids: &mut dyn IdSource,
    db: Option<DatabaseType>,
) -> Result<NotebookDoc, LoadError> {
    let value: Value = serde_json::from_str(text).map_err(|e| LoadError::InvalidJson {
        detail: e.to_string(),
    })?;
    let mut ctx = MigrateContext {
        ids,
        db,
        title: String::new(),
    };
    from_ipynb_value(&value, &mut ctx)
}

fn from_ipynb_value(root: &Value, ctx: &mut MigrateContext<'_>) -> Result<NotebookDoc, LoadError> {
    let major = root.get("nbformat").and_then(Value::as_u64).unwrap_or(0);
    if major < 4 {
        return Err(LoadError::UnsupportedIpynb { nbformat: major });
    }
    let raw_cells = root
        .get("cells")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let meta = root.get("metadata").and_then(|m| m.get("sqlly"));
    let field = |name: &str| meta.and_then(|m| m.get(name));
    // Keep ids SQLLY wrote so `{{id}}` references survive a round trip: any
    // unique id in a file SQLLY saved, else only ids shaped like SQLLY's.
    let ours = meta.is_some();
    // Since v2 the saved policy is authoritative: a cell without one is
    // read-only, never re-judged (which could only loosen it).
    let policies_saved = field("format_version")
        .and_then(Value::as_u64)
        .is_some_and(|v| v >= 2);
    let mut seen = HashSet::new();
    let kept: Vec<Option<String>> = raw_cells
        .iter()
        .map(|cell| {
            cell.get("id")
                .and_then(Value::as_str)
                .filter(|id| {
                    (is_sqlly_cell_id(id) || (ours && !id.is_empty()))
                        && seen.insert(id.to_string())
                })
                .map(str::to_string)
        })
        .collect();
    let mut legacy = LegacyIds::new(kept.iter().flatten().cloned());
    let notebook_id = field("notebook_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| new_notebook_id(ctx.ids));
    let title = field("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| ctx.title.clone());
    let mut doc = NotebookDoc::empty(notebook_id, title);
    for (raw, kept) in raw_cells.iter().zip(kept) {
        let metadata = raw.get("metadata");
        let sqlly = metadata.and_then(|m| m.get("sqlly"));
        let kind = match sqlly.and_then(|s| s.get("kind")).and_then(Value::as_str) {
            Some(kind) => CellKind::parse(kind),
            None => match raw.get("cell_type").and_then(Value::as_str) {
                Some("code") => CellKind::Sql,
                _ => CellKind::Markdown,
            },
        };
        let mut source = ipynb_source(raw.get("source"));
        if kind == CellKind::Sql {
            source = strip_sql_magic(&source);
        }
        let collapsed = metadata
            .and_then(|m| m.get("collapsed"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || metadata
                .and_then(|m| m.get("jupyter"))
                .and_then(|j| j.get("source_hidden"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let id = kept.unwrap_or_else(|| legacy.alloc());
        let policy = sqlly
            .and_then(|s| s.get("execution_policy"))
            .and_then(|p| serde_json::from_value(p.clone()).ok())
            .unwrap_or_else(|| {
                if policies_saved {
                    ExecutionPolicy::default()
                } else {
                    policy_for(&kind, &source, ctx.db)
                }
            });
        let mut cell = Cell::new(id.clone(), kind, policy);
        cell.name = match sqlly {
            Some(s) => s.get("name").and_then(Value::as_str).map(str::to_string),
            None => Some(id),
        };
        cell.source = source;
        cell.collapsed = collapsed;
        let text_field = |name: &str| {
            sqlly
                .and_then(|s| s.get(name))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        cell.connection_ref = text_field("connection_ref");
        cell.session_group = text_field("session_group");
        cell.spec = sqlly
            .and_then(|s| s.get("spec"))
            .cloned()
            .unwrap_or(Value::Null);
        read_sqlly_extra(sqlly, &mut cell.extra);
        // The collapsed flags are `cell.collapsed`; the rest is kept.
        let mut own = foreign_metadata(metadata, &["sqlly", "collapsed"]);
        if let Some(Value::Object(map)) = own.as_mut() {
            if let Some(Value::Object(jupyter)) = map.get_mut("jupyter") {
                jupyter.remove("source_hidden");
                if jupyter.is_empty() {
                    map.remove("jupyter");
                }
            }
            if map.is_empty() {
                own = None;
            }
        }
        if let Some(own) = own {
            cell.extra.insert(IPYNB_METADATA_KEY.into(), own);
        }
        if sqlly.and_then(|s| s.get("kind")).is_none()
            && raw.get("cell_type").and_then(Value::as_str) == Some("raw")
        {
            cell.extra
                .insert(IPYNB_CELL_TYPE_KEY.into(), Value::String("raw".into()));
        }
        doc.cells.push(cell);
    }
    read_sqlly_extra(meta, &mut doc.extra);
    if let Some(own) = foreign_metadata(
        root.get("metadata"),
        &["sqlly", "kernelspec", "language_info"],
    ) {
        doc.extra.insert(IPYNB_METADATA_KEY.into(), own);
    }
    doc.parameters = decode(meta, "parameters").unwrap_or_else(|| v1_params(field("params")));
    doc.connection_refs = decode(meta, "connection_refs").unwrap_or_default();
    doc.settings = decode(meta, "settings").unwrap_or_default();
    doc.data_retention = decode(meta, "data_retention").unwrap_or_default();
    if doc.cells.is_empty() {
        let id = legacy.alloc();
        let mut cell = Cell::new(id.clone(), CellKind::Sql, ExecutionPolicy::default());
        cell.name = Some(id);
        doc.cells.push(cell);
    }
    Ok(doc)
}

/// Where an imported Jupyter cell's or notebook's own metadata (everything
/// but SQLLY's) is kept, in `extra`, so saving as `.ipynb` writes it back.
pub const IPYNB_METADATA_KEY: &str = "ipynb_metadata";
/// Marks a Markdown cell that was a Jupyter raw cell.
pub const IPYNB_CELL_TYPE_KEY: &str = "ipynb_cell_type";
/// SQLLY's unknown fields, inside `metadata.sqlly`.
const SQLLY_EXTRA: &str = "extra";

fn ipynb_keys(key: &str) -> bool {
    key == IPYNB_METADATA_KEY || key == IPYNB_CELL_TYPE_KEY
}

/// `metadata` without `drop`, as an object, when anything is left.
fn foreign_metadata(metadata: Option<&Value>, drop: &[&str]) -> Option<Value> {
    let mut map = metadata?.as_object()?.clone();
    for key in drop {
        map.remove(*key);
    }
    (!map.is_empty()).then_some(Value::Object(map))
}

/// SQLLY's own unknown fields (not the Jupyter keys) for `metadata.sqlly`.
fn sqlly_extra(extra: &Extra) -> Option<Value> {
    let map: Map<String, Value> = extra
        .iter()
        .filter(|(k, _)| !ipynb_keys(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    (!map.is_empty()).then_some(Value::Object(map))
}

fn read_sqlly_extra(sqlly: Option<&Value>, into: &mut Extra) {
    if let Some(map) = sqlly
        .and_then(|s| s.get(SQLLY_EXTRA))
        .and_then(Value::as_object)
    {
        for (k, v) in map {
            if !ipynb_keys(k) {
                into.insert(k.clone(), v.clone());
            }
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(meta: Option<&Value>, name: &str) -> Option<T> {
    meta.and_then(|m| m.get(name))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
}

fn is_plain(cell: &Cell) -> bool {
    matches!(cell.kind, CellKind::Sql | CellKind::Markdown)
        && cell.name.as_deref() == Some(cell.id.as_str())
        && cell.connection_ref.is_none()
        && cell.session_group.is_none()
        && cell.spec.is_null()
        && cell.execution_policy == ExecutionPolicy::default()
        && cell.extra.keys().all(|k| ipynb_keys(k))
}

/// Write the notebook as nbformat 4.5 with the SQL kernelspec Azure Data
/// Studio uses. Cell ids, names, kinds, policies and parameters ride in
/// `metadata.sqlly` so SQLLY reads them back unchanged; outputs are never
/// written.
pub fn to_ipynb(doc: &NotebookDoc) -> String {
    let cells: Vec<Value> = doc
        .cells
        .iter()
        .map(|cell| {
            let mut metadata = cell
                .extra
                .get(IPYNB_METADATA_KEY)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            metadata.remove("collapsed");
            if let Some(Value::Object(jupyter)) = metadata.get_mut("jupyter") {
                jupyter.remove("source_hidden");
                if jupyter.is_empty() {
                    metadata.remove("jupyter");
                }
            }
            if cell.collapsed {
                match metadata.get_mut("jupyter") {
                    Some(Value::Object(jupyter)) => {
                        jupyter.insert("source_hidden".into(), json!(true));
                    }
                    _ => {
                        metadata.insert("jupyter".into(), json!({ "source_hidden": true }));
                    }
                }
            }
            if !is_plain(cell) {
                let mut sqlly = Map::new();
                if let Some(name) = &cell.name {
                    sqlly.insert("name".into(), json!(name));
                }
                if !matches!(cell.kind, CellKind::Sql | CellKind::Markdown) {
                    sqlly.insert("kind".into(), json!(cell.kind));
                }
                if let Some(slot) = &cell.connection_ref {
                    sqlly.insert("connection_ref".into(), json!(slot));
                }
                if let Some(group) = &cell.session_group {
                    sqlly.insert("session_group".into(), json!(group));
                }
                if cell.execution_policy != ExecutionPolicy::default() {
                    sqlly.insert("execution_policy".into(), json!(cell.execution_policy));
                }
                if !cell.spec.is_null() {
                    sqlly.insert("spec".into(), cell.spec.clone());
                }
                if let Some(extra) = sqlly_extra(&cell.extra) {
                    sqlly.insert(SQLLY_EXTRA.into(), extra);
                }
                metadata.insert("sqlly".into(), Value::Object(sqlly));
            }
            let raw_cell =
                cell.extra.get(IPYNB_CELL_TYPE_KEY).and_then(Value::as_str) == Some("raw");
            match cell.kind {
                CellKind::Sql => json!({
                    "cell_type": "code",
                    "execution_count": null,
                    "id": cell.id,
                    "metadata": metadata,
                    "outputs": [],
                    "source": ipynb_lines(&cell.source),
                }),
                CellKind::Markdown if !raw_cell => json!({
                    "cell_type": "markdown",
                    "id": cell.id,
                    "metadata": metadata,
                    "source": ipynb_lines(&cell.source),
                }),
                _ => json!({
                    "cell_type": "raw",
                    "id": cell.id,
                    "metadata": metadata,
                    "source": ipynb_lines(&cell.source),
                }),
            }
        })
        .collect();
    let legacy_params: Vec<(String, String)> = doc
        .parameters
        .iter()
        .map(|p| {
            let value = match &p.default {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            (
                p.name.clone(),
                if p.is_redacted() {
                    String::new()
                } else {
                    value
                },
            )
        })
        .collect();
    let mut sqlly = json!({
        "format_version": FORMAT_VERSION,
        "notebook_id": doc.notebook_id,
        "title": doc.title,
        "params": legacy_params,
        "parameters": doc.parameters,
    });
    if !doc.connection_refs.is_empty() {
        sqlly["connection_refs"] = json!(doc.connection_refs);
    }
    if doc.settings != Default::default() {
        sqlly["settings"] = json!(doc.settings);
    }
    if doc.data_retention != Default::default() {
        sqlly["data_retention"] = json!(doc.data_retention);
    }
    if let Some(extra) = sqlly_extra(&doc.extra) {
        sqlly[SQLLY_EXTRA] = extra;
    }
    let mut metadata = doc
        .extra
        .get(IPYNB_METADATA_KEY)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    metadata.insert(
        "kernelspec".into(),
        json!({ "display_name": "SQL", "language": "sql", "name": "SQL" }),
    );
    metadata.insert(
        "language_info".into(),
        json!({ "name": "sql", "version": "" }),
    );
    metadata.insert("sqlly".into(), sqlly);
    let root = json!({
        "cells": cells,
        "metadata": metadata,
        "nbformat": 4,
        "nbformat_minor": 5,
    });
    let mut out = serde_json::to_string_pretty(&super::document::Sorted(&root))
        .unwrap_or_else(|_| "{}".into());
    out.push('\n');
    out
}

/// Encode `doc` for `path`: Jupyter for `.ipynb`, the canonical `.sqllynb`
/// JSON otherwise.
pub fn save(path: &str, doc: &NotebookDoc) -> String {
    if FileFormat::from_path(path) == Some(FileFormat::Ipynb) {
        to_ipynb(doc)
    } else {
        doc.to_canonical_json()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::SeededIds;

    fn load_text(path: &str, text: &str) -> Result<Loaded, LoadError> {
        load(Some(path), text, &mut SeededIds(1), None)
    }

    const ADS_FIXTURE: &str = r##"{
 "metadata": {
  "kernelspec": { "name": "SQL", "display_name": "SQL", "language": "sql" },
  "language_info": { "name": "sql", "version": "" }
 },
 "nbformat_minor": 2,
 "nbformat": 4,
 "cells": [
  { "cell_type": "markdown", "source": ["# Sales\n", "Top customers"], "metadata": { "azdata_cell_guid": "a1" } },
  { "cell_type": "code", "source": ["SELECT TOP 10 *\n", "FROM dbo.Customers;"], "metadata": { "azdata_cell_guid": "b2" },
    "outputs": [ { "output_type": "execute_result", "data": {} } ], "execution_count": 3 }
 ]
}"##;

    const V1_FIXTURE: &str = r#"{
  "version": 1,
  "cells": [
    {"id": "cell_1", "kind": "sql", "source": "SELECT * FROM t WHERE id = @id", "collapsed": false},
    {"id": "cell_2", "kind": "markdown", "source": "Notes", "collapsed": true},
    {"id": "cell_4", "kind": "sql", "source": "UPDATE t SET x = 1 WHERE id IN (SELECT id FROM {{cell_1}})"},
    {"id": "cell_5", "kind": "sql", "source": "-- just a comment"}
  ],
  "params": [["id", "7"], ["@region", ""]],
  "next_cell_id": 6,
  "owner_note": {"k": 1}
}"#;

    #[test]
    fn v1_notebooks_migrate_with_ids_names_policies_and_params() {
        let loaded = load_text("/x/Sales Review.sqlnb.json", V1_FIXTURE).unwrap();
        assert_eq!(loaded.source_format, FileFormat::LegacyV1);
        assert_eq!(loaded.migrated_from, Some(1));
        let doc = loaded.doc;
        assert_eq!(doc.format_version, 2);
        assert_eq!(doc.title, "Sales Review");
        let ids: Vec<&str> = doc.cells.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cell_1", "cell_2", "cell_4", "cell_5"]);
        assert!(
            doc.cells
                .iter()
                .all(|c| c.name.as_deref() == Some(c.id.as_str()))
        );
        assert!(doc.cells[0].execution_policy.read_only);
        assert!(doc.cells[1].collapsed);
        assert!(
            !doc.cells[2].execution_policy.read_only,
            "write cells keep working"
        );
        assert!(
            doc.cells[3].execution_policy.read_only,
            "a comment is not a write"
        );
        assert_eq!(doc.parameters.len(), 2);
        assert_eq!(doc.parameters[0].name, "id");
        assert_eq!(doc.parameters[0].default, Some(Value::String("7".into())));
        assert_eq!(doc.parameters[1].name, "region");
        assert_eq!(doc.parameters[1].default, None);
        assert_eq!(doc.extra["owner_note"]["k"], 1);
        assert!(!doc.extra.contains_key("next_cell_id"));
        // Saving and reloading is lossless and does not migrate again.
        let saved = doc.to_canonical_json();
        let again = load_text("/x/a.sqllynb", &saved).unwrap();
        assert_eq!(again.migrated_from, None);
        assert_eq!(again.doc, doc);
    }

    #[test]
    fn v1_write_detection_is_dialect_aware() {
        let text =
            r#"{"version":1,"cells":[{"id":"cell_1","kind":"sql","source":"SELECT 1 AS `x`"}]}"#;
        let mysql = load(None, text, &mut SeededIds(1), Some(DatabaseType::MySql)).unwrap();
        assert!(mysql.doc.cells[0].execution_policy.read_only);
        let text =
            r#"{"version":1,"cells":[{"id":"cell_1","kind":"sql","source":"EXEC dbo.purge"}]}"#;
        let ms = load(None, text, &mut SeededIds(1), Some(DatabaseType::SqlServer)).unwrap();
        assert!(!ms.doc.cells[0].execution_policy.read_only);
        assert!(writes_data(None, "DELETE FROM t"));
        assert!(!writes_data(None, "SELECT * FROM {{cell_1}}"));
        assert!(!writes_data(Some(DatabaseType::Postgres), "  "));
    }

    #[test]
    fn v1_edge_shapes() {
        let doc = load_text("n.sqlnb.json", r#"{"version":0}"#).unwrap().doc;
        assert_eq!(doc.cells.len(), 1);
        assert_eq!(doc.cells[0].id, "cell_1");
        let doc = load_text("n.sqlnb.json", r#"{"cells":[{"kind":"sql","source":"SELECT 1"},{"id":"cell_3","kind":"sql"},{"id":"cell_3","kind":"markdown"}]}"#)
            .unwrap()
            .doc;
        let ids: Vec<&str> = doc.cells.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cell_4", "cell_3", "cell_5"]);
        let doc = load_text(
            "n.sqlnb.json",
            r#"{"version":1,"cells":[{"id":"x","kind":"chart"}]}"#,
        )
        .unwrap()
        .doc;
        assert_eq!(doc.cells[0].kind, CellKind::Chart);
    }

    #[test]
    fn rejects_newer_versions_and_garbage() {
        let v1_newer = r#"{"version":99,"cells":[],"params":[]}"#;
        assert_eq!(
            load_text("a.sqlnb.json", v1_newer).unwrap_err(),
            LoadError::NewerFormat {
                found: 99,
                supported: 2
            }
        );
        let v3 = r#"{"format":"sqlly.notebook","format_version":3,"notebook_id":"n"}"#;
        let err = load_text("a.sqllynb", v3).unwrap_err();
        assert_eq!(
            err,
            LoadError::NewerFormat {
                found: 3,
                supported: 2
            }
        );
        assert_eq!(err.message().key(), Some("notebook.core.load.newer_format"));
        assert!(matches!(
            load_text("a.sqllynb", "not json"),
            Err(LoadError::InvalidJson { .. })
        ));
        assert_eq!(
            load_text("a.txt", "[1,2]").unwrap_err(),
            LoadError::NotANotebook
        );
        assert_eq!(
            load_text("a.txt", r#"{"x":1}"#).unwrap_err(),
            LoadError::NotANotebook
        );
        let dup = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","cells":[{"id":"a","kind":"sql"},{"id":"a","kind":"sql"}]}"#;
        assert!(matches!(
            load_text("a.sqllynb", dup),
            Err(LoadError::InvalidDocument { .. })
        ));
        let bad = r#"{"format":"sqlly.notebook","format_version":"two","notebook_id":"n"}"#;
        assert!(matches!(
            load_text("a.sqllynb", bad),
            Err(LoadError::InvalidDocument { .. })
        ));
    }

    #[test]
    fn content_wins_over_extension() {
        let v2 = NotebookDoc::new("t", &mut SeededIds(3)).to_canonical_json();
        assert_eq!(
            load_text("mislabeled.ipynb", &v2).unwrap().source_format,
            FileFormat::SqllyNotebook
        );
        assert_eq!(
            load_text("mislabeled.sqllynb", ADS_FIXTURE)
                .unwrap()
                .source_format,
            FileFormat::Ipynb
        );
    }

    #[test]
    fn local_cells_named_by_slot_load_with_the_spec_target() {
        let text = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n",
            "connection_refs":[{"id":"local","label":"Local"},{"id":"sales","label":"Sales"}],
            "cells":[{"id":"c_aaaaaaaaaa","kind":"sql","connection_ref":"local","source":"SELECT 1"},
                     {"id":"c_bbbbbbbbbb","kind":"sql","connection_ref":"sales","source":"SELECT 2"}]}"#;
        let doc = load_text("a.sqllynb", text).unwrap().doc;
        assert_eq!(doc.cells[0].connection_ref, None);
        assert_eq!(doc.cells[0].spec["target"], "local");
        assert_eq!(doc.cells[1].connection_ref.as_deref(), Some("sales"));
        let slots: Vec<&str> = doc.connection_refs.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(slots, ["sales"]);
        let saved = doc.to_canonical_json();
        assert!(!saved.contains("\"connection_ref\": \"local\""), "{saved}");
    }

    #[test]
    fn unknown_fields_and_kinds_survive_v2_round_trips() {
        let text = r#"{"format":"sqlly.notebook","format_version":2,"notebook_id":"n","title":"T",
            "future":{"b":2,"a":1},
            "cells":[{"id":"c_aaaaaaaaaa","kind":"sankey","source":"","spec":{"z":1,"a":[1,2]},"widget":{"w":1},
                      "execution_policy":{"read_only":true,"cache":{"mode":"pinned"},"budget":5}}]}"#;
        let doc = load_text("a.sqllynb", text).unwrap().doc;
        assert_eq!(doc.cells[0].kind, CellKind::Unknown("sankey".into()));
        assert_eq!(doc.cells[0].execution_policy.extra["budget"], 5);
        let out = doc.to_canonical_json();
        let again = load_text("a.sqllynb", &out).unwrap().doc;
        assert_eq!(again, doc);
        assert_eq!(again.to_canonical_json(), out);
        assert!(
            out.contains("\"sankey\"") && out.contains("\"widget\"") && out.contains("\"future\"")
        );
    }

    // Ported from the v1 ipynb tests.
    #[test]
    fn reads_azure_data_studio_notebook() {
        let loaded =
            load_text(r"C:\Users\me\Documents\Notebooks\sales.ipynb", ADS_FIXTURE).unwrap();
        let doc = loaded.doc;
        assert_eq!(loaded.source_format, FileFormat::Ipynb);
        assert_eq!(doc.title, "sales");
        assert_eq!(doc.cells.len(), 2);
        assert_eq!(doc.cells[0].kind, CellKind::Markdown);
        assert_eq!(doc.cells[0].source, "# Sales\nTop customers");
        assert_eq!(doc.cells[1].kind, CellKind::Sql);
        assert_eq!(doc.cells[1].source, "SELECT TOP 10 *\nFROM dbo.Customers;");
        assert_eq!(doc.cells[0].id, "cell_1");
        assert_eq!(doc.cells[1].id, "cell_2");
        assert_eq!(doc.cells[1].name.as_deref(), Some("cell_2"));
        assert!(doc.cells[1].execution_policy.read_only);
    }

    // https://github.com/microsoft/azuredatastudio/issues/26153
    #[test]
    fn python_kernel_notebooks_open_as_sql_and_save_with_the_sql_kernel() {
        let python_kernel = r#"{"nbformat": 4, "nbformat_minor": 4,
            "metadata": {"kernelspec": {"name": "python3", "display_name": "Python 3", "language": "python"}},
            "cells": [{"cell_type": "code", "metadata": {}, "source": ["%%sql\n", "SELECT 1"], "outputs": [], "execution_count": null}]}"#;
        let doc = load_text("n.ipynb", python_kernel).unwrap().doc;
        assert_eq!(doc.cells[0].kind, CellKind::Sql);
        assert_eq!(doc.cells[0].source, "SELECT 1");
        let saved: Value = serde_json::from_str(&save("n.ipynb", &doc)).unwrap();
        assert_eq!(saved["metadata"]["kernelspec"]["name"], "SQL");
        assert_eq!(saved["metadata"]["kernelspec"]["language"], "sql");
        assert!(!saved.to_string().to_ascii_lowercase().contains("python"));
    }

    #[test]
    fn strips_jupyter_sql_magics_and_reads_string_sources() {
        let text = r#"{"nbformat": 4, "nbformat_minor": 5, "metadata": {}, "cells": [
            {"cell_type": "code", "id": "x", "metadata": {"jupyter": {"source_hidden": true}}, "source": "%%sql\nSELECT 1", "outputs": [], "execution_count": null},
            {"cell_type": "code", "id": "y", "metadata": {}, "source": "%sql SELECT 2\n", "outputs": [], "execution_count": null},
            {"cell_type": "raw", "id": "z", "metadata": {}, "source": "plain"},
            {"cell_type": "code", "id": "w", "metadata": {"collapsed": true}, "source": "DELETE FROM t", "outputs": []}
        ]}"#;
        let doc = from_ipynb(text, &mut SeededIds(1), None).unwrap();
        assert_eq!(doc.cells[0].source, "SELECT 1");
        assert!(doc.cells[0].collapsed);
        assert_eq!(doc.cells[1].source, "SELECT 2\n");
        assert_eq!(doc.cells[2].kind, CellKind::Markdown);
        assert_eq!(doc.cells[2].source, "plain");
        assert!(doc.cells[3].collapsed);
        assert!(!doc.cells[3].execution_policy.read_only);
    }

    #[test]
    fn rejects_old_nbformat() {
        assert_eq!(
            load_text("a.ipynb", r#"{"nbformat": 3, "worksheets": []}"#).unwrap_err(),
            LoadError::UnsupportedIpynb { nbformat: 3 }
        );
        assert!(from_ipynb("not json", &mut SeededIds(1), None).is_err());
    }

    #[test]
    fn ipynb_round_trips_v1_style_cells_and_params() {
        let doc = load_text("a.sqlnb.json", V1_FIXTURE).unwrap().doc;
        let text = to_ipynb(&doc);
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["nbformat"], 4);
        assert_eq!(value["nbformat_minor"], 5);
        assert_eq!(
            value["cells"][0]["source"][0],
            "SELECT * FROM t WHERE id = @id"
        );
        assert_eq!(value["cells"][0]["outputs"], json!([]));
        assert!(
            value["cells"][0]["metadata"].get("sqlly").is_none(),
            "plain cells stay plain"
        );
        assert_eq!(
            value["cells"][1]["metadata"]["jupyter"]["source_hidden"],
            true
        );
        assert_eq!(
            value["metadata"]["sqlly"]["params"],
            json!([["id", "7"], ["region", ""]])
        );
        let back = from_ipynb(&text, &mut SeededIds(9), None).unwrap();
        assert_eq!(back.cells, doc.cells);
        assert_eq!(back.parameters, doc.parameters);
        assert_eq!(back.notebook_id, doc.notebook_id);
        assert_eq!(to_ipynb(&back), text);
    }

    #[test]
    fn ipynb_round_trips_v2_cells() {
        let mut ids = SeededIds(4);
        let mut doc = NotebookDoc::new("Revenue", &mut ids);
        doc.cells[0].source = "SELECT 1 AS n".into();
        doc.cells[0].name = Some("base".into());
        let i = doc.add_cell(CellKind::Chart, None, &mut ids);
        doc.cells[i].spec = json!({"input": doc.cells[0].id.clone(), "kind": "bar"});
        let j = doc.add_cell(CellKind::Sql, None, &mut ids);
        doc.cells[j].execution_policy.read_only = false;
        doc.cells[j].session_group = Some("tx".into());
        doc.cells[j].connection_ref = Some("primary".into());
        doc.add_cell(CellKind::Markdown, None, &mut ids);
        let mut p = ParameterDef::new("token", ParamType::SecretRef);
        p.default = Some(json!("vault.key"));
        doc.parameters.push(p);
        let text = to_ipynb(&doc);
        assert!(
            !text.contains("\"vault.key\"]"),
            "secrets stay out of the legacy params"
        );
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["cells"][1]["cell_type"], "raw");
        let back = from_ipynb(&text, &mut SeededIds(9), None).unwrap();
        assert_eq!(back.cells, doc.cells);
        assert_eq!(back.parameters, doc.parameters);
        assert_eq!(back.title, "Revenue");
    }

    #[test]
    fn saved_ipynb_policies_are_never_loosened_on_import() {
        let mut ids = SeededIds(8);
        let mut doc = NotebookDoc::new("t", &mut ids);
        doc.cells[0].source = "DELETE FROM t".into();
        doc.cells[0].name = Some(doc.cells[0].id.clone());
        let text = to_ipynb(&doc);
        assert!(
            !text.contains("execution_policy"),
            "the default policy is not written"
        );
        let back = from_ipynb(&text, &mut SeededIds(1), Some(DatabaseType::SqlServer)).unwrap();
        assert!(back.cells[0].execution_policy.read_only);
        // A file from another tool (or older SQLLY) is judged by its SQL.
        let foreign = text.replace("\"format_version\": 2,", "");
        let back = from_ipynb(&foreign, &mut SeededIds(1), Some(DatabaseType::SqlServer)).unwrap();
        assert!(!back.cells[0].execution_policy.read_only);
    }

    #[test]
    fn titles_from_paths() {
        assert_eq!(title_from_path("/a/b/Q3 Review.sqllynb"), "Q3 Review");
        assert_eq!(title_from_path(r"C:\x\old.SQLNB.JSON"), "old");
        assert_eq!(title_from_path("n.ipynb"), "n");
        assert_eq!(title_from_path("plain"), "plain");
    }

    #[test]
    fn foreign_jupyter_metadata_and_raw_cells_survive_import_and_export() {
        let text = json!({
            "cells": [
                {"cell_type": "code", "id": "p1", "execution_count": null, "outputs": [],
                 "metadata": {"tags": ["parameters"], "azdata_cell_guid": "6f1c"},
                 "source": "SELECT 1"},
                {"cell_type": "raw", "id": "r1", "metadata": {"format": "text/x-rst"},
                 "source": ".. note:: kept as written"},
                {"cell_type": "markdown", "id": "m1",
                 "metadata": {"jupyter": {"source_hidden": true, "outputs_hidden": true}},
                 "source": "# Notes"}
            ],
            "metadata": {
                "kernelspec": {"name": "python3", "display_name": "Python 3"},
                "papermill": {"parameters": {"region": "west"}},
                "azdata": {"version": "1.0"}
            },
            "nbformat": 4, "nbformat_minor": 5
        })
        .to_string();
        let doc = from_ipynb(&text, &mut SeededIds(1), None).unwrap();
        assert_eq!(doc.cells[1].kind, CellKind::Markdown);
        assert!(doc.cells[2].collapsed);
        let out: Value = serde_json::from_str(&to_ipynb(&doc)).unwrap();
        let cells = out["cells"].as_array().unwrap();
        assert_eq!(cells[0]["metadata"]["tags"], json!(["parameters"]));
        assert_eq!(cells[0]["metadata"]["azdata_cell_guid"], "6f1c");
        assert_eq!(cells[1]["cell_type"], "raw", "a raw cell stays raw");
        assert_eq!(cells[1]["metadata"]["format"], "text/x-rst");
        assert_eq!(cells[1]["source"], json!([".. note:: kept as written"]));
        assert_eq!(
            cells[2]["metadata"]["jupyter"],
            json!({"outputs_hidden": true, "source_hidden": true})
        );
        assert_eq!(out["metadata"]["papermill"]["parameters"]["region"], "west");
        assert_eq!(out["metadata"]["azdata"]["version"], "1.0");
        assert_eq!(out["metadata"]["kernelspec"]["name"], "SQL");

        // Expanding a hidden cell clears the flag rather than keeping the
        // imported one.
        let mut doc = doc;
        doc.cells[2].collapsed = false;
        let out: Value = serde_json::from_str(&to_ipynb(&doc)).unwrap();
        assert_eq!(
            out["cells"][2]["metadata"]["jupyter"],
            json!({"outputs_hidden": true})
        );
    }

    #[test]
    fn values_from_a_newer_sqlly_never_make_a_notebook_unopenable() {
        let text = json!({
            "format": "sqlly.notebook",
            "format_version": 2,
            "notebook_id": "nb_newer",
            "title": "Newer",
            "settings": {"run_mode": "presentation"},
            "parameters": [{
                "id": "region", "name": "region", "type": "date",
                "scope": "team", "reactivity": "debounced",
                "presets": ["today", "last_14_days", {"kind": "fiscal_year"}],
                "allowed_values": {"from_query": "SELECT 1"}
            }],
            "cells": [{
                "id": "c_1", "kind": "sql", "source": "SELECT 1",
                "execution_policy": {"read_only": true, "cache": {"mode": "sliding", "seconds": 30}}
            }]
        });
        let loaded = load_text("n.sqllynb", &text.to_string()).expect("opens");
        let doc = &loaded.doc;
        assert!(matches!(
            doc.settings.run_mode,
            super::super::document::RunMode::Unknown(_)
        ));
        let def = &doc.parameters[0];
        assert!(
            def.reactivity.waits_for_apply(),
            "an unknown reactivity never auto-runs"
        );
        assert!(matches!(
            def.presets[1],
            super::super::params::Preset::Unknown(_)
        ));
        assert!(
            def.allowed_values
                .as_ref()
                .unwrap()
                .static_values()
                .is_empty()
        );
        let saved: Value = serde_json::from_str(&doc.to_canonical_json()).unwrap();
        assert_eq!(saved["settings"]["run_mode"], "presentation");
        assert_eq!(saved["parameters"][0]["scope"], "team");
        assert_eq!(saved["parameters"][0]["reactivity"], "debounced");
        assert_eq!(
            saved["parameters"][0]["presets"],
            text["parameters"][0]["presets"]
        );
        assert_eq!(
            saved["parameters"][0]["allowed_values"],
            text["parameters"][0]["allowed_values"]
        );
        assert_eq!(
            saved["cells"][0]["execution_policy"]["cache"],
            json!({"mode": "sliding", "seconds": 30})
        );
        // Through Jupyter too.
        let back = from_ipynb(&to_ipynb(doc), &mut SeededIds(2), None).unwrap();
        assert_eq!(back.settings, doc.settings);
        assert_eq!(back.parameters, doc.parameters);
        assert_eq!(
            back.cells[0].execution_policy,
            doc.cells[0].execution_policy
        );
    }

    #[test]
    fn randomized_documents_round_trip_through_ipynb() {
        for seed in 0..300 {
            let doc = crate::notebook::document::tests::random_doc(seed);
            if doc.cells.is_empty() {
                continue;
            }
            let text = to_ipynb(&doc);
            let back = from_ipynb(&text, &mut SeededIds(seed), None).unwrap();
            assert_eq!(back.cells, doc.cells, "seed {seed}");
            assert_eq!(back.extra, doc.extra, "seed {seed}");
            assert_eq!(back.title, doc.title);
            assert_eq!(to_ipynb(&back), text);
        }
    }
}
