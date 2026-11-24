//! `sqlly notebook diff` and `sqlly notebook cat`: a semantic diff of two
//! notebooks (cells matched by id) and a stable text rendering for Git's
//! `textconv`.
//!
//! Formatting churn never shows: the two documents are compared as models,
//! so key order, indentation and line endings do not matter, and a cell's
//! source is compared with line endings unified and trailing blanks
//! removed. Collapsing a cell is view state and is ignored.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;
use sqlly_services::notebook::{Cell, NotebookDoc};

use super::textdiff::unified;
use super::{NotebookFormat, source};
use crate::agent::{CliError, ExitCode};

pub const GIT_RECIPE: &str = "\
Use with Git:
  # .gitattributes
  *.sqllynb diff=sqllynb
  # once per machine (or repository)
  git config --global diff.sqllynb.textconv \"sqlly-cli notebook cat\"
  git config --global difftool.sqllynb.cmd 'sqlly-cli notebook diff \"$LOCAL\" \"$REMOTE\"'
  git difftool -t sqllynb -- report.sqllynb
`git diff` then shows cells, parameters and connections instead of raw JSON;
`git difftool -t sqllynb` shows this semantic diff.";

#[derive(Debug, Clone, clap::Args)]
#[command(after_help = GIT_RECIPE)]
pub struct DiffArgs {
    /// The older notebook.
    pub a: PathBuf,
    /// The newer notebook.
    pub b: PathBuf,
    /// Lines of context around source changes.
    #[arg(long, default_value_t = 3)]
    pub context: usize,
    /// Exit 1 when the notebooks differ (like `git diff --exit-code`).
    #[arg(long)]
    pub exit_code: bool,
    #[arg(long, value_enum, default_value_t = NotebookFormat::Table)]
    pub format: NotebookFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FieldChange {
    pub field: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub after: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ItemChange {
    pub id: String,
    pub change: Presence,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CellDiff {
    pub id: String,
    /// The cell's name (else id) in the newer notebook, or the older one
    /// when removed.
    pub label: String,
    pub kind: String,
    pub change: Presence,
    /// 1-based positions before and after, when the cell moved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub moved: Option<(usize, usize)>,
    /// 1-based position in the notebook it appears in (added or removed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<FieldChange>,
    /// Unified diff of the source, when it changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_diff: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct NotebookDiff {
    pub notebook: Vec<FieldChange>,
    pub parameters: Vec<ItemChange>,
    pub connections: Vec<ItemChange>,
    pub cells: Vec<CellDiff>,
}

impl NotebookDiff {
    pub fn is_empty(&self) -> bool {
        self.notebook.is_empty()
            && self.parameters.is_empty()
            && self.connections.is_empty()
            && self.cells.is_empty()
    }
}

/// Every changed leaf between two JSON values, with dotted paths.
fn json_changes(path: &str, a: &Value, b: &Value, out: &mut Vec<FieldChange>) {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let keys: BTreeSet<&String> = x.keys().chain(y.keys()).collect();
            for key in keys {
                let sub = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                match (x.get(key), y.get(key)) {
                    (Some(l), Some(r)) => json_changes(&sub, l, r, out),
                    (l, r) => out.push(FieldChange {
                        field: sub,
                        before: l.cloned(),
                        after: r.cloned(),
                    }),
                }
            }
        }
        _ if a != b => out.push(FieldChange {
            field: path.to_string(),
            before: (!a.is_null()).then(|| a.clone()),
            after: (!b.is_null()).then(|| b.clone()),
        }),
        _ => {}
    }
}

fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A source as compared: line endings unified, trailing blanks dropped.
pub fn normalize_source(text: &str) -> String {
    let mut lines: Vec<&str> = text.split('\n').map(|l| l.trim_end()).collect();
    while lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// A cell's comparable fields, minus its id, source and view state.
fn cell_fields(cell: &Cell) -> Value {
    let mut value = to_json(cell);
    if let Value::Object(map) = &mut value {
        for key in ["id", "source", "collapsed"] {
            map.remove(key);
        }
    }
    value
}

/// The positions (in `b`) of ids that keep their relative order: the
/// longest common subsequence of the shared ids.
fn stable_ids(a: &[&str], b: &[&str]) -> BTreeSet<String> {
    let (n, m) = (a.len(), b.len());
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = BTreeSet::new();
    while i < n && j < m {
        if a[i] == b[j] {
            out.insert(a[i].to_string());
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    out
}

fn keyed_changes<T: Serialize>(a: &[T], b: &[T], key: impl Fn(&T) -> String) -> Vec<ItemChange> {
    let before: BTreeMap<String, &T> = a.iter().map(|x| (key(x), x)).collect();
    let after: BTreeMap<String, &T> = b.iter().map(|x| (key(x), x)).collect();
    let mut out = Vec::new();
    for (id, old) in &before {
        match after.get(id) {
            None => out.push(ItemChange {
                id: id.clone(),
                change: Presence::Removed,
                fields: Vec::new(),
            }),
            Some(new) => {
                let mut fields = Vec::new();
                json_changes("", &to_json(old), &to_json(new), &mut fields);
                if !fields.is_empty() {
                    out.push(ItemChange {
                        id: id.clone(),
                        change: Presence::Changed,
                        fields,
                    });
                }
            }
        }
    }
    for id in after.keys().filter(|id| !before.contains_key(*id)) {
        out.push(ItemChange {
            id: id.clone(),
            change: Presence::Added,
            fields: Vec::new(),
        });
    }
    out
}

/// The semantic difference from `a` to `b`.
pub fn diff(a: &NotebookDoc, b: &NotebookDoc, context: usize) -> NotebookDiff {
    let mut out = NotebookDiff::default();
    for (field, l, r) in [
        ("title", to_json(&a.title), to_json(&b.title)),
        (
            "notebook_id",
            to_json(&a.notebook_id),
            to_json(&b.notebook_id),
        ),
        (
            "format_version",
            to_json(&a.format_version),
            to_json(&b.format_version),
        ),
        ("settings", to_json(&a.settings), to_json(&b.settings)),
        (
            "data_retention",
            to_json(&a.data_retention),
            to_json(&b.data_retention),
        ),
        ("extra", to_json(&a.extra), to_json(&b.extra)),
    ] {
        json_changes(field, &l, &r, &mut out.notebook);
    }
    out.parameters = keyed_changes(&a.parameters, &b.parameters, |p| p.name.clone());
    out.connections = keyed_changes(&a.connection_refs, &b.connection_refs, |c| c.id.clone());

    let ids_a: Vec<&str> = a.cells.iter().map(|c| c.id.as_str()).collect();
    let ids_b: Vec<&str> = b.cells.iter().map(|c| c.id.as_str()).collect();
    let shared_a: Vec<&str> = ids_a
        .iter()
        .copied()
        .filter(|id| b.cell(id).is_some())
        .collect();
    let shared_b: Vec<&str> = ids_b
        .iter()
        .copied()
        .filter(|id| a.cell(id).is_some())
        .collect();
    let stable = stable_ids(&shared_a, &shared_b);

    for (index, cell) in a.cells.iter().enumerate() {
        if b.cell(&cell.id).is_none() {
            let old = normalize_source(&cell.source);
            out.cells.push(CellDiff {
                id: cell.id.clone(),
                label: cell.relation_name().to_string(),
                kind: cell.kind.as_str().to_string(),
                change: Presence::Removed,
                moved: None,
                position: Some(index + 1),
                fields: Vec::new(),
                source_diff: (!old.is_empty()).then(|| unified(&old, "", context)),
            });
        }
    }
    for (index, cell) in b.cells.iter().enumerate() {
        let Some(old) = a.cell(&cell.id) else {
            let new = normalize_source(&cell.source);
            out.cells.push(CellDiff {
                id: cell.id.clone(),
                label: cell.relation_name().to_string(),
                kind: cell.kind.as_str().to_string(),
                change: Presence::Added,
                moved: None,
                position: Some(index + 1),
                fields: Vec::new(),
                source_diff: (!new.is_empty()).then(|| unified("", &new, context)),
            });
            continue;
        };
        let moved = (!stable.contains(&cell.id)).then(|| {
            let from = a.cell_index(&cell.id).unwrap_or(0) + 1;
            (from, index + 1)
        });
        let mut fields = Vec::new();
        json_changes("", &cell_fields(old), &cell_fields(cell), &mut fields);
        let (before, after) = (
            normalize_source(&old.source),
            normalize_source(&cell.source),
        );
        let source_diff = (before != after).then(|| unified(&before, &after, context));
        if moved.is_some() || !fields.is_empty() || source_diff.is_some() {
            out.cells.push(CellDiff {
                id: cell.id.clone(),
                label: cell.relation_name().to_string(),
                kind: cell.kind.as_str().to_string(),
                change: Presence::Changed,
                moved,
                position: None,
                fields,
                source_diff,
            });
        }
    }
    out
}

/// `value` with every object's keys in sorted order, so `cat` prints the same
/// bytes whether or not serde_json preserves insertion order in this build.
fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), sorted(&map[k])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

fn short(value: &Option<Value>) -> String {
    match value {
        None => "(none)".into(),
        Some(v) => {
            let text = serde_json::to_string(&sorted(v)).unwrap_or_default();
            if text.chars().count() > 80 {
                format!("{}…", text.chars().take(79).collect::<String>())
            } else {
                text
            }
        }
    }
}

fn field_line(prefix: &str, f: &FieldChange) -> String {
    format!(
        "{prefix}{}: {} -> {}\n",
        f.field,
        short(&f.before),
        short(&f.after)
    )
}

/// The diff as Git-style text.
pub fn render_text(diff: &NotebookDiff, a: &str, b: &str) -> String {
    if diff.is_empty() {
        return String::new();
    }
    let mut out = format!("--- {a}\n+++ {b}\n");
    for f in &diff.notebook {
        out.push_str(&field_line("notebook ", f));
    }
    for (what, items) in [
        ("parameter", &diff.parameters),
        ("connection", &diff.connections),
    ] {
        for item in items {
            match item.change {
                Presence::Added => out.push_str(&format!("{what} added: {}\n", item.id)),
                Presence::Removed => out.push_str(&format!("{what} removed: {}\n", item.id)),
                Presence::Changed => {
                    for f in &item.fields {
                        out.push_str(&field_line(&format!("{what} {} ", item.id), f));
                    }
                }
            }
        }
    }
    for cell in &diff.cells {
        let head = format!("cell {} \"{}\" ({})", cell.id, cell.label, cell.kind);
        match cell.change {
            Presence::Added => out.push_str(&format!(
                "{head} added at {}\n",
                cell.position.unwrap_or_default()
            )),
            Presence::Removed => out.push_str(&format!(
                "{head} removed from {}\n",
                cell.position.unwrap_or_default()
            )),
            Presence::Changed => {
                if let Some((from, to)) = cell.moved {
                    out.push_str(&format!("{head} moved {from} -> {to}\n"));
                }
                for f in &cell.fields {
                    let label = match f.field.as_str() {
                        "name" => "renamed",
                        _ => "",
                    };
                    if label.is_empty() {
                        out.push_str(&field_line(&format!("{head} "), f));
                    } else {
                        out.push_str(&format!(
                            "{head} {label}: {} -> {}\n",
                            short(&f.before),
                            short(&f.after)
                        ));
                    }
                }
                if cell.source_diff.is_some() {
                    out.push_str(&format!("{head} source:\n"));
                }
            }
        }
        if let Some(text) = &cell.source_diff {
            out.push_str(text);
        }
    }
    out
}

/// A stable, readable rendering of a notebook for `git diff` (textconv).
pub fn cat(doc: &NotebookDoc) -> String {
    let mut out = format!("# {}\n", doc.title);
    out.push_str(&format!(
        "notebook {} (format {})\n",
        doc.notebook_id, doc.format_version
    ));
    for p in &doc.parameters {
        out.push_str(&format!(
            "parameter {}: {}\n",
            p.name,
            serde_json::to_string(&sorted(&to_json(p))).unwrap_or_default()
        ));
    }
    for c in &doc.connection_refs {
        let mut line = format!("connection {}: {}", c.id, c.label);
        if let Some(db) = &c.database {
            line.push_str(&format!(" [{db}]"));
        }
        out.push_str(&line);
        out.push('\n');
    }
    for cell in &doc.cells {
        let mut flags = vec![cell.kind.as_str().to_string()];
        if let Some(slot) = &cell.connection_ref {
            flags.push(format!("connection {slot}"));
        }
        if cell.exec_class() == sqlly_services::notebook::ExecClass::Remote {
            flags.push(if cell.execution_policy.read_only {
                "read-only".into()
            } else {
                "writes".into()
            });
        }
        out.push_str(&format!(
            "\n---- cell {} \"{}\" ({})\n",
            cell.id,
            cell.relation_name(),
            flags.join(", ")
        ));
        let fields = cell_fields(cell);
        let mut rest = fields.clone();
        if let Value::Object(map) = &mut rest {
            for key in ["name", "kind", "connection_ref"] {
                map.remove(key);
            }
            if map.get("execution_policy")
                == Some(&to_json(
                    &sqlly_services::notebook::ExecutionPolicy::default(),
                ))
            {
                map.remove("execution_policy");
            }
        }
        let source = normalize_source(&cell.source);
        if !source.is_empty() {
            out.push_str(&source);
            out.push('\n');
        }
        if let Value::Object(map) = &rest {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            for key in keys {
                out.push_str(&format!(
                    "@{key} {}\n",
                    serde_json::to_string(&sorted(&map[key])).unwrap_or_default()
                ));
            }
        }
    }
    out
}

pub fn command(args: &DiffArgs, json: bool) -> ExitCode {
    let result = (|| -> Result<NotebookDiff, CliError> {
        let a = source::load_file(&args.a)?;
        let b = source::load_file(&args.b)?;
        Ok(diff(&a.doc, &b.doc, args.context))
    })();
    match result {
        Ok(found) => {
            let text = render_text(
                &found,
                &args.a.display().to_string(),
                &args.b.display().to_string(),
            );
            let exit = if args.exit_code && !found.is_empty() {
                ExitCode::Error
            } else {
                ExitCode::Ok
            };
            let data = serde_json::json!({"identical": found.is_empty(), "diff": found});
            super::finish(json, args.format, text.trim_end().to_string(), data, exit)
        }
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

pub fn cat_command(file: &std::path::Path, json: bool) -> ExitCode {
    match source::load_file(file) {
        Ok(loaded) => {
            let text = cat(&loaded.doc);
            crate::agent::emit(
                json,
                Some(text.trim_end().to_string()),
                Ok(serde_json::json!({"text": text})),
            )
        }
        Err(err) => crate::agent::emit(json, None, Err(err)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_testkit::golden::GoldenTest;
    use std::path::Path;

    fn golden(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("golden")
            .join("notebook")
            .join(name)
    }

    fn fixture(name: &str) -> NotebookDoc {
        let path = golden(name);
        source::load_file(&path).unwrap().doc
    }

    #[test]
    fn reformatting_alone_is_no_change() {
        let a = fixture("diff-a.sqllynb");
        let mut b = a.clone();
        for cell in &mut b.cells {
            cell.source = cell.source.replace('\n', "\r\n") + "\n\n";
            cell.collapsed = !cell.collapsed;
        }
        let found = diff(&a, &b, 3);
        assert!(found.is_empty(), "{found:?}");
        assert_eq!(render_text(&found, "a", "b"), "");
        // Canonical re-serialization (key order, spacing) is equally invisible.
        let reparsed = source::load_text(Path::new("x.sqllynb"), &a.to_canonical_json()).unwrap();
        assert!(diff(&a, &reparsed.doc, 3).is_empty());
    }

    #[test]
    fn semantic_diff_matches_golden() {
        let a = fixture("diff-a.sqllynb");
        let b = fixture("diff-b.sqllynb");
        let found = diff(&a, &b, 3);
        let text = render_text(&found, "a/report.sqllynb", "b/report.sqllynb");
        assert!(
            GoldenTest::compare(&text, golden("diff.golden.txt")).unwrap(),
            "diff output changed:\n{text}"
        );
        let json = serde_json::to_string_pretty(&found).unwrap();
        assert!(
            GoldenTest::compare(&json, golden("diff.golden.json")).unwrap(),
            "diff JSON changed:\n{json}"
        );
    }

    #[test]
    fn moves_are_relative_order_changes() {
        let a = fixture("diff-a.sqllynb");
        let mut b = a.clone();
        let last = b.cells.len() - 1;
        b.move_cell(last, 0);
        let found = diff(&a, &b, 3);
        let moved: Vec<&CellDiff> = found.cells.iter().filter(|c| c.moved.is_some()).collect();
        assert_eq!(moved.len(), 1, "{found:?}");
        assert_eq!(moved[0].moved, Some((last + 1, 1)));
    }

    #[test]
    fn cat_matches_golden() {
        let text = cat(&fixture("diff-b.sqllynb"));
        assert!(
            GoldenTest::compare(&text, golden("cat.golden.txt")).unwrap(),
            "cat output changed:\n{text}"
        );
    }
}
