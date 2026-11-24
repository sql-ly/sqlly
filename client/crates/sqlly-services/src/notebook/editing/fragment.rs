//! Cells on the clipboard: a native fragment (JSON with a format marker)
//! that pastes back as new cells with fresh ids, de-duplicated names and
//! references rewritten to the pasted copies, plus the plain-text forms
//! other apps read (a SQL script, or Markdown with fenced SQL) and the
//! reverse: plain text split into cells.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

use crate::notebook::document::{
    Cell, CellKind, ConnectionRef, IdSource, NotebookDoc, new_cell_id,
};
use crate::notebook::references::{self, Dialects, RefTarget, validate_cell_name};

/// `format` of a fragment.
pub const FRAGMENT_FORMAT: &str = "sqlly.notebook.fragment";
pub const FRAGMENT_VERSION: u32 = 1;
/// Media type a host can register the fragment under.
pub const FRAGMENT_MIME: &str = "application/x-sqlly-notebook-fragment+json";

/// Copied cells, in document order, with the connection slots they name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fragment {
    pub format: String,
    pub format_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_notebook: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connection_refs: Vec<ConnectionRef>,
    pub cells: Vec<Cell>,
}

impl Fragment {
    /// The cells `ids` of `doc`, in document order (unknown ids skipped).
    pub fn of(doc: &NotebookDoc, ids: &[String]) -> Self {
        let wanted: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        let cells: Vec<Cell> = doc
            .cells
            .iter()
            .filter(|c| wanted.contains(c.id.as_str()))
            .cloned()
            .collect();
        let slots: BTreeSet<&str> = cells
            .iter()
            .filter_map(|c| c.connection_ref.as_deref())
            .collect();
        Self {
            format: FRAGMENT_FORMAT.to_string(),
            format_version: FRAGMENT_VERSION,
            source_notebook: Some(doc.notebook_id.clone()),
            connection_refs: doc
                .connection_refs
                .iter()
                .filter(|r| slots.contains(r.id.as_str()))
                .cloned()
                .collect(),
            cells,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// A fragment from clipboard text, when the text is one (by its marker).
    pub fn from_json(text: &str) -> Option<Self> {
        let text = text.trim_start();
        if !text.starts_with('{') || !text.contains(FRAGMENT_FORMAT) {
            return None;
        }
        let fragment: Self = serde_json::from_str(text).ok()?;
        (fragment.format == FRAGMENT_FORMAT && fragment.format_version <= FRAGMENT_VERSION)
            .then_some(fragment)
    }
}

/// Paste `fragment` into `doc` at `index`. Every cell gets a new id; names
/// that are taken get a `_2`, `_3`… suffix; `{{x}}` references and spec
/// inputs that point at a cell of the fragment are rewritten to its copy;
/// connection slots map onto the document's slot for the same profile (or
/// are added). Pasted cells start read-only: permission to change data is
/// never copied. Returns the new ids in order.
pub fn paste(
    doc: &mut NotebookDoc,
    fragment: &Fragment,
    index: usize,
    dialects: &Dialects,
    ids: &mut dyn IdSource,
) -> Vec<String> {
    let slot_map = map_slots(doc, &fragment.connection_refs);
    let mut id_map: BTreeMap<String, String> = BTreeMap::new();
    let mut new_ids = Vec::new();
    let start = index.min(doc.cells.len());
    for (index, cell) in (start..).zip(&fragment.cells) {
        let id = new_cell_id(ids, |candidate| {
            doc.cell(candidate).is_some() || id_map.values().any(|v| v == candidate)
        });
        id_map.insert(cell.id.clone(), id.clone());
        let mut copy = cell.clone();
        copy.id = id.clone();
        copy.name = None;
        copy.execution_policy.read_only = true;
        copy.connection_ref = cell
            .connection_ref
            .as_ref()
            .map(|slot| slot_map.get(slot).cloned().unwrap_or_else(|| slot.clone()));
        doc.cells.insert(index, copy);
        new_ids.push(id);
    }
    // Names: the original where it is free, else the first free suffix. A v1
    // cell named after its own id is pasted without a name.
    let mut name_map: BTreeMap<String, String> = BTreeMap::new();
    for cell in &fragment.cells {
        let Some(name) = cell.name.as_deref().filter(|n| *n != cell.id) else {
            continue;
        };
        let new_id = &id_map[&cell.id];
        if let Some(free) = free_name(doc, new_id, name) {
            if let Some(target) = doc.cell_mut(new_id) {
                target.name = Some(free.clone());
            }
            name_map.insert(name.to_ascii_lowercase(), free);
        }
    }
    for cell in &fragment.cells {
        let new_id = id_map[&cell.id].clone();
        let Some(copy) = doc.cell(&new_id) else {
            continue;
        };
        let db = dialects.for_cell(doc, copy);
        let edits: Vec<(usize, usize, String)> =
            references::find_references(&copy.source, copy.kind.source_language(), db)
                .into_iter()
                .filter_map(|r| {
                    let RefTarget::Cell(token) = &r.target else {
                        return None;
                    };
                    let old = fragment_target(fragment, token)?;
                    let named = fragment
                        .cells
                        .iter()
                        .find(|c| c.id == old)
                        .and_then(|c| c.name.as_deref().filter(|n| *n != c.id))
                        .is_some_and(|n| n.eq_ignore_ascii_case(token));
                    let replacement = named
                        .then(|| name_map.get(&token.to_ascii_lowercase()).cloned())
                        .flatten()
                        .unwrap_or_else(|| id_map[old].clone());
                    Some((r.start, r.end, format!("{{{{{replacement}}}}}")))
                })
                .collect();
        let source = rewrite(&copy.source, &edits);
        let mut spec = copy.spec.clone();
        remap_ids(&mut spec, &id_map);
        if let Some(target) = doc.cell_mut(&new_id) {
            target.source = source;
            target.spec = spec;
        }
    }
    new_ids
}

/// The fragment cell `token` names (by name first, then id).
fn fragment_target<'a>(fragment: &'a Fragment, token: &str) -> Option<&'a str> {
    fragment
        .cells
        .iter()
        .find(|c| {
            c.name
                .as_deref()
                .is_some_and(|n| n.eq_ignore_ascii_case(token))
        })
        .or_else(|| fragment.cells.iter().find(|c| c.id == token))
        .map(|c| c.id.as_str())
}

