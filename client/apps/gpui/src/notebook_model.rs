//! Pure notebook document model: cells, `{{cell_N}}` references, `@param`
//! globals, CTE expansion, cycle detection, and the on-disk JSON format.
//! No gpui types — unit-tested without a window (`notebook_view.rs` renders it).

use serde::{Deserialize, Serialize};

pub const NOTEBOOK_FORMAT_VERSION: u32 = 1;
pub const NOTEBOOK_EXTENSION: &str = "sqlnb.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CellKind {
    Sql,
    Markdown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cell {
    /// Stable id (`cell_1`, `cell_2`, …) allocated from `next_cell_id`; never reused after delete so references stay unambiguous.
    pub id: String,
    pub kind: CellKind,
    pub source: String,
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notebook {
    pub version: u32,
    pub cells: Vec<Cell>,
    /// `@name` globals: (name, value). Values are substituted with `query_params::substitute_params` typed as text.
    pub params: Vec<(String, String)>,
    next_cell_id: u32,
}

impl Default for Notebook {
    fn default() -> Self {
        Self::new()
    }
}

impl Notebook {
    pub fn new() -> Self {
        let mut nb = Self {
            version: NOTEBOOK_FORMAT_VERSION,
            cells: Vec::new(),
            params: Vec::new(),
            next_cell_id: 1,
        };
        nb.add_cell(CellKind::Sql, None);
        nb
    }

    fn alloc_id(&mut self) -> String {
        let id = format!("cell_{}", self.next_cell_id);
        self.next_cell_id += 1;
        id
    }

    /// Insert a cell after `after` (or at the end when `None`). Returns the new index.
    pub fn add_cell(&mut self, kind: CellKind, after: Option<usize>) -> usize {
        let cell = Cell {
            id: self.alloc_id(),
            kind,
            source: String::new(),
            collapsed: false,
        };
        let index = match after {
            Some(i) if i < self.cells.len() => i + 1,
            _ => self.cells.len(),
        };
        self.cells.insert(index, cell);
        index
    }

    pub fn remove_cell(&mut self, index: usize) -> Option<Cell> {
        if index < self.cells.len() {
            Some(self.cells.remove(index))
        } else {
            None
        }
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

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".into())
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        let mut nb: Notebook = serde_json::from_str(text).map_err(|e| {
            crate::localization::format(
                "editor.notebook.invalid_file",
                &[("error", &e.to_string())],
            )
        })?;
        if nb.version > NOTEBOOK_FORMAT_VERSION {
            return Err(crate::localization::text("editor.notebook.newer_version"));
        }
        if nb.version == 0 {
            nb.version = NOTEBOOK_FORMAT_VERSION;
        }
        let max_id = nb
            .cells
            .iter()
            .filter_map(|c| parse_cell_num(&c.id))
            .max()
            .unwrap_or(0);
        if nb.next_cell_id <= max_id {
            nb.next_cell_id = max_id + 1;
        }
        Ok(nb)
    }
}

fn parse_cell_num(id: &str) -> Option<u32> {
    id.strip_prefix("cell_")?.parse().ok()
}

fn is_cell_ident(ident: &str) -> bool {
    let Some(rest) = ident.strip_prefix("cell_") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
}

/// `{{cell_N}}` references found in `source`, in order of first appearance, deduplicated.
pub fn references(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                let ident = &source[start..j];
                if is_cell_ident(ident) && !out.iter().any(|s| s == ident) {
                    out.push(ident.to_string());
                }
                i = j + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    UnknownCell(String),
    NotSql(String),
    Cycle(Vec<String>),
    /// A referenced cell is not a single SELECT (contains `;` between statements, or `contains_non_select_statement`).
    NotSelect(String),
    SelfReference(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Color {
    White,
    Grey,
    Black,
}

/// Transitive dependencies of `cell_id` in execution order (dependencies before dependents, deterministic), excluding the cell itself.
pub fn dependency_order(nb: &Notebook, cell_id: &str) -> Result<Vec<String>, ExpandError> {
    if nb.cell(cell_id).is_none() {
        return Err(ExpandError::UnknownCell(cell_id.to_string()));
    }
    let mut color: Vec<Color> = vec![Color::White; nb.cells.len()];
    let mut stack: Vec<String> = Vec::new();
    let mut order: Vec<String> = Vec::new();
    dfs(nb, cell_id, &mut color, &mut stack, &mut order)?;
    order.retain(|id| id != cell_id);
    Ok(order)
}

fn dfs(
    nb: &Notebook,
    id: &str,
    color: &mut [Color],
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
) -> Result<(), ExpandError> {
    let Some(idx) = nb.cell_index(id) else {
        return Err(ExpandError::UnknownCell(id.to_string()));
    };
    match color[idx] {
        Color::Grey => {
            let mut path: Vec<String> = stack
                .iter()
                .skip_while(|s| s.as_str() != id)
                .cloned()
                .collect();
            path.push(id.to_string());
            return Err(ExpandError::Cycle(path));
        }
        Color::Black => return Ok(()),
        Color::White => {}
    }
    let cell = &nb.cells[idx];
    if cell.kind != CellKind::Sql && !stack.is_empty() {
        return Err(ExpandError::NotSql(id.to_string()));
    }
    color[idx] = Color::Grey;
    stack.push(id.to_string());
    for r in references(&cell.source) {
        if r == id {
            return Err(ExpandError::SelfReference(id.to_string()));
        }
        dfs(nb, &r, color, stack, order)?;
    }
    stack.pop();
    color[idx] = Color::Black;
    order.push(id.to_string());
    Ok(())
}

fn strip_trailing_semi(sql: &str) -> &str {
    sql.trim().trim_end_matches(';').trim()
}

fn has_internal_semicolon(sql: &str) -> bool {
    strip_trailing_semi(sql).contains(';')
}

fn starts_with_with(sql: &str) -> bool {
    let s = sql.trim_start();
    s.len() >= 4
        && s[..4].eq_ignore_ascii_case("with")
        && s[4..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == '(')
}

/// Split `WITH cte1 AS (...), cte2 AS (...) <rest>` into CTE items and remainder.
/// Returns `None` when the leading WITH cannot be parsed.
fn split_leading_with(sql: &str) -> Option<(Vec<String>, String)> {
    let s = sql.trim_start();
    if !starts_with_with(s) {
        return None;
    }
    let after_with = s[4..].trim_start();
    let mut ctes = Vec::new();
    let mut rest = after_with;
    loop {
        rest = rest.trim_start();
        let name_len = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '#')
            .count();
        if name_len == 0 {
            return None;
        }
        let name = &rest[..name_len];
        rest = rest[name_len..].trim_start();
        if rest.len() < 2 || !rest[..2].eq_ignore_ascii_case("as") {
            return None;
        }
        let after_as = rest[2..].trim_start();
        if !after_as.starts_with('(') {
            return None;
        }
        let (body, after_body) = take_balanced_parens(after_as)?;
        ctes.push(format!("{name} AS {body}"));
        rest = after_body.trim_start();
        if rest.starts_with(',') {
            rest = rest[1..].trim_start();
            continue;
        }
        if rest.is_empty() {
            return None;
        }
        return Some((ctes, rest.to_string()));
    }
}

fn take_balanced_parens(s: &str) -> Option<(&str, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'"' {
                if bytes.get(i + 1) == Some(&b'"') {
                    i += 2;
                    continue;
                }
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((&s[..=i], &s[i + 1..]));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn replace_refs(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut last = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'{' && bytes[i + 1] == b'{' {
            let start = i + 2;
            let mut j = start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            if j + 1 < bytes.len() && bytes[j] == b'}' && bytes[j + 1] == b'}' {
                let ident = &source[start..j];
                if is_cell_ident(ident) {
                    out.push_str(&source[last..i]);
                    out.push_str(ident);
                    last = j + 2;
                    i = j + 2;
                    continue;
                }
            }
        }
        i += 1;
    }
    out.push_str(&source[last..]);
    out
}

fn cte_parts_for_cell(
    nb: &Notebook,
    cell_id: &str,
    is_select: &dyn Fn(&str) -> bool,
) -> Result<Vec<String>, ExpandError> {
    let cell = nb
        .cell(cell_id)
        .ok_or_else(|| ExpandError::UnknownCell(cell_id.to_string()))?;
    if cell.kind != CellKind::Sql {
        return Err(ExpandError::NotSql(cell_id.to_string()));
    }
    let trimmed = strip_trailing_semi(&cell.source);
    if has_internal_semicolon(&cell.source) || !is_select(trimmed) {
        return Err(ExpandError::NotSelect(cell_id.to_string()));
    }
    let rewritten = replace_refs(trimmed);
    if starts_with_with(trimmed) {
        let Some((inner, remainder)) = split_leading_with(&rewritten) else {
            return Err(ExpandError::NotSelect(cell_id.to_string()));
        };
        if remainder.trim().is_empty() || !is_select(&remainder) {
            return Err(ExpandError::NotSelect(cell_id.to_string()));
        }
        let mut parts = inner;
        parts.push(format!("{cell_id} AS ({remainder})"));
        Ok(parts)
    } else {
        Ok(vec![format!("{cell_id} AS ({rewritten})")])
    }
}

/// Rewrite `source` so every `{{cell_N}}` becomes a CTE.
pub fn expand_sql(
    nb: &Notebook,
    cell_id: &str,
    is_select: &dyn Fn(&str) -> bool,
) -> Result<String, ExpandError> {
    let cell = nb
        .cell(cell_id)
        .ok_or_else(|| ExpandError::UnknownCell(cell_id.to_string()))?;
    if cell.kind != CellKind::Sql {
        return Err(ExpandError::NotSql(cell_id.to_string()));
    }
    let refs = references(&cell.source);
    if refs.iter().any(|r| r == cell_id) {
        return Err(ExpandError::SelfReference(cell_id.to_string()));
    }
    if refs.is_empty() {
        return Ok(cell.source.clone());
    }
    let order = dependency_order(nb, cell_id)?;
    let mut cte_parts = Vec::new();
    for dep_id in &order {
        cte_parts.extend(cte_parts_for_cell(nb, dep_id, is_select)?);
    }
    let rewritten = replace_refs(strip_trailing_semi(&cell.source));
    Ok(format!("WITH {} {}", cte_parts.join(", "), rewritten))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunAllSummary {
    pub ran: usize,
    pub skipped: usize,
    pub failed: Option<String>,
}

/// Cells to run, in document order, SQL only; markdown cells are skipped.
pub fn run_all_order(nb: &Notebook) -> Vec<String> {
    nb.cells
        .iter()
        .filter(|c| c.kind == CellKind::Sql)
        .map(|c| c.id.clone())
        .collect()
}

pub struct MarkdownCellResult<'a> {
    pub cell_id: &'a str,
    pub table: Option<String>,
}

/// `# <title>` + each cell: markdown source verbatim; SQL as a ```sql fence followed by the result table when one is supplied.
pub fn export_markdown(nb: &Notebook, title: &str, results: &[MarkdownCellResult<'_>]) -> String {
    let mut out = format!("# {title}\n");
    for cell in &nb.cells {
        out.push('\n');
        match cell.kind {
            CellKind::Markdown => {
                out.push_str(&cell.source);
                if !cell.source.ends_with('\n') {
                    out.push('\n');
                }
            }
            CellKind::Sql => {
                out.push_str("```sql\n");
                out.push_str(&cell.source);
                if !cell.source.ends_with('\n') {
                    out.push('\n');
                }
                out.push_str("```\n");
                if let Some(table) = results
                    .iter()
                    .find(|r| r.cell_id == cell.id)
                    .and_then(|r| r.table.as_deref())
                {
                    out.push('\n');
                    out.push_str(table);
                    if !table.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }
    }
    out
}

/// File extension of Jupyter notebooks (nbformat 4), the format Azure Data
/// Studio, VS Code, and Jupyter save SQL notebooks in.
pub const IPYNB_EXTENSION: &str = "ipynb";

pub fn is_ipynb_path(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(IPYNB_EXTENSION))
}

/// nbformat `source`: one string, or a list of lines that already carry their `\n`.
fn ipynb_source(value: Option<&serde_json::Value>) -> String {
    match value {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(lines)) => lines.iter().filter_map(|l| l.as_str()).collect(),
        _ => String::new(),
    }
}

/// Drops the Jupyter SQL magics (`%%sql` cell magic line, `%sql ` line prefix)
/// that ipython-sql / jupysql notebooks put in front of the query.
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

fn ipynb_lines(source: &str) -> Vec<serde_json::Value> {
    source
        .split_inclusive('\n')
        .map(|line| serde_json::Value::String(line.to_string()))
        .collect()
}

impl Notebook {
    /// Decodes a notebook file by its extension: `.ipynb` as Jupyter, anything else as SQLly's own format.
    pub fn from_file_text(path: &str, text: &str) -> Result<Self, String> {
        if is_ipynb_path(path) {
            Self::from_ipynb(text)
        } else {
            Self::from_json(text)
        }
    }

    /// Encodes the notebook for `path`: Jupyter for `.ipynb`, SQLly's own format otherwise.
    pub fn to_file_text(&self, path: &str) -> String {
        if is_ipynb_path(path) {
            self.to_ipynb()
        } else {
            self.to_json()
        }
    }

    /// Reads a Jupyter notebook (nbformat 4.x). Code cells become SQL cells,
    /// markdown and raw cells become Markdown cells; outputs are ignored.
    pub fn from_ipynb(text: &str) -> Result<Self, String> {
        let root: serde_json::Value = serde_json::from_str(text).map_err(|e| {
            crate::localization::format(
                "editor.notebook.invalid_ipynb_file",
                &[("error", &e.to_string())],
            )
        })?;
        let major = root
            .get("nbformat")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if major < 4 {
            return Err(crate::localization::format(
                "editor.notebook.unsupported_ipynb_version",
                &[("version", &major.to_string())],
            ));
        }
        let raw_cells = root
            .get("cells")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Keep `cell_N` ids written by SQLly so `{{cell_N}}` references survive a round trip.
        let mut seen = std::collections::HashSet::new();
        let kept_ids: Vec<Option<String>> = raw_cells
            .iter()
            .map(|cell| {
                cell.get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| is_cell_ident(id) && seen.insert(id.to_string()))
                    .map(str::to_string)
            })
            .collect();
        let mut nb = Self {
            version: NOTEBOOK_FORMAT_VERSION,
            cells: Vec::new(),
            params: Vec::new(),
            next_cell_id: kept_ids
                .iter()
                .flatten()
                .filter_map(|id| parse_cell_num(id))
                .max()
                .unwrap_or(0)
                + 1,
        };
        for (cell, kept) in raw_cells.iter().zip(kept_ids) {
            let kind = match cell.get("cell_type").and_then(serde_json::Value::as_str) {
                Some("code") => CellKind::Sql,
                _ => CellKind::Markdown,
            };
            let mut source = ipynb_source(cell.get("source"));
            if kind == CellKind::Sql {
                source = strip_sql_magic(&source);
            }
            let metadata = cell.get("metadata");
            let collapsed = metadata
                .and_then(|m| m.get("collapsed"))
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
                || metadata
                    .and_then(|m| m.get("jupyter"))
                    .and_then(|j| j.get("source_hidden"))
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
            let id = kept.unwrap_or_else(|| nb.alloc_id());
            nb.cells.push(Cell {
                id,
                kind,
                source,
                collapsed,
            });
        }
        if let Some(params) = root
            .get("metadata")
            .and_then(|m| m.get("sqlly"))
            .and_then(|s| s.get("params"))
        {
            nb.params = serde_json::from_value(params.clone()).unwrap_or_default();
        }
        if nb.cells.is_empty() {
            nb.add_cell(CellKind::Sql, None);
        }
        Ok(nb)
    }

    /// Writes the notebook as nbformat 4.5 with the SQL kernelspec Azure Data
    /// Studio uses; `{{cell_N}}` ids and `@param` globals ride along in
    /// metadata so SQLly reads them back unchanged.
    pub fn to_ipynb(&self) -> String {
        use serde_json::json;
        let cells: Vec<serde_json::Value> = self
            .cells
            .iter()
            .map(|cell| {
                let mut metadata = serde_json::Map::new();
                if cell.collapsed {
                    metadata.insert("jupyter".into(), json!({ "source_hidden": true }));
                }
                match cell.kind {
                    CellKind::Sql => json!({
                        "cell_type": "code",
                        "execution_count": null,
                        "id": cell.id,
                        "metadata": metadata,
                        "outputs": [],
                        "source": ipynb_lines(&cell.source),
                    }),
                    CellKind::Markdown => json!({
                        "cell_type": "markdown",
                        "id": cell.id,
                        "metadata": metadata,
                        "source": ipynb_lines(&cell.source),
                    }),
                }
            })
            .collect();
        let doc = json!({
            "cells": cells,
            "metadata": {
                "kernelspec": { "display_name": "SQL", "language": "sql", "name": "SQL" },
                "language_info": { "name": "sql", "version": "" },
                "sqlly": { "params": self.params },
            },
            "nbformat": 4,
            "nbformat_minor": 5,
        });
        let mut out = serde_json::to_string_pretty(&doc).unwrap_or_else(|_| "{}".into());
        out.push('\n');
        out
    }
}

#[cfg(test)]
mod ipynb_tests {
    use super::*;

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

    #[test]
    fn reads_azure_data_studio_notebook() {
        let nb = Notebook::from_ipynb(ADS_FIXTURE).unwrap();
        assert_eq!(nb.cells.len(), 2);
        assert_eq!(nb.cells[0].kind, CellKind::Markdown);
        assert_eq!(nb.cells[0].source, "# Sales\nTop customers");
        assert_eq!(nb.cells[1].kind, CellKind::Sql);
        assert_eq!(nb.cells[1].source, "SELECT TOP 10 *\nFROM dbo.Customers;");
        assert_eq!(nb.cells[0].id, "cell_1");
        assert_eq!(nb.cells[1].id, "cell_2");
    }

    #[test]
    fn strips_jupyter_sql_magics_and_reads_string_sources() {
        let text = r#"{"nbformat": 4, "nbformat_minor": 5, "metadata": {}, "cells": [
            {"cell_type": "code", "id": "x", "metadata": {"jupyter": {"source_hidden": true}}, "source": "%%sql\nSELECT 1", "outputs": [], "execution_count": null},
            {"cell_type": "code", "id": "y", "metadata": {}, "source": "%sql SELECT 2\n", "outputs": [], "execution_count": null},
            {"cell_type": "raw", "id": "z", "metadata": {}, "source": "plain"}
        ]}"#;
        let nb = Notebook::from_ipynb(text).unwrap();
        assert_eq!(nb.cells[0].source, "SELECT 1");
        assert!(nb.cells[0].collapsed);
        assert_eq!(nb.cells[1].source, "SELECT 2\n");
        assert_eq!(nb.cells[2].kind, CellKind::Markdown);
        assert_eq!(nb.cells[2].source, "plain");
    }

    #[test]
    fn rejects_old_nbformat_and_garbage() {
        assert!(Notebook::from_ipynb(r#"{"nbformat": 3, "worksheets": []}"#).is_err());
        assert!(Notebook::from_ipynb("not json").is_err());
    }

    #[test]
    fn round_trips_ids_params_and_collapsed() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT *\nFROM t\nWHERE id = @id".into();
        let md = nb.add_cell(CellKind::Markdown, None);
        nb.cells[md].source = "Notes".into();
        nb.cells[md].collapsed = true;
        let sql = nb.add_cell(CellKind::Sql, None);
        nb.cells[sql].source = "SELECT * FROM {{cell_1}}".into();
        nb.params = vec![("id".into(), "7".into())];

        let text = nb.to_ipynb();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["nbformat"], 4);
        assert_eq!(value["nbformat_minor"], 5);
        assert_eq!(value["metadata"]["kernelspec"]["language"], "sql");
        assert_eq!(value["cells"][0]["source"][0], "SELECT *\n");
        assert_eq!(value["cells"][0]["outputs"], serde_json::json!([]));

        let back = Notebook::from_ipynb(&text).unwrap();
        assert_eq!(back.cells, nb.cells);
        assert_eq!(back.params, nb.params);
        // New cells continue after the imported ids.
        let mut back = back;
        let added = back.add_cell(CellKind::Sql, None);
        assert_eq!(back.cells[added].id, "cell_4");
        assert_eq!(text, Notebook::from_ipynb(&text).unwrap().to_ipynb());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{CellValue, ResultColumn};

    fn select_only(s: &str) -> bool {
        let t = strip_trailing_semi(s);
        let upper = t.to_ascii_uppercase();
        upper.starts_with("SELECT") || upper.starts_with("WITH")
    }

    #[test]
    fn new_notebook_has_one_sql_cell_and_version() {
        let nb = Notebook::new();
        assert_eq!(nb.version, NOTEBOOK_FORMAT_VERSION);
        assert_eq!(nb.cells.len(), 1);
        assert_eq!(nb.cells[0].id, "cell_1");
        assert_eq!(nb.cells[0].kind, CellKind::Sql);
        assert!(nb.cells[0].source.is_empty());
    }

    #[test]
    fn add_remove_move_keep_ids_stable() {
        let mut nb = Notebook::new();
        let i2 = nb.add_cell(CellKind::Sql, Some(0));
        assert_eq!(nb.cells[i2].id, "cell_2");
        let i3 = nb.add_cell(CellKind::Markdown, None);
        assert_eq!(nb.cells[i3].id, "cell_3");
        let removed = nb.remove_cell(1).unwrap();
        assert_eq!(removed.id, "cell_2");
        let i4 = nb.add_cell(CellKind::Sql, None);
        assert_eq!(nb.cells[i4].id, "cell_4");
        assert!(nb.cells.iter().all(|c| c.id != "cell_2"));
        assert!(nb.move_cell(0, 1));
        assert_eq!(nb.cells[0].id, "cell_3");
        assert_eq!(nb.cells[1].id, "cell_1");
    }

    #[test]
    fn references_dedup_and_order() {
        let src = "SELECT * FROM {{cell_2}} JOIN {{cell_1}} JOIN {{cell_2}}";
        assert_eq!(
            references(src),
            vec!["cell_2".to_string(), "cell_1".to_string()]
        );
        assert!(references("SELECT {{not_a_cell}} FROM t").is_empty());
        assert_eq!(references("{{cell_10}}"), vec!["cell_10".to_string()]);
    }

    #[test]
    fn expand_without_references_is_identity() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1 AS x".into();
        assert_eq!(
            expand_sql(&nb, "cell_1", &select_only).unwrap(),
            "SELECT 1 AS x"
        );
    }

    #[test]
    fn expand_single_reference_builds_with_clause() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1 AS x".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT x FROM {{cell_1}}".into();
        assert_eq!(
            expand_sql(&nb, "cell_2", &select_only).unwrap(),
            "WITH cell_1 AS (SELECT 1 AS x) SELECT x FROM cell_1"
        );
    }

    #[test]
    fn expand_transitive_orders_dependencies_first() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1 AS x".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT x FROM {{cell_1}}".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[2].source = "SELECT x FROM {{cell_2}}".into();
        assert_eq!(
            expand_sql(&nb, "cell_3", &select_only).unwrap(),
            "WITH cell_1 AS (SELECT 1 AS x), cell_2 AS (SELECT x FROM cell_1) SELECT x FROM cell_2"
        );
    }

    #[test]
    fn expand_flattens_nested_with() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "WITH a AS (SELECT 1 AS x) SELECT x FROM a".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT * FROM {{cell_1}}".into();
        assert_eq!(
            expand_sql(&nb, "cell_2", &select_only).unwrap(),
            "WITH a AS (SELECT 1 AS x), cell_1 AS (SELECT x FROM a) SELECT * FROM cell_1"
        );
    }

    #[test]
    fn expand_reports_cycle_path() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT * FROM {{cell_2}}".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT * FROM {{cell_1}}".into();
        match expand_sql(&nb, "cell_1", &select_only) {
            Err(ExpandError::Cycle(path)) => {
                assert!(
                    path.windows(2)
                        .any(|w| w[0] == "cell_1" && w[1] == "cell_2")
                        || path
                            .windows(2)
                            .any(|w| w[0] == "cell_2" && w[1] == "cell_1")
                );
                assert_eq!(path.first(), path.last());
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn expand_rejects_markdown_and_unknown_and_self() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1".into();
        nb.add_cell(CellKind::Markdown, None);
        nb.cells[1].source = "# notes".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[2].source = "SELECT * FROM {{cell_2}}".into();
        assert!(matches!(
            expand_sql(&nb, "cell_3", &select_only),
            Err(ExpandError::NotSql(_))
        ));
        nb.cells[0].source = "SELECT * FROM {{cell_99}}".into();
        assert!(matches!(
            expand_sql(&nb, "cell_1", &select_only),
            Err(ExpandError::UnknownCell(_))
        ));
        nb.cells[0].source = "SELECT * FROM {{cell_1}}".into();
        assert!(matches!(
            expand_sql(&nb, "cell_1", &select_only),
            Err(ExpandError::SelfReference(_))
        ));
        assert!(matches!(
            expand_sql(&nb, "cell_2", &select_only),
            Err(ExpandError::NotSql(_))
        ));
    }

    #[test]
    fn expand_rejects_non_select_dependency() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "DELETE FROM t".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT * FROM {{cell_1}}".into();
        assert!(matches!(
            expand_sql(&nb, "cell_2", &select_only),
            Err(ExpandError::NotSelect(_))
        ));
        nb.cells[0].source = "SELECT 1; SELECT 2".into();
        assert!(matches!(
            expand_sql(&nb, "cell_2", &select_only),
            Err(ExpandError::NotSelect(_))
        ));
    }

    #[test]
    fn json_round_trip_and_never_stores_results() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1 AS x".into();
        nb.params.push(("limit".into(), "10".into()));
        let json = nb.to_json();
        assert!(!json.contains("\"rows\""));
        assert!(!json.contains("\"results\""));
        let restored = Notebook::from_json(&json).unwrap();
        assert_eq!(restored.cells[0].source, "SELECT 1 AS x");
        assert_eq!(restored.params, vec![("limit".into(), "10".into())]);
        assert_eq!(restored.version, NOTEBOOK_FORMAT_VERSION);
    }

    #[test]
    fn json_rejects_newer_version() {
        let json = r#"{"version":99,"cells":[],"params":[]}"#;
        let err = Notebook::from_json(json).unwrap_err();
        assert!(
            err.contains("written by a newer SQLLY"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn run_all_order_skips_markdown() {
        let mut nb = Notebook::new();
        nb.add_cell(CellKind::Markdown, None);
        nb.add_cell(CellKind::Sql, None);
        assert_eq!(run_all_order(&nb), vec!["cell_1", "cell_3"]);
    }

    #[test]
    fn export_markdown_golden() {
        let mut nb = Notebook::new();
        nb.cells[0].kind = CellKind::Markdown;
        nb.cells[0].source = "# Notes".into();
        nb.add_cell(CellKind::Sql, None);
        nb.cells[1].source = "SELECT 1 AS n".into();
        let cols = [ResultColumn {
            name: "n".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let rows = vec![vec![CellValue::I64(1)], vec![CellValue::I64(2)]];
        let table = crate::result_export::markdown(&cols, &rows, true);
        let out = export_markdown(
            &nb,
            "Report",
            &[MarkdownCellResult {
                cell_id: "cell_2",
                table: Some(table.clone()),
            }],
        );
        let expected = format!("# Report\n\n# Notes\n\n```sql\nSELECT 1 AS n\n```\n\n{table}\n");
        assert_eq!(out, expected);
    }

    #[test]
    fn export_markdown_without_results_has_no_tables() {
        let mut nb = Notebook::new();
        nb.cells[0].source = "SELECT 1 AS n".into();
        let out = export_markdown(&nb, "T", &[]);
        assert!(!out.contains('|'));
        assert!(out.contains("```sql"));
    }
}
