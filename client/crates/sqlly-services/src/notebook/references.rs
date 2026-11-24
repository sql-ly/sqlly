//! `{{name}}`, `{{id}}` and `{{param.name}}` references in cell text, cell
//! and parameter renames that rewrite their dependents, and name rules.
//!
//! Only live text counts: in SQL, strings, comments and quoted names are
//! skipped (read the way the cell's engine reads them); in Markdown, code
//! spans, fenced code blocks and HTML comments are skipped.

use std::collections::BTreeMap;

use sqlly_lang::DisplayMessage;
use sqlly_lang::sqlmodel::{DatabaseType, Dialect, is_reserved_in_dialect};

use super::document::{Cell, CellKind, NotebookDoc, SourceLanguage};
use super::params::{ParamStyle, find_param_uses, template_param};
use super::scan::{is_identifier, mask_markdown, mask_sql};

/// The engine each cell's SQL is read as: by connection slot, else
/// `default`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dialects {
    pub default: DatabaseType,
    pub by_connection_ref: BTreeMap<String, DatabaseType>,
}

impl Dialects {
    pub fn single(db: DatabaseType) -> Self {
        Self {
            default: db,
            by_connection_ref: BTreeMap::new(),
        }
    }

    pub fn for_cell(&self, doc: &NotebookDoc, cell: &Cell) -> DatabaseType {
        doc.effective_connection_ref(cell)
            .and_then(|r| self.by_connection_ref.get(r))
            .copied()
            .unwrap_or(self.default)
    }
}

/// What a reference names, as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefTarget {
    /// A cell name or id.
    Cell(String),
    Param {
        name: String,
        field: Option<String>,
    },
}

/// One `{{…}}` reference; `start..end` covers the braces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    pub target: RefTarget,
    pub start: usize,
    pub end: usize,
}

/// Every reference in `text`, in order.
pub fn find_references(text: &str, language: SourceLanguage, db: DatabaseType) -> Vec<Reference> {
    let masked = match language {
        SourceLanguage::Sql => mask_sql(db, text),
        SourceLanguage::Markdown => mask_markdown(text),
        SourceLanguage::None => return Vec::new(),
    };
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(at) = masked[i..].find("{{") {
        let start = i + at;
        let body = start + 2;
        let Some(close) = masked[body..].find("}}") else {
            break;
        };
        let inner = &masked[body..body + close];
        if inner.contains(['{', '\n']) {
            i = start + 1;
            continue;
        }
        let end = body + close + 2;
        let target = if let Some((name, field)) = template_param(inner) {
            Some(RefTarget::Param {
                name: name.to_string(),
                field: field.map(str::to_string),
            })
        } else if is_identifier(inner.trim()) {
            Some(RefTarget::Cell(inner.trim().to_string()))
        } else {
            None
        };
        match target {
            Some(target) => {
                out.push(Reference { target, start, end });
                i = end;
            }
            None => i = start + 1,
        }
    }
    out
}

/// The references in a cell's `source`.
pub fn cell_references(doc: &NotebookDoc, cell: &Cell, dialects: &Dialects) -> Vec<Reference> {
    find_references(
        &cell.source,
        cell.kind.source_language(),
        dialects.for_cell(doc, cell),
    )
}

/// A reference resolved against the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    Cell { id: String },
    Param { name: String },
    UnknownCell { token: String },
    UnknownParam { name: String },
}

pub fn resolve(doc: &NotebookDoc, reference: &Reference) -> Resolved {
    match &reference.target {
        RefTarget::Cell(token) => match doc.resolve(token) {
            Some(cell) => Resolved::Cell {
                id: cell.id.clone(),
            },
            None => Resolved::UnknownCell {
                token: token.clone(),
            },
        },
        RefTarget::Param { name, .. } => match doc.parameter(name) {
            Some(def) => Resolved::Param {
                name: def.name.clone(),
            },
            None => Resolved::UnknownParam { name: name.clone() },
        },
    }
}