/// `name`, or `name_2`, `name_3`… the first one cell `id` may take.
pub fn free_name(doc: &NotebookDoc, id: &str, name: &str) -> Option<String> {
    if validate_cell_name(doc, id, name).is_ok() {
        return Some(name.to_string());
    }
    let base = match name.rsplit_once('_') {
        Some((stem, n)) if !stem.is_empty() && n.parse::<u32>().is_ok() => stem,
        _ => name,
    };
    (2..1000)
        .map(|n| format!("{base}_{n}"))
        .find(|candidate| validate_cell_name(doc, id, candidate).is_ok())
}

fn map_slots(doc: &mut NotebookDoc, refs: &[ConnectionRef]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for slot in refs {
        let existing = doc
            .connection_refs
            .iter()
            .find(|r| r.id == slot.id && r.profile_id == slot.profile_id)
            .or_else(|| {
                doc.connection_refs
                    .iter()
                    .find(|r| slot.profile_id.is_some() && r.profile_id == slot.profile_id)
            })
            .map(|r| r.id.clone());
        let id = match existing {
            Some(id) => id,
            None => {
                let taken = |id: &str| doc.connection_refs.iter().any(|r| r.id == id);
                let id = if taken(&slot.id) {
                    let mut n = doc.connection_refs.len() + 1;
                    while taken(&format!("conn_{n}")) {
                        n += 1;
                    }
                    format!("conn_{n}")
                } else {
                    slot.id.clone()
                };
                let mut added = slot.clone();
                added.id = id.clone();
                doc.connection_refs.push(added);
                id
            }
        };
        map.insert(slot.id.clone(), id);
    }
    map
}

