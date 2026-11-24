//! Find and replace across a notebook: cell sources (SQL and Markdown),
//! cell names, the notebook title, and the text fields of cell specs
//! (chart titles, annotations, captions, notes); optionally the values of
//! cell outputs (searched only, never replaced).

use serde_json::Value;
use sqlly_core::{CellValue, ResultColumn};

use crate::notebook::document::{Cell, NotebookDoc};

/// Spec keys whose string values are user-written text.
pub const TEXT_KEYS: &[&str] = &[
    "title",
    "subtitle",
    "annotation",
    "annotations",
    "caption",
    "note",
    "notes",
    "label",
    "text",
    "description",
];

/// Matches kept per search; navigation and the counter stop here.
pub const MAX_MATCHES: usize = 5_000;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FindOptions {
    pub query: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
}

/// Where a match is.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindField {
    Title,
    Name,
    Source,
    /// A text value inside the spec, by JSON pointer.
    Spec(String),
    /// A value in the cell's output (row, column).
    Data {
        row: usize,
        column: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindMatch {
    /// `None` for the notebook title.
    pub cell_id: Option<String>,
    pub field: FindField,
    /// Byte range within the field's text.
    pub start: usize,
    pub end: usize,
    /// The line around the match, for the result list.
    pub snippet: String,
}

impl FindMatch {
    pub fn replaceable(&self) -> bool {
        !matches!(self.field, FindField::Data { .. })
    }
}

/// Byte ranges of `needle` in `hay` (ASCII case folding unless
/// `case_sensitive`; whole words when asked).
pub fn find_in(hay: &str, opts: &FindOptions) -> Vec<(usize, usize)> {
    let needle = opts.query.as_bytes();
    if needle.is_empty() || needle.len() > hay.len() {
        return Vec::new();
    }
    let bytes = hay.as_bytes();
    let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        let window = &bytes[i..i + needle.len()];
        let hit = if opts.case_sensitive {
            window == needle
        } else {
            window.eq_ignore_ascii_case(needle)
        };
        let end = i + needle.len();
        if hit
            && hay.is_char_boundary(i)
            && hay.is_char_boundary(end)
            && (!opts.whole_word
                || ((i == 0 || !word(bytes[i - 1])) && (end == bytes.len() || !word(bytes[end]))))
        {
            out.push((i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

fn snippet(text: &str, start: usize, end: usize) -> String {
    let line_start = text[..start].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let line_end = text[end..]
        .find('\n')
        .map(|p| end + p)
        .unwrap_or(text.len());
    let line = &text[line_start..line_end];
    let mut out: String = line.trim().chars().take(160).collect();
    if line.trim().chars().count() > 160 {
        out.push('…');
    }
    out
}

fn push_matches(
    out: &mut Vec<FindMatch>,
    cell_id: Option<&str>,
    field: FindField,
    text: &str,
    opts: &FindOptions,
) {
    for (start, end) in find_in(text, opts) {
        if out.len() >= MAX_MATCHES {
            return;
        }
        out.push(FindMatch {
            cell_id: cell_id.map(str::to_string),
            field: field.clone(),
            start,
            end,
            snippet: snippet(text, start, end),
        });
    }
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// Every user-text string in `spec`, with its JSON pointer.
pub fn spec_texts(spec: &Value) -> Vec<(String, String)> {
    fn walk(value: &Value, path: &str, texty: bool, out: &mut Vec<(String, String)>) {
        match value {
            Value::String(s) if texty => out.push((path.to_string(), s.clone())),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    walk(item, &format!("{path}/{i}"), texty, out);
                }
            }
            Value::Object(map) => {
                for (k, v) in map {
                    let texty = TEXT_KEYS.contains(&k.as_str());
                    walk(v, &format!("{path}/{}", escape_pointer(k)), texty, out);
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(spec, "", false, &mut out);
    out
}

/// Matches in one cell: name, source, spec text.
pub fn find_in_cell(cell: &Cell, opts: &FindOptions, out: &mut Vec<FindMatch>) {
    if let Some(name) = cell.name.as_deref() {
        push_matches(out, Some(&cell.id), FindField::Name, name, opts);
    }
    push_matches(out, Some(&cell.id), FindField::Source, &cell.source, opts);
    for (path, text) in spec_texts(&cell.spec) {
        push_matches(out, Some(&cell.id), FindField::Spec(path), &text, opts);
    }
}

/// Every match in the document, title first, then cells in order.
pub fn find(doc: &NotebookDoc, opts: &FindOptions) -> Vec<FindMatch> {
    let mut out = Vec::new();
    if opts.query.is_empty() {
        return out;
    }
    push_matches(&mut out, None, FindField::Title, &doc.title, opts);
    for cell in &doc.cells {
        find_in_cell(cell, opts, &mut out);
    }
    out
}

/// A value as search text.
pub fn value_text(value: &CellValue) -> Option<String> {
    match value {
        CellValue::Null | CellValue::Bytes(_) => None,
        CellValue::Bool(b) => Some(b.to_string()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::String(s) => Some(s.clone()),
    }
}

/// Matches in an output's rows (column names are part of the snippet).
pub fn find_in_rows(
    cell_id: &str,
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    opts: &FindOptions,
    out: &mut Vec<FindMatch>,
) {
    if opts.query.is_empty() {
        return;
    }
    for (r, row) in rows.iter().enumerate() {
        for (c, value) in row.iter().enumerate() {
            if out.len() >= MAX_MATCHES {
                return;
            }
            let Some(text) = value_text(value) else {
                continue;
            };
            for (start, end) in find_in(&text, opts) {
                let column = columns.get(c).map(|c| c.name.as_str()).unwrap_or("");
                out.push(FindMatch {
                    cell_id: Some(cell_id.to_string()),
                    field: FindField::Data { row: r, column: c },
                    start,
                    end,
                    snippet: format!("{column}: {}", snippet(&text, start, end)),
                });
            }
        }
    }
}

fn replace_ranges(text: &str, ranges: &[(usize, usize)], with: &str) -> String {
    let edits: Vec<(usize, usize, String)> = ranges
        .iter()
        .map(|(s, e)| (*s, *e, with.to_string()))
        .collect();
    super::fragment::rewrite(text, &edits)
}

fn set_pointer(spec: &mut Value, path: &str, next: String) -> bool {
    match spec.pointer_mut(path) {
        Some(slot @ Value::String(_)) => {
            *slot = Value::String(next);
            true
        }
        _ => false,
    }
}

/// What a replace changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Replaced {
    pub count: usize,
    /// Cells whose source changed (their editors need the new text).
    pub sources: Vec<String>,
    /// Names that could not be replaced (the new name is not allowed).
    pub refused_names: Vec<String>,
}

/// Replace every match in the cells `editable` allows (and the title).
/// Names are renamed only when the result is a valid, free name; a rename
/// does not rewrite references here (the host renames through the core so
/// dependents follow).
pub fn replace_all(
    doc: &mut NotebookDoc,
    opts: &FindOptions,
    with: &str,
    editable: &dyn Fn(&Cell) -> bool,
) -> (Replaced, Vec<(String, String)>) {
    let mut done = Replaced::default();
    let mut renames = Vec::new();
    if opts.query.is_empty() {
        return (done, renames);
    }
    let title = find_in(&doc.title, opts);
    if !title.is_empty() {
        done.count += title.len();
        doc.title = replace_ranges(&doc.title, &title, with);
    }
    for cell in doc.cells.iter_mut().filter(|c| editable(c)) {
        let hits = find_in(&cell.source, opts);
        if !hits.is_empty() {
            done.count += hits.len();
            cell.source = replace_ranges(&cell.source, &hits, with);
            done.sources.push(cell.id.clone());
        }
        for (path, text) in spec_texts(&cell.spec) {
            let hits = find_in(&text, opts);
            if !hits.is_empty()
                && set_pointer(&mut cell.spec, &path, replace_ranges(&text, &hits, with))
            {
                done.count += hits.len();
            }
        }
        if let Some(name) = cell.name.as_deref() {
            let hits = find_in(name, opts);
            if !hits.is_empty() {
                renames.push((cell.id.clone(), replace_ranges(name, &hits, with)));
            }
        }
    }
    (done, renames)
}

/// Replace one match (it must still be where `find` put it). Returns
/// whether the text changed; name matches come back as a rename to apply.
pub fn replace_one(
    doc: &mut NotebookDoc,
    m: &FindMatch,
    with: &str,
) -> Result<bool, (String, String)> {
    let range = [(m.start, m.end)];
    let fits = |text: &str| {
        m.end <= text.len() && text.is_char_boundary(m.start) && text.is_char_boundary(m.end)
    };
    match (&m.cell_id, &m.field) {
        (None, FindField::Title) if fits(&doc.title) => {
            doc.title = replace_ranges(&doc.title, &range, with);
            Ok(true)
        }
        (Some(id), field) => {
            let Some(cell) = doc.cell_mut(id) else {
                return Ok(false);
            };
            match field {
                FindField::Source if fits(&cell.source) => {
                    cell.source = replace_ranges(&cell.source, &range, with);
                    Ok(true)
                }
                FindField::Spec(path) => {
                    let Some(text) = cell
                        .spec
                        .pointer(path)
                        .and_then(Value::as_str)
                        .map(str::to_string)
                    else {
                        return Ok(false);
                    };
                    if !fits(&text) {
                        return Ok(false);
                    }
                    Ok(set_pointer(
                        &mut cell.spec,
                        path,
                        replace_ranges(&text, &range, with),
                    ))
                }
                FindField::Name => {
                    let name = cell.name.clone().unwrap_or_default();
                    if !fits(&name) {
                        return Ok(false);
                    }
                    Err((id.clone(), replace_ranges(&name, &range, with)))
                }
                _ => Ok(false),
            }
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CellKind, SeededIds};

    fn doc() -> NotebookDoc {
        let mut doc = NotebookDoc::new("Revenue review", &mut SeededIds(1));
        doc.cells[0].name = Some("revenue_by_region".into());
        doc.cells[0].source = "select region, sum(revenue) from sales\n-- revenue".into();
        let c = doc.add_cell(CellKind::Chart, None, &mut SeededIds(2));
        doc.cells[c].spec = serde_json::json!({
            "input": "x",
            "chart": {"title": "Revenue by region", "kind": "bar",
                      "annotations": [{"x": "2026", "text": "Revenue dip"}]}
        });
        let u = doc.add_cell(CellKind::Unknown("python".into()), None, &mut SeededIds(3));
        doc.cells[u].source = "revenue = 1".into();
        doc
    }

    fn opts(q: &str) -> FindOptions {
        FindOptions {
            query: q.into(),
            ..Default::default()
        }
    }

    #[test]
    fn finds_in_every_text_field() {
        let d = doc();
        let found = find(&d, &opts("revenue"));
        let fields: Vec<_> = found.iter().map(|m| m.field.clone()).collect();
        assert!(fields.contains(&FindField::Title));
        assert!(fields.contains(&FindField::Name));
        assert!(fields.contains(&FindField::Spec("/chart/title".into())));
        assert!(fields.contains(&FindField::Spec("/chart/annotations/0/text".into())));
        assert_eq!(
            fields.iter().filter(|f| **f == FindField::Source).count(),
            3
        );
        assert!(
            find(
                &d,
                &FindOptions {
                    case_sensitive: true,
                    ..opts("Revenue")
                }
            )
            .len()
                < found.len()
        );
        assert!(!fields.contains(&FindField::Spec("/chart/kind".into())));
    }

    #[test]
    fn whole_word_and_unicode_boundaries() {
        let o = FindOptions {
            whole_word: true,
            ..opts("rev")
        };
        assert!(find_in("revenue rev", &o) == vec![(8, 11)]);
        assert!(find_in("café é", &opts("é")).len() == 2);
    }

    #[test]
    fn replace_all_skips_read_only_cells_and_reports_renames() {
        let mut d = doc();
        let (done, renames) = replace_all(&mut d, &opts("revenue"), "income", &|c| {
            !matches!(c.kind, CellKind::Unknown(_))
        });
        assert_eq!(d.title, "income review");
        assert_eq!(
            d.cells[0].source,
            "select region, sum(income) from sales\n-- income"
        );
        assert_eq!(d.cells[1].spec["chart"]["title"], "income by region");
        assert_eq!(d.cells[2].source, "revenue = 1");
        assert_eq!(
            renames,
            vec![(d.cells[0].id.clone(), "income_by_region".into())]
        );
        assert_eq!(done.count, 5);
    }

    #[test]
    fn replace_one_hits_only_that_match() {
        let mut d = doc();
        let found = find(&d, &opts("revenue"));
        let src = found.iter().find(|m| m.field == FindField::Source).unwrap();
        assert_eq!(replace_one(&mut d, src, "rev"), Ok(true));
        assert!(
            d.cells[0]
                .source
                .starts_with("select region, sum(rev) from")
        );
        assert!(d.cells[0].source.ends_with("-- revenue"));
    }

    #[test]
    fn data_values_are_found_not_replaced() {
        let columns = vec![ResultColumn {
            name: "region".into(),
            data_type: "text".into(),
            nullable: true,
        }];
        let rows = vec![
            vec![CellValue::String("North".into())],
            vec![CellValue::I64(42)],
        ];
        let mut out = Vec::new();
        find_in_rows("c1", &columns, &rows, &opts("north"), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].snippet, "region: North");
        assert!(!out[0].replaceable());
    }
}