/// Why a name cannot be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameError {
    UnknownCell {
        id: String,
    },
    UnknownParameter {
        name: String,
    },
    /// Not `[A-Za-z_][A-Za-z0-9_]*` (at most 128 bytes).
    NotIdentifier,
    /// A SQL reserved word in some dialect, or `param`.
    Reserved,
    /// Another cell already has this name (case-insensitive).
    Duplicate {
        other: String,
    },
    /// Another cell's id is spelled this way.
    CollidesWithCellId {
        other: String,
    },
    CollidesWithParameter {
        name: String,
    },
    CollidesWithCellName {
        other: String,
    },
}

impl NameError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            NameError::UnknownCell { id } => DisplayMessage::new(
                "notebook.core.name.unknown_cell",
                format!("There is no cell {id}"),
            )
            .with_arg("id", id.clone()),
            NameError::UnknownParameter { name } => DisplayMessage::new(
                "notebook.core.name.unknown_parameter",
                format!("There is no parameter {name}"),
            )
            .with_arg("name", name.clone()),
            NameError::NotIdentifier => DisplayMessage::new(
                "notebook.core.name.not_identifier",
                "Use letters, digits and underscores, starting with a letter or underscore",
            ),
            NameError::Reserved => DisplayMessage::new(
                "notebook.core.name.reserved",
                "This word is reserved in SQL; choose another name",
            ),
            NameError::Duplicate { other } => DisplayMessage::new(
                "notebook.core.name.duplicate",
                format!("Cell {other} already uses this name"),
            )
            .with_arg("other", other.clone()),
            NameError::CollidesWithCellId { other } => DisplayMessage::new(
                "notebook.core.name.collides_with_cell_id",
                format!("This name is the id of cell {other}"),
            )
            .with_arg("other", other.clone()),
            NameError::CollidesWithParameter { name } => DisplayMessage::new(
                "notebook.core.name.collides_with_parameter",
                format!("A parameter is already called {name}"),
            )
            .with_arg("name", name.clone()),
            NameError::CollidesWithCellName { other } => DisplayMessage::new(
                "notebook.core.name.collides_with_cell_name",
                format!("Cell {other} is already called this"),
            )
            .with_arg("other", other.clone()),
        }
    }
}

fn check_identifier(name: &str) -> Result<(), NameError> {
    if !is_identifier(name) {
        return Err(NameError::NotIdentifier);
    }
    let reserved = name.eq_ignore_ascii_case("param")
        || [
            Dialect::SqlServer,
            Dialect::Postgres,
            Dialect::MySql,
            Dialect::Oracle,
            Dialect::Sqlite,
        ]
        .into_iter()
        .any(|d| is_reserved_in_dialect(d, name));
    if reserved {
        return Err(NameError::Reserved);
    }
    Ok(())
}

/// Whether `name` may be given to cell `cell_id`.
pub fn validate_cell_name(doc: &NotebookDoc, cell_id: &str, name: &str) -> Result<(), NameError> {
    if doc.cell(cell_id).is_none() {
        return Err(NameError::UnknownCell {
            id: cell_id.to_string(),
        });
    }
    // A v1 cell named after its own id is always fine.
    if name != cell_id {
        check_identifier(name)?;
    } else if !is_identifier(name) {
        return Err(NameError::NotIdentifier);
    }
    for other in doc.cells.iter().filter(|c| c.id != cell_id) {
        if other
            .name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
        {
            return Err(NameError::Duplicate {
                other: other.id.clone(),
            });
        }
        if other.id.eq_ignore_ascii_case(name) {
            return Err(NameError::CollidesWithCellId {
                other: other.id.clone(),
            });
        }
    }
    if let Some(def) = doc.parameter(name) {
        return Err(NameError::CollidesWithParameter {
            name: def.name.clone(),
        });
    }
    Ok(())
}

/// Whether parameter `old` (or a new parameter, `None`) may be called `name`.
pub fn validate_parameter_name(
    doc: &NotebookDoc,
    old: Option<&str>,
    name: &str,
) -> Result<(), NameError> {
    check_identifier(name)?;
    let same = |n: &str| old.is_some_and(|o| o.eq_ignore_ascii_case(n));
    if let Some(def) = doc.parameter(name)
        && !same(&def.name)
    {
        return Err(NameError::CollidesWithParameter {
            name: def.name.clone(),
        });
    }
    if let Some(cell) = doc.cells.iter().find(|c| {
        c.name
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
            || c.id.eq_ignore_ascii_case(name)
    }) {
        return Err(NameError::CollidesWithCellName {
            other: cell.id.clone(),
        });
    }
    Ok(())
}