/// Replace every string in `value` that is a copied cell's id with its
/// copy's id (spec inputs, follows, baselines, tiles).
pub fn remap_ids(value: &mut Value, ids: &BTreeMap<String, String>) {
    match value {
        Value::String(s) => {
            if let Some(next) = ids.get(s.as_str()) {
                *s = next.clone();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|v| remap_ids(v, ids)),
        Value::Object(map) => map.values_mut().for_each(|v| remap_ids(v, ids)),
        _ => {}
    }
}

pub(crate) fn rewrite(text: &str, edits: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for (start, end, replacement) in edits {
        out.push_str(&text[last..*start]);
        out.push_str(replacement);
        last = *end;
    }
    out.push_str(&text[last..]);
    out
}

/// Whether statements on `db` are separated by `GO` batches.
fn uses_go(db: DatabaseType) -> bool {
    db.family() == Dialect::SqlServer && db != DatabaseType::Access
}

/// The cells as one SQL script: SQL cells separated by `;` (or `GO` on SQL
/// Server), Markdown as `--` comments, other kinds left out.
pub fn sql_text(fragment: &Fragment, db: DatabaseType) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cell in &fragment.cells {
        match cell.kind {
            CellKind::Sql => {
                let sql = cell.source.trim();
                if sql.is_empty() {
                    continue;
                }
                if uses_go(db) {
                    parts.push(format!("{sql}\nGO"));
                } else if sql.ends_with(';') {
                    parts.push(sql.to_string());
                } else {
                    parts.push(format!("{sql};"));
                }
            }
            CellKind::Markdown => {
                let text = cell.source.trim();
                if !text.is_empty() {
                    parts.push(
                        text.lines()
                            .map(|l| format!("-- {l}").trim_end().to_string())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
            }
            _ => {}
        }
    }
    let mut out = parts.join("\n\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// The cells as Markdown: narrative as written, SQL in ```sql fences, other
/// kinds with text in a fence named after their kind.
pub fn markdown_text(fragment: &Fragment) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cell in &fragment.cells {
        let text = cell.source.trim();
        match cell.kind {
            CellKind::Markdown => {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
            CellKind::Sql => parts.push(format!("```sql\n{text}\n```")),
            _ if !text.is_empty() => {
                parts.push(format!("```{}\n{text}\n```", cell.kind.as_str()));
            }
            _ => {}
        }
    }
    let mut out = parts.join("\n\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

/// Whether clipboard text reads as Markdown rather than SQL: a fenced block
/// or a heading line.
pub fn looks_like_markdown(text: &str) -> bool {
    text.lines().any(|l| {
        let l = l.trim_start();
        l.starts_with("```")
            || (l.starts_with('#') && {
                let hashes = l.bytes().take_while(|b| *b == b'#').count();
                hashes <= 6 && l[hashes..].starts_with(' ')
            })
    })
}

/// Plain text as cells: Markdown splits into narrative and its ```sql
/// fences; anything else is SQL split into one cell per statement by
/// `split_sql` (the editor's statement splitter).
pub fn cells_from_text(
    text: &str,
    split_sql: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(CellKind, String)> {
    let text = text.replace("\r\n", "\n");
    if !looks_like_markdown(&text) {
        return split_sql(&text)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .map(|s| (CellKind::Sql, s))
            .collect();
    }
    let mut out = Vec::new();
    let mut narrative = String::new();
    let mut fence: Option<(String, String)> = None;
    let flush = |narrative: &mut String, out: &mut Vec<(CellKind, String)>| {
        let t = narrative.trim();
        if !t.is_empty() {
            out.push((CellKind::Markdown, t.to_string()));
        }
        narrative.clear();
    };
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        match &mut fence {
            Some((lang, body)) => {
                if trimmed.starts_with("```") {
                    let body = body.trim().to_string();
                    if lang.eq_ignore_ascii_case("sql") || lang.is_empty() {
                        out.push((CellKind::Sql, body));
                    } else {
                        narrative.push_str(&format!("```{lang}\n{body}\n```\n"));
                        flush(&mut narrative, &mut out);
                    }
                    fence = None;
                } else {
                    body.push_str(line);
                    body.push('\n');
                }
            }
            None if trimmed.starts_with("```") => {
                flush(&mut narrative, &mut out);
                fence = Some((trimmed[3..].trim().to_string(), String::new()));
            }
            None => {
                narrative.push_str(line);
                narrative.push('\n');
            }
        }
    }
    if let Some((lang, body)) = fence {
        narrative.push_str(&format!("```{lang}\n{body}"));
    }
    flush(&mut narrative, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::SeededIds;

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(1));
        doc.cells[0].name = Some("orders".into());
        doc.cells[0].source = "select * from sales.orders".into();
        doc.cells[0].execution_policy.read_only = false;
        let i = doc.add_cell(CellKind::Sql, None, &mut SeededIds(2));
        doc.cells[i].name = Some("totals".into());
        doc.cells[i].source = "select sum(amount) from {{orders}}".into();
        let c = doc.add_cell(CellKind::Chart, None, &mut SeededIds(3));
        let input = doc.cells[i].id.clone();
        doc.cells[c].spec = serde_json::json!({"input": input, "chart": {"title": "Totals"}});
        doc
    }

    fn ids(doc: &NotebookDoc) -> Vec<String> {
        doc.cells.iter().map(|c| c.id.clone()).collect()
    }

    #[test]
    fn fragment_round_trips_through_json() {
        let d = doc();
        let f = Fragment::of(&d, &ids(&d));
        let back = Fragment::from_json(&f.to_json()).unwrap();
        assert_eq!(back, f);
        assert!(Fragment::from_json("select 1").is_none());
        assert!(Fragment::from_json("{\"a\": 1}").is_none());
    }

    #[test]
    fn paste_regenerates_ids_dedupes_names_and_rewrites_references() {
        let mut d = doc();
        let f = Fragment::of(&d, &ids(&d));
        let dialects = Dialects::single(DatabaseType::SqlServer);
        let pasted = paste(&mut d, &f, 3, &dialects, &mut SeededIds(99));
        assert_eq!(pasted.len(), 3);
        assert_eq!(d.cells.len(), 6);
        for id in &pasted {
            assert!(!f.cells.iter().any(|c| &c.id == id));
        }
        assert_eq!(d.cells[3].name.as_deref(), Some("orders_2"));
        assert_eq!(d.cells[4].name.as_deref(), Some("totals_2"));
        assert_eq!(d.cells[4].source, "select sum(amount) from {{orders_2}}");
        assert_eq!(d.cells[5].spec["input"], serde_json::json!(pasted[1]));
        assert!(
            d.cells[3].execution_policy.read_only,
            "writes are never copied"
        );
        assert!(references::validate_names(&d).is_empty());
    }

    #[test]
    fn references_outside_the_fragment_are_kept() {
        let mut d = doc();
        let f = Fragment::of(&d, &[d.cells[1].id.clone()]);
        let dialects = Dialects::single(DatabaseType::Postgres);
        paste(&mut d, &f, 3, &dialects, &mut SeededIds(7));
        assert_eq!(d.cells[3].source, "select sum(amount) from {{orders}}");
    }

    #[test]
    fn paste_into_another_notebook_keeps_free_names_and_maps_slots() {
        let mut src = doc();
        src.connection_refs.push(ConnectionRef {
            id: "conn_1".into(),
            profile_id: Some("p1".into()),
            label: "Sales".into(),
            database: None,
            extra: Default::default(),
        });
        src.cells[0].connection_ref = Some("conn_1".into());
        let f = Fragment::of(&src, &[src.cells[0].id.clone()]);
        assert_eq!(f.connection_refs.len(), 1);
        let mut dst = NotebookDoc::new("other", &mut SeededIds(50));
        dst.connection_refs.push(ConnectionRef {
            id: "conn_9".into(),
            profile_id: Some("p1".into()),
            label: "Sales".into(),
            database: None,
            extra: Default::default(),
        });
        paste(
            &mut dst,
            &f,
            0,
            &Dialects::single(DatabaseType::SqlServer),
            &mut SeededIds(51),
        );
        assert_eq!(dst.cells[0].name.as_deref(), Some("orders"));
        assert_eq!(dst.cells[0].connection_ref.as_deref(), Some("conn_9"));
        assert_eq!(dst.connection_refs.len(), 1);
    }

    #[test]
    fn plain_text_forms() {
        let d = doc();
        let f = Fragment::of(&d, &ids(&d));
        let tsql = sql_text(&f, DatabaseType::SqlServer);
        assert!(tsql.contains("select * from sales.orders\nGO"));
        let pg = sql_text(&f, DatabaseType::Postgres);
        assert!(pg.contains("select * from sales.orders;\n\nselect sum"));
        let md = markdown_text(&f);
        assert!(md.starts_with("```sql\nselect * from sales.orders\n```"));
    }

    #[test]
    fn text_becomes_cells() {
        let split = |s: &str| s.split(';').map(str::to_string).collect::<Vec<_>>();
        let cells = cells_from_text("select 1; select 2;\r\n", &split);
        assert_eq!(
            cells,
            vec![
                (CellKind::Sql, "select 1".into()),
                (CellKind::Sql, "select 2".into())
            ]
        );
        let md = "# Revenue\nWhy it fell.\n\n```sql\nselect 1\n```\nAfter.";
        let cells = cells_from_text(md, &split);
        assert_eq!(cells.len(), 3);
        assert_eq!(
            cells[0],
            (CellKind::Markdown, "# Revenue\nWhy it fell.".into())
        );
        assert_eq!(cells[1], (CellKind::Sql, "select 1".into()));
        assert_eq!(cells[2], (CellKind::Markdown, "After.".into()));
    }
}