/// Every naming problem in the document, by cell id (or parameter name),
/// in document order.
pub fn validate_names(doc: &NotebookDoc) -> Vec<(String, NameError)> {
    let mut out = Vec::new();
    for cell in &doc.cells {
        if let Some(name) = &cell.name
            && let Err(error) = validate_cell_name(doc, &cell.id, name)
        {
            out.push((cell.id.clone(), error));
        }
    }
    for (i, def) in doc.parameters.iter().enumerate() {
        let duplicate = doc.parameters[..i]
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case(&def.name));
        let result = if duplicate {
            Err(NameError::CollidesWithParameter {
                name: def.name.clone(),
            })
        } else {
            validate_parameter_name(doc, Some(&def.name), &def.name)
        };
        if let Err(error) = result {
            out.push((def.name.clone(), error));
        }
    }
    out
}

fn rewrite(text: &str, edits: &[(usize, usize, String)]) -> String {
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

/// Give cell `cell_id` a new name (or clear it) and rewrite every
/// `{{old name}}` that pointed at it; references by id are left alone.
/// Returns the ids of the cells whose text changed.
pub fn rename_cell(
    doc: &mut NotebookDoc,
    cell_id: &str,
    new_name: Option<&str>,
    dialects: &Dialects,
) -> Result<Vec<String>, NameError> {
    let Some(cell) = doc.cell(cell_id) else {
        return Err(NameError::UnknownCell {
            id: cell_id.to_string(),
        });
    };
    if let Some(name) = new_name {
        validate_cell_name(doc, cell_id, name)?;
    }
    let old_name = cell.name.clone();
    let replacement = new_name.unwrap_or(cell_id).to_string();
    let mut changed = Vec::new();
    if let Some(old) = old_name.as_deref() {
        let mut updates = Vec::new();
        for other in &doc.cells {
            if other.kind == CellKind::Scalar {
                let spans = crate::analytics::scalar::reference_spans(&other.source, old)
                    .unwrap_or_default();
                if !spans.is_empty() && old != replacement {
                    let text = crate::analytics::scalar::reference_text(&replacement);
                    let edits: Vec<(usize, usize, String)> = spans
                        .into_iter()
                        .map(|(s, e)| (s, e, text.clone()))
                        .collect();
                    updates.push((other.id.clone(), rewrite(&other.source, &edits)));
                }
                continue;
            }
            let edits: Vec<(usize, usize, String)> = cell_references(doc, other, dialects)
                .into_iter()
                .filter(|r| {
                    matches!(&r.target, RefTarget::Cell(token) if token.eq_ignore_ascii_case(old))
                })
                .map(|r| (r.start, r.end, format!("{{{{{replacement}}}}}")))
                .collect();
            if !edits.is_empty() && old != replacement {
                updates.push((other.id.clone(), rewrite(&other.source, &edits)));
            }
        }
        for (id, source) in updates {
            if let Some(target) = doc.cell_mut(&id) {
                target.source = source;
            }
            changed.push(id);
        }
    }
    if let Some(target) = doc.cell_mut(cell_id) {
        target.name = new_name.map(str::to_string);
    }
    Ok(changed)
}

/// Rename parameter `old` to `new`, rewriting `:old`, `@old` and
/// `{{param.old…}}` in every cell. Returns the ids of the cells changed.
pub fn rename_parameter(
    doc: &mut NotebookDoc,
    old: &str,
    new: &str,
    dialects: &Dialects,
) -> Result<Vec<String>, NameError> {
    let Some(index) = doc
        .parameters
        .iter()
        .position(|p| p.name.eq_ignore_ascii_case(old))
    else {
        return Err(NameError::UnknownParameter {
            name: old.to_string(),
        });
    };
    validate_parameter_name(doc, Some(old), new)?;
    let old_name = doc.parameters[index].name.clone();
    let mut updates = Vec::new();
    for cell in &doc.cells {
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        match cell.kind.source_language() {
            SourceLanguage::Sql => {
                let db = dialects.for_cell(doc, cell);
                for u in find_param_uses(db, &cell.source, &doc.parameters) {
                    if u.name != old_name {
                        continue;
                    }
                    let text = match (u.style, &u.field) {
                        (ParamStyle::Colon, _) => format!(":{new}"),
                        (ParamStyle::At, _) => format!("@{new}"),
                        (ParamStyle::Template, Some(field)) => {
                            format!("{{{{param.{new}.{field}}}}}")
                        }
                        (ParamStyle::Template, None) => format!("{{{{param.{new}}}}}"),
                    };
                    edits.push((u.start, u.end, text));
                }
            }
            SourceLanguage::Markdown => {
                for r in cell_references(doc, cell, dialects) {
                    if let RefTarget::Param { name, field } = &r.target
                        && name.eq_ignore_ascii_case(&old_name)
                    {
                        let text = match field {
                            Some(field) => format!("{{{{param.{new}.{field}}}}}"),
                            None => format!("{{{{param.{new}}}}}"),
                        };
                        edits.push((r.start, r.end, text));
                    }
                }
            }
            SourceLanguage::None => {}
        }
        if !edits.is_empty() {
            updates.push((cell.id.clone(), rewrite(&cell.source, &edits)));
        }
    }
    let mut changed = Vec::new();
    for (id, source) in updates {
        if let Some(cell) = doc.cell_mut(&id) {
            cell.source = source;
        }
        changed.push(id);
    }
    doc.parameters[index].name = new.to_string();
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::{CellKind, SeededIds};
    use crate::notebook::params::{ParamType, ParameterDef};

    fn refs(text: &str) -> Vec<RefTarget> {
        find_references(text, SourceLanguage::Sql, DatabaseType::SqlServer)
            .into_iter()
            .map(|r| r.target)
            .collect()
    }

    fn cell_ref(s: &str) -> RefTarget {
        RefTarget::Cell(s.into())
    }

    fn doc_with(sources: &[(&str, Option<&str>, CellKind, &str)]) -> NotebookDoc {
        let mut doc = NotebookDoc::empty("n", "t");
        for (id, name, kind, source) in sources {
            let mut cell = Cell::new(*id, kind.clone(), Default::default());
            cell.name = name.map(str::to_string);
            cell.source = source.to_string();
            doc.cells.push(cell);
        }
        doc
    }

    #[test]
    fn finds_cell_and_param_references_in_order() {
        assert_eq!(
            refs(
                "SELECT * FROM {{orders}} o JOIN {{ c_abc }} c ON 1=1 WHERE d > {{param.from}} AND {{param.r.start}}"
            ),
            vec![
                cell_ref("orders"),
                cell_ref("c_abc"),
                RefTarget::Param {
                    name: "from".into(),
                    field: None
                },
                RefTarget::Param {
                    name: "r".into(),
                    field: Some("start".into())
                },
            ]
        );
    }

    #[test]
    fn skips_strings_comments_and_quoted_names() {
        let sql = "SELECT '{{a}}', N'{{b}}', [{{c}}], \"{{d}}\" -- {{e}}\n/* {{f}} */ FROM {{g}}";
        assert_eq!(refs(sql), vec![cell_ref("g")]);
        let pg = "SELECT $body$ {{a}} $body$, E'\\' {{b}}', {{c}}";
        let found: Vec<_> = find_references(pg, SourceLanguage::Sql, DatabaseType::Postgres)
            .into_iter()
            .map(|r| r.target)
            .collect();
        assert_eq!(found, vec![cell_ref("c")]);
        let mysql = "SELECT `{{a}}` # {{b}}\nFROM {{c}}";
        let found: Vec<_> = find_references(mysql, SourceLanguage::Sql, DatabaseType::MySql)
            .into_iter()
            .map(|r| r.target)
            .collect();
        assert_eq!(found, vec![cell_ref("c")]);
    }

    #[test]
    fn ignores_malformed_tokens() {
        assert!(refs("{{}} {{1abc}} {{a b}} {{a-b}} {{param.}} {{param.1x}} {{x").is_empty());
        assert_eq!(refs("{{{x}}}"), vec![cell_ref("x")]);
        assert_eq!(refs("{{a\n}} {{b}}"), vec![cell_ref("b")]);
    }

    #[test]
    fn spans_cover_the_braces() {
        let sql = "SELECT * FROM {{ orders }}";
        let r = &find_references(sql, SourceLanguage::Sql, DatabaseType::SqlServer)[0];
        assert_eq!(&sql[r.start..r.end], "{{ orders }}");
    }

    #[test]
    fn markdown_references_skip_code() {
        let md = "Revenue is {{total}}.\n\n```sql\nSELECT {{hidden}}\n```\nUse `{{also_hidden}}` for {{param.region}}.";
        let found: Vec<_> = find_references(md, SourceLanguage::Markdown, DatabaseType::SqlServer)
            .into_iter()
            .map(|r| r.target)
            .collect();
        assert_eq!(
            found,
            vec![
                cell_ref("total"),
                RefTarget::Param {
                    name: "region".into(),
                    field: None
                }
            ]
        );
        assert!(find_references("{{x}}", SourceLanguage::None, DatabaseType::SqlServer).is_empty());
    }

    #[test]
    fn resolves_names_ids_and_params() {
        let mut doc = doc_with(&[
            ("c_1", Some("orders"), CellKind::Sql, "SELECT 1"),
            (
                "c_2",
                None,
                CellKind::Sql,
                "SELECT * FROM {{ORDERS}} JOIN {{c_1}} JOIN {{nope}} WHERE {{param.d}} AND {{param.zz}}",
            ),
        ]);
        doc.parameters.push(ParameterDef::new("D", ParamType::Date));
        let cell = doc.cells[1].clone();
        let resolved: Vec<_> = cell_references(&doc, &cell, &Dialects::default())
            .iter()
            .map(|r| resolve(&doc, r))
            .collect();
        assert_eq!(
            resolved,
            vec![
                Resolved::Cell { id: "c_1".into() },
                Resolved::Cell { id: "c_1".into() },
                Resolved::UnknownCell {
                    token: "nope".into()
                },
                Resolved::Param { name: "D".into() },
                Resolved::UnknownParam { name: "zz".into() },
            ]
        );
    }

    #[test]
    fn rename_rewrites_scalar_formulas_that_name_the_cell() {
        let mut doc = doc_with(&[
            ("c_1", Some("orders"), CellKind::Sql, "SELECT 1"),
            (
                "c_2",
                Some("total"),
                CellKind::Scalar,
                "sum(Orders.amount) / count(\"orders\") + orders.n[0] + c_1.n + param.orders + x.orders",
            ),
            ("c_3", None, CellKind::Scalar, "1 + 2"),
        ]);
        let changed =
            rename_cell(&mut doc, "c_1", Some("sales_2026"), &Dialects::default()).unwrap();
        assert_eq!(changed, vec!["c_2"]);
        assert_eq!(
            doc.cells[1].source,
            "sum(sales_2026.amount) / count(sales_2026) + sales_2026.n[0] + c_1.n + param.orders + x.orders"
        );
        assert_eq!(
            crate::analytics::scalar::reference_text("two words"),
            "\"two words\""
        );
        crate::analytics::scalar::parse(&doc.cells[1].source).expect("still a formula");
        rename_cell(&mut doc, "c_1", Some("sales"), &Dialects::default()).unwrap();
        assert!(
            doc.cells[1]
                .source
                .starts_with("sum(sales.amount) / count(sales)")
        );
    }

    #[test]
    fn rename_rewrites_dependents_by_name_only() {
        let mut doc = doc_with(&[
            ("c_1", Some("orders"), CellKind::Sql, "SELECT 1"),
            (
                "c_2",
                None,
                CellKind::Sql,
                "SELECT * FROM {{orders}} JOIN {{c_1}} WHERE s = '{{orders}}'",
            ),
            (
                "c_3",
                None,
                CellKind::Markdown,
                "We have {{ Orders }} rows; `{{orders}}`",
            ),
            ("c_4", None, CellKind::Sql, "SELECT 2"),
        ]);
        let changed = rename_cell(&mut doc, "c_1", Some("sales"), &Dialects::default()).unwrap();
        assert_eq!(changed, vec!["c_2", "c_3"]);
        assert_eq!(doc.cells[0].name.as_deref(), Some("sales"));
        assert_eq!(
            doc.cells[1].source,
            "SELECT * FROM {{sales}} JOIN {{c_1}} WHERE s = '{{orders}}'"
        );
        assert_eq!(doc.cells[2].source, "We have {{sales}} rows; `{{orders}}`");
        // Clearing the name falls back to the id.
        let changed = rename_cell(&mut doc, "c_1", None, &Dialects::default()).unwrap();
        assert_eq!(changed, vec!["c_2", "c_3"]);
        assert_eq!(
            doc.cells[1].source,
            "SELECT * FROM {{c_1}} JOIN {{c_1}} WHERE s = '{{orders}}'"
        );
        assert_eq!(doc.cells[0].name, None);
        // Naming a cell that had no name rewrites nothing.
        assert!(
            rename_cell(&mut doc, "c_1", Some("x1"), &Dialects::default())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rename_refuses_bad_names() {
        let mut doc = doc_with(&[
            ("c_1", Some("a"), CellKind::Sql, ""),
            ("c_2", Some("b"), CellKind::Sql, ""),
        ]);
        doc.parameters
            .push(ParameterDef::new("region", ParamType::String));
        let d = Dialects::default();
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("B"), &d),
            Err(NameError::Duplicate {
                other: "c_2".into()
            })
        );
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("c_2"), &d),
            Err(NameError::CollidesWithCellId {
                other: "c_2".into()
            })
        );
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("Region"), &d),
            Err(NameError::CollidesWithParameter {
                name: "region".into()
            })
        );
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("select"), &d),
            Err(NameError::Reserved)
        );
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("param"), &d),
            Err(NameError::Reserved)
        );
        assert_eq!(
            rename_cell(&mut doc, "c_1", Some("2x"), &d),
            Err(NameError::NotIdentifier)
        );
        assert_eq!(
            rename_cell(&mut doc, "zz", Some("ok"), &d),
            Err(NameError::UnknownCell { id: "zz".into() })
        );
        assert_eq!(doc.cells[0].name.as_deref(), Some("a"));
        // A v1 cell may carry its own id as its name.
        assert!(validate_cell_name(&doc, "c_1", "c_1").is_ok());
    }

    #[test]
    fn validate_names_reports_every_problem() {
        let mut doc = doc_with(&[
            ("c_1", Some("dup"), CellKind::Sql, ""),
            ("c_2", Some("DUP"), CellKind::Sql, ""),
            ("c_3", Some("ok"), CellKind::Sql, ""),
        ]);
        doc.parameters
            .push(ParameterDef::new("ok", ParamType::String));
        doc.parameters
            .push(ParameterDef::new("p", ParamType::String));
        doc.parameters
            .push(ParameterDef::new("P", ParamType::String));
        let issues = validate_names(&doc);
        let ids: Vec<&str> = issues.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["c_1", "c_2", "c_3", "ok", "P"]);
    }

    #[test]
    fn rename_parameter_rewrites_every_style() {
        let mut doc = doc_with(&[
            (
                "c_1",
                None,
                CellKind::Sql,
                "SELECT :region, @Region, {{param.region}}, ':region' -- :region\nFROM t WHERE r = :regions",
            ),
            (
                "c_2",
                None,
                CellKind::Markdown,
                "For {{param.region}} and {{param.period.start}}",
            ),
        ]);
        doc.parameters
            .push(ParameterDef::new("region", ParamType::String));
        doc.parameters
            .push(ParameterDef::new("period", ParamType::DateRange));
        let changed = rename_parameter(&mut doc, "REGION", "area", &Dialects::default()).unwrap();
        assert_eq!(changed, vec!["c_1", "c_2"]);
        assert_eq!(
            doc.cells[0].source,
            "SELECT :area, @area, {{param.area}}, ':region' -- :region\nFROM t WHERE r = :regions"
        );
        assert_eq!(
            doc.cells[1].source,
            "For {{param.area}} and {{param.period.start}}"
        );
        assert_eq!(doc.parameters[0].name, "area");
        rename_parameter(&mut doc, "period", "span", &Dialects::default()).unwrap();
        assert_eq!(
            doc.cells[1].source,
            "For {{param.area}} and {{param.span.start}}"
        );
        assert!(rename_parameter(&mut doc, "area", "span", &Dialects::default()).is_err());
        assert!(rename_parameter(&mut doc, "missing", "x", &Dialects::default()).is_err());
    }

    #[test]
    fn dialects_follow_connection_slots() {
        let mut ids = SeededIds(1);
        let mut doc = NotebookDoc::new("t", &mut ids);
        doc.add_cell(CellKind::Sql, None, &mut ids);
        doc.cells[1].connection_ref = Some("pg".into());
        let mut dialects = Dialects::single(DatabaseType::SqlServer);
        dialects
            .by_connection_ref
            .insert("pg".into(), DatabaseType::Postgres);
        assert_eq!(
            dialects.for_cell(&doc, &doc.cells[0]),
            DatabaseType::SqlServer
        );
        assert_eq!(
            dialects.for_cell(&doc, &doc.cells[1]),
            DatabaseType::Postgres
        );
    }

    #[test]
    fn randomized_text_never_yields_references_inside_literals() {
        // Fixed-seed generator: references planted inside strings and
        // comments are never found; ones planted in code always are.
        let mut ids = SeededIds(42);
        let mut byte = || {
            let mut b = [0u8; 1];
            crate::notebook::document::IdSource::fill(&mut ids, &mut b);
            b[0]
        };
        for _ in 0..500 {
            let mut sql = String::from("SELECT ");
            let mut expected = Vec::new();
            for n in 0..(byte() % 6) {
                let name = format!("n{n}");
                match byte() % 5 {
                    0 => sql.push_str(&format!("'x {{{{{name}}}}} y' ")),
                    1 => sql.push_str(&format!("/* {{{{{name}}}}} */ ")),
                    2 => sql.push_str(&format!("-- {{{{{name}}}}}\n")),
                    3 => sql.push_str(&format!("[{{{{{name}}}}}] ")),
                    _ => {
                        sql.push_str(&format!("{{{{{name}}}}} "));
                        expected.push(cell_ref(&name));
                    }
                }
            }
            assert_eq!(refs(&sql), expected, "{sql}");
        }
    }

    /// Renaming any cell to any valid fresh name never changes what any
    /// reference points at.
    #[test]
    fn randomized_renames_preserve_the_graph() {
        use crate::notebook::graph::Graph;
        let mut src = SeededIds(5150);
        let mut next = |n: usize| {
            let mut b = [0u8; 1];
            crate::notebook::document::IdSource::fill(&mut src, &mut b);
            b[0] as usize % n
        };
        let d = Dialects::default();
        for round in 0..200 {
            let n = 1 + next(7);
            let mut doc = NotebookDoc::empty("n", "t");
            for i in 0..n {
                let kind = if next(4) == 0 {
                    CellKind::Markdown
                } else {
                    CellKind::Sql
                };
                let mut cell = Cell::new(format!("c_{i}"), kind, Default::default());
                if next(2) == 0 {
                    cell.name = Some(format!("name{i}"));
                }
                doc.cells.push(cell);
            }
            for i in 0..n {
                let mut text = String::from("SELECT 1");
                for _ in 0..next(4) {
                    let j = next(n);
                    let token = match (&doc.cells[j].name, next(2)) {
                        (Some(name), 0) => name.to_uppercase(),
                        _ => doc.cells[j].id.clone(),
                    };
                    let wrap = ["{{T}}", "'{{T}}'", "-- {{T}}\n", "{{ T }}"][next(4)];
                    text.push(' ');
                    text.push_str(&wrap.replace('T', &token));
                }
                doc.cells[i].source = text;
            }
            let edges = |doc: &NotebookDoc| {
                let g = Graph::build(doc, &d);
                doc.cells
                    .iter()
                    .map(|c| {
                        g.upstream(&crate::notebook::graph::Node::cell(c.id.as_str()))
                            .to_vec()
                    })
                    .collect::<Vec<_>>()
            };
            let before = edges(&doc);
            let target = doc.cells[next(n)].id.clone();
            let new_name = if next(3) == 0 {
                None
            } else {
                Some(format!("renamed{round}"))
            };
            rename_cell(&mut doc, &target, new_name.as_deref(), &d).unwrap();
            assert_eq!(edges(&doc), before, "round {round}");
        }
    }
}
