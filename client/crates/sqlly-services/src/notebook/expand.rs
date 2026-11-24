//! Inlining referenced SQL cells as CTEs: `SELECT * FROM {{orders}}` runs
//! as `WITH orders AS (…) SELECT * FROM orders`.
//!
//! A referenced cell is inlined only when it is a single read-only query on
//! the same connection as the cell that uses it. Upstream cells on another
//! connection, already materialized locally, or of a non-SQL kind cannot be
//! inlined: the result is [`Expansion::NeedsLocalMaterialization`] and the
//! host routes the query through the notebook's local database instead.
//!
//! Engines that allow `WITH` inside a CTE body get each upstream nested
//! whole. SQL Server does not, so an upstream's own CTEs are hoisted into
//! the outer list (refusing names that would collide or shadow), and a
//! trailing `ORDER BY` without `TOP`/`OFFSET`, which SQL Server rejects in a
//! CTE, is dropped.

use std::collections::{BTreeMap, BTreeSet};

use sqlly_intel::query::safety::classify_select_only_for;
use sqlly_lang::DisplayMessage;
use sqlly_lang::sqllex::{StrictLexer, Token, TokenKind, split_batches, split_statements};
use sqlly_lang::sqlmodel::{DatabaseType, Dialect};

use super::document::{Cell, CellKind, ExecClass, NotebookDoc};
use super::references::{Dialects, RefTarget, cell_references};
use super::scan::is_identifier;

/// Why an upstream query cannot be inlined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InlineRefusal {
    Empty,
    MultipleStatements,
    /// Does not start with `SELECT`, `WITH` or `(`.
    NotAQuery,
    /// Not SELECT-only under the engine's reading.
    NotReadOnly,
    /// A clause a CTE body cannot hold (`FOR XML`, `FOR JSON`, `FOR BROWSE`).
    TrailingClause,
    /// A leading `WITH` this expander cannot take apart.
    UnsupportedWith,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpandError {
    UnknownCell {
        id: String,
    },
    /// Only SQL cells expand.
    NotSql {
        cell: String,
    },
    UnknownReference {
        cell: String,
        token: String,
    },
    /// A Markdown, parameter or layout cell used as a table.
    NotARelation {
        cell: String,
        referenced: String,
    },
    /// Closed path of cell ids.
    Cycle {
        path: Vec<String>,
    },
    NotInlinable {
        cell: String,
        reason: InlineRefusal,
    },
    /// Two CTEs would share `name`, or a hoisted CTE would shadow a name
    /// used elsewhere.
    CteNameConflict {
        name: String,
        cells: Vec<String>,
    },
    /// The cell references other cells from more than one of its
    /// statements; the referenced queries can be put in front of one only.
    ReferencesSpanStatements {
        cell: String,
    },
}

impl ExpandError {
    pub fn message(&self) -> DisplayMessage {
        match self {
            ExpandError::UnknownCell { id } => DisplayMessage::new(
                "notebook.core.expand.unknown_cell",
                format!("There is no cell {id}"),
            )
            .with_arg("id", id.clone()),
            ExpandError::NotSql { cell } => DisplayMessage::new(
                "notebook.core.expand.not_sql",
                format!("Cell {cell} is not a SQL cell"),
            )
            .with_arg("cell", cell.clone()),
            ExpandError::UnknownReference { token, .. } => DisplayMessage::new(
                "notebook.core.expand.unknown_reference",
                format!("No cell is named {token}"),
            )
            .with_arg("token", token.clone()),
            ExpandError::NotARelation { referenced, .. } => DisplayMessage::new(
                "notebook.core.expand.not_a_relation",
                format!("Cell {referenced} has no table to select from"),
            )
            .with_arg("referenced", referenced.clone()),
            ExpandError::Cycle { path } => {
                let path = path.join(" → ");
                DisplayMessage::new(
                    "notebook.core.cycle.loop",
                    format!("These cells depend on each other in a loop: {path}"),
                )
                .with_arg("path", path)
            }
            ExpandError::NotInlinable { cell, reason } => {
                let (key, text) = match reason {
                    InlineRefusal::Empty => ("notebook.core.expand.empty", "is empty"),
                    InlineRefusal::MultipleStatements => (
                        "notebook.core.expand.multiple_statements",
                        "holds more than one statement",
                    ),
                    InlineRefusal::NotAQuery => {
                        ("notebook.core.expand.not_a_query", "is not a query")
                    }
                    InlineRefusal::NotReadOnly => (
                        "notebook.core.expand.not_read_only",
                        "is not a read-only query",
                    ),
                    InlineRefusal::TrailingClause => (
                        "notebook.core.expand.trailing_clause",
                        "ends with a clause a CTE cannot hold",
                    ),
                    InlineRefusal::UnsupportedWith => (
                        "notebook.core.expand.unsupported_with",
                        "starts with a WITH clause that cannot be combined",
                    ),
                };
                DisplayMessage::new(
                    key,
                    format!("Cell {cell} {text}, so it cannot be used as a table"),
                )
                .with_arg("cell", cell.clone())
            }
            ExpandError::CteNameConflict { name, cells } => DisplayMessage::new(
                "notebook.core.expand.cte_name_conflict",
                format!(
                    "The name {name} is used by more than one of: {}",
                    cells.join(", ")
                ),
            )
            .with_arg("name", name.clone())
            .with_arg("cells", cells.join(", ")),
            ExpandError::ReferencesSpanStatements { cell } => DisplayMessage::new(
                "notebook.core.expand.references_span_statements",
                format!(
                    "Cell {cell} uses other cells in more than one statement; keep the references in one statement"
                ),
            )
            .with_arg("cell", cell.clone()),
        }
    }
}

/// The SQL to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedSql {
    pub sql: String,
    /// Cells inlined as CTEs, dependencies first.
    pub inlined: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expansion {
    Sql(ExpandedSql),
    /// These upstream cells must be materialized locally first (dependency
    /// order); route the cell through the local database.
    NeedsLocalMaterialization {
        cells: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy)]
pub struct ExpandOptions<'a> {
    pub dialects: &'a Dialects,
    /// Cells whose output already lives in the local database.
    pub materialized: &'a BTreeSet<String>,
}

fn relation_sql(db: DatabaseType, name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        db.quote_identifier(name)
    }
}

/// Replace every `{{x}}` naming a cell with `name_for(cell)`; unknown
/// references are errors, parameters are left for binding.
fn replace_refs(
    doc: &NotebookDoc,
    cell: &Cell,
    dialects: &Dialects,
    name_for: &dyn Fn(&Cell) -> String,
) -> Result<String, ExpandError> {
    let mut out = String::with_capacity(cell.source.len());
    let mut last = 0;
    for r in cell_references(doc, cell, dialects) {
        let RefTarget::Cell(token) = &r.target else {
            continue;
        };
        let target = doc
            .resolve(token)
            .ok_or_else(|| ExpandError::UnknownReference {
                cell: cell.id.clone(),
                token: token.clone(),
            })?;
        out.push_str(&cell.source[last..r.start]);
        out.push_str(&name_for(target));
        last = r.end;
    }
    out.push_str(&cell.source[last..]);
    Ok(out)
}

/// A cell's SQL with each `{{x}}` replaced by a relation name the host
/// chooses (for example a local materialized table).
pub fn substitute_relations(
    doc: &NotebookDoc,
    cell_id: &str,
    dialects: &Dialects,
    name_for: &dyn Fn(&Cell) -> String,
) -> Result<String, ExpandError> {
    let cell = doc.cell(cell_id).ok_or_else(|| ExpandError::UnknownCell {
        id: cell_id.to_string(),
    })?;
    replace_refs(doc, cell, dialects, name_for)
}

struct Walk<'a> {
    doc: &'a NotebookDoc,
    opts: ExpandOptions<'a>,
    target_slot: Option<&'a str>,
    /// The target runs on the notebook's local data, so every upstream is
    /// read from there rather than inlined.
    target_local: bool,
    state: BTreeMap<String, bool>,
    stack: Vec<String>,
    inline: Vec<String>,
    local: Vec<String>,
}

impl<'a> Walk<'a> {
    fn visit(&mut self, cell: &'a Cell) -> Result<(), ExpandError> {
        self.state.insert(cell.id.clone(), false);
        self.stack.push(cell.id.clone());
        for r in cell_references(self.doc, cell, self.opts.dialects) {
            let RefTarget::Cell(token) = &r.target else {
                continue;
            };
            let dep = self
                .doc
                .resolve(token)
                .ok_or_else(|| ExpandError::UnknownReference {
                    cell: cell.id.clone(),
                    token: token.clone(),
                })?;
            match self.state.get(&dep.id) {
                Some(false) => {
                    let at = self.stack.iter().position(|id| *id == dep.id).unwrap_or(0);
                    let mut path = self.stack[at..].to_vec();
                    path.push(dep.id.clone());
                    return Err(ExpandError::Cycle { path });
                }
                Some(true) => continue,
                None => {}
            }
            match dep.kind.exec_class() {
                ExecClass::Instant => {
                    return Err(ExpandError::NotARelation {
                        cell: cell.id.clone(),
                        referenced: dep.id.clone(),
                    });
                }
                _ if dep.kind != CellKind::Sql
                    || self.target_local
                    || dep.exec_class() == ExecClass::Local
                    || self.opts.materialized.contains(&dep.id)
                    || self.doc.effective_connection_ref(dep) != self.target_slot =>
                {
                    self.state.insert(dep.id.clone(), true);
                    self.local.push(dep.id.clone());
                }
                _ => {
                    self.visit(dep)?;
                    self.inline.push(dep.id.clone());
                }
            }
        }
        self.stack.pop();
        self.state.insert(cell.id.clone(), true);
        Ok(())
    }
}

fn significant<'t>(tokens: &'t [Token<'t>]) -> Vec<&'t Token<'t>> {
    tokens.iter().filter(|t| !t.is_trivia()).collect()
}

fn word(token: &Token<'_>, text: &str) -> bool {
    token.is_word() && token.text.eq_ignore_ascii_case(text)
}

/// `sql` without trailing whitespace, comments and semicolons.
fn trim_tail(db: DatabaseType, sql: &str) -> String {
    let tokens = StrictLexer::tokenize(sql, db.family());
    let end = tokens
        .iter()
        .rev()
        .find(|t| !t.is_trivia() && t.kind != TokenKind::Semicolon)
        .map_or(0, |t| t.span.end);
    let start = tokens
        .iter()
        .find(|t| !t.is_trivia())
        .map_or(0, |t| t.span.start);
    sql[start.min(end)..end].to_string()
}

/// One CTE of a leading `WITH`: its normalized name and byte range.
struct CteItem {
    name: String,
    start: usize,
    end: usize,
}

struct WithParts {
    recursive: bool,
    items: Vec<CteItem>,
    main_start: usize,
}

/// Take apart `WITH [RECURSIVE] a [(cols)] AS [[NOT] MATERIALIZED] (…), … main`.
/// `None` when the text has no leading `WITH`; `Some(Err(()))` when it has
/// one this parser does not understand.
fn split_with(db: DatabaseType, sql: &str) -> Option<Result<WithParts, ()>> {
    let tokens = StrictLexer::tokenize(sql, db.family());
    let sig = significant(&tokens);
    if !sig.first().is_some_and(|t| word(t, "with")) {
        return None;
    }
    let mut i = 1;
    let recursive = sig.get(i).is_some_and(|t| word(t, "recursive"));
    if recursive {
        i += 1;
    }
    let close_of = |open: usize| -> Option<usize> {
        let mut depth = 0i32;
        for (j, t) in sig.iter().enumerate().skip(open) {
            match t.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(j);
                    }
                }
                _ => {}
            }
        }
        None
    };
    let mut items = Vec::new();
    loop {
        let Some(name) = sig.get(i).filter(|t| t.is_word()) else {
            return Some(Err(()));
        };
        let start = name.span.start;
        i += 1;
        if sig.get(i).is_some_and(|t| t.kind == TokenKind::LParen) {
            let Some(close) = close_of(i) else {
                return Some(Err(()));
            };
            i = close + 1;
        }
        if !sig.get(i).is_some_and(|t| word(t, "as")) {
            return Some(Err(()));
        }
        i += 1;
        if sig.get(i).is_some_and(|t| word(t, "not")) {
            i += 1;
        }
        if sig.get(i).is_some_and(|t| word(t, "materialized")) {
            i += 1;
        }
        if !sig.get(i).is_some_and(|t| t.kind == TokenKind::LParen) {
            return Some(Err(()));
        }
        let Some(close) = close_of(i) else {
            return Some(Err(()));
        };
        items.push(CteItem {
            name: name.ident_text().to_ascii_lowercase(),
            start,
            end: sig[close].span.end,
        });
        i = close + 1;
        match sig.get(i) {
            Some(t) if t.kind == TokenKind::Comma => i += 1,
            Some(t) if t.is_word() || t.kind == TokenKind::LParen => {
                return Some(Ok(WithParts {
                    recursive,
                    items,
                    main_start: t.span.start,
                }));
            }
            _ => return Some(Err(())),
        }
    }
}

/// SQL Server only: drop a top-level trailing `ORDER BY` that has no `TOP`
/// or `OFFSET` to give it meaning. Refuses `FOR XML/JSON/BROWSE` and
/// `OPTION (…)` query hints, which a CTE body cannot hold.
///
/// `TOP` counts only in the select list of a query with no `UNION`,
/// `EXCEPT` or `INTERSECT` (there the `ORDER BY` sorts the whole set, which
/// a `TOP` in one branch does not limit), and `OFFSET` only as the clause
/// after the `ORDER BY` (`OFFSET n ROWS`), not a column named `offset`.
fn tsql_inline_body(sql: &str) -> Result<String, InlineRefusal> {
    let tokens = StrictLexer::tokenize(sql, Dialect::SqlServer);
    let sig = significant(&tokens);
    let mut depth = 0i32;
    let mut order_at: Option<usize> = None;
    let mut top = false;
    let mut offset = false;
    let mut set_operation = false;
    for (i, t) in sig.iter().enumerate() {
        match t.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => depth -= 1,
            _ if depth == 0 => {
                let previous = i.checked_sub(1).and_then(|p| sig.get(p));
                if word(t, "top")
                    && previous
                        .is_some_and(|p| word(p, "select") || word(p, "distinct") || word(p, "all"))
                {
                    top = true;
                }
                if word(t, "offset")
                    && order_at.is_some()
                    && sig.get(i + 1).is_some_and(|n| {
                        matches!(
                            n.kind,
                            TokenKind::Number(_) | TokenKind::LParen | TokenKind::ParamMarker(_)
                        ) || n.text.starts_with('@')
                    })
                {
                    offset = true;
                }
                if word(t, "for")
                    && sig
                        .get(i + 1)
                        .is_some_and(|n| word(n, "xml") || word(n, "json") || word(n, "browse"))
                {
                    return Err(InlineRefusal::TrailingClause);
                }
                if word(t, "option") && sig.get(i + 1).is_some_and(|n| n.kind == TokenKind::LParen)
                {
                    return Err(InlineRefusal::TrailingClause);
                }
                if word(t, "order") && sig.get(i + 1).is_some_and(|n| word(n, "by")) {
                    order_at = Some(t.span.start);
                }
                if word(t, "union") || word(t, "except") || word(t, "intersect") {
                    order_at = None;
                    set_operation = true;
                }
            }
            _ => {}
        }
    }
    let limited = offset || (top && !set_operation);
    Ok(match order_at {
        Some(at) if !limited => sql[..at].trim_end().to_string(),
        _ => sql.to_string(),
    })
}

/// The part of `cell`'s SQL the inlined CTEs go in front of: the one
/// statement (in its batch) that references other cells. Returns the text
/// before it, the statement, and the text after it. Leading semicolons
/// (`;WITH …`) are dropped when nothing else comes before the statement.
fn referencing_statement(
    doc: &NotebookDoc,
    cell: &Cell,
    db: DatabaseType,
    dialects: &Dialects,
) -> Result<(String, String, String), ExpandError> {
    let source = cell.source.as_str();
    let refs: Vec<usize> = cell_references(doc, cell, dialects)
        .into_iter()
        .filter(|r| matches!(r.target, RefTarget::Cell(_)))
        .map(|r| r.start)
        .collect();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for batch in split_batches(source, db.family()) {
        for statement in split_statements(batch.text, db.family()) {
            let start = batch.span.start + statement.span.start;
            spans.push((start, batch.span.start + statement.span.end));
        }
    }
    let holding: BTreeSet<usize> = refs
        .iter()
        .filter_map(|at| spans.iter().position(|(s, e)| *s <= *at && *at < *e))
        .collect();
    if holding.len() > 1 {
        return Err(ExpandError::ReferencesSpanStatements {
            cell: cell.id.clone(),
        });
    }
    let Some(&(start, end)) = holding.iter().next().map(|i| &spans[*i]) else {
        return Ok((String::new(), source.to_string(), String::new()));
    };
    let before = &source[..start];
    let only_terminators = StrictLexer::tokenize(before, db.family())
        .iter()
        .all(|t| t.is_trivia() || t.kind == TokenKind::Semicolon);
    let before = if only_terminators { "" } else { before };
    Ok((
        before.to_string(),
        source[start..end].to_string(),
        source[end..].to_string(),
    ))
}

fn nests_with(db: DatabaseType) -> bool {
    match db.family() {
        Dialect::SqlServer => false,
        Dialect::Oracle => db == DatabaseType::Oracle,
        Dialect::Postgres | Dialect::MySql | Dialect::Sqlite => true,
    }
}

fn identifiers(db: DatabaseType, sql: &str) -> BTreeSet<String> {
    StrictLexer::tokenize(sql, db.family())
        .iter()
        .filter(|t| t.is_word())
        .map(|t| t.ident_text().to_ascii_lowercase())
        .collect()
}

/// The CTE name for each inlined cell: its relation name, unless a query in
/// the expansion also reads a table or column by that name (a cell called
/// `orders` that selects `FROM orders` would otherwise reference itself), in
/// which case `<name>_cell`, `<name>_cell2`, … is used instead.
fn cte_names(
    doc: &NotebookDoc,
    inline: &[String],
    target: &Cell,
    db: DatabaseType,
    dialects: &Dialects,
) -> Result<BTreeMap<String, String>, ExpandError> {
    let mut taken = BTreeSet::new();
    for cell in inline
        .iter()
        .filter_map(|id| doc.cell(id))
        .chain(std::iter::once(target))
    {
        let bare = replace_refs(doc, cell, dialects, &|_| String::new())?;
        taken.extend(identifiers(db, &bare));
    }
    let bases: BTreeSet<String> = inline
        .iter()
        .filter_map(|id| doc.cell(id))
        .map(|c| c.relation_name().to_ascii_lowercase())
        .collect();
    let mut names = BTreeMap::new();
    for cell in inline.iter().filter_map(|id| doc.cell(id)) {
        let base = cell.relation_name();
        let mut name = base.to_string();
        let mut n = 1;
        while taken.contains(&name.to_ascii_lowercase())
            || (name != base && bases.contains(&name.to_ascii_lowercase()))
        {
            name = if n == 1 {
                format!("{base}_cell")
            } else {
                format!("{base}_cell{n}")
            };
            n += 1;
        }
        taken.insert(name.to_ascii_lowercase());
        names.insert(cell.id.clone(), name);
    }
    Ok(names)
}

fn check_inlinable(db: DatabaseType, cell: &str, body: &str) -> Result<(), ExpandError> {
    let refuse = |reason| ExpandError::NotInlinable {
        cell: cell.to_string(),
        reason,
    };
    let tokens = StrictLexer::tokenize(body, db.family());
    let Some(first) = tokens.iter().find(|t| !t.is_trivia()) else {
        return Err(refuse(InlineRefusal::Empty));
    };
    if !(word(first, "select") || word(first, "with") || first.kind == TokenKind::LParen) {
        return Err(refuse(InlineRefusal::NotAQuery));
    }
    if !classify_select_only_for(db, body).select_only {
        return Err(refuse(InlineRefusal::NotReadOnly));
    }
    let statements = split_statements(body, db.family()).len();
    let batches = split_batches(body, db.family()).len();
    if statements > 1 || batches > 1 {
        return Err(refuse(InlineRefusal::MultipleStatements));
    }
    Ok(())
}

/// Expand `cell_id`'s SQL, inlining every referenced SQL cell it can.
pub fn expand_sql(
    doc: &NotebookDoc,
    cell_id: &str,
    opts: ExpandOptions<'_>,
) -> Result<Expansion, ExpandError> {
    let target = doc.cell(cell_id).ok_or_else(|| ExpandError::UnknownCell {
        id: cell_id.to_string(),
    })?;
    if target.kind != CellKind::Sql {
        return Err(ExpandError::NotSql {
            cell: cell_id.to_string(),
        });
    }
    let db = opts.dialects.for_cell(doc, target);
    let mut walk = Walk {
        doc,
        opts,
        target_slot: doc.effective_connection_ref(target),
        target_local: target.exec_class() == ExecClass::Local,
        state: BTreeMap::new(),
        stack: Vec::new(),
        inline: Vec::new(),
        local: Vec::new(),
    };
    walk.visit(target)?;
    if walk.inline.is_empty() && walk.local.is_empty() {
        return Ok(Expansion::Sql(ExpandedSql {
            sql: target.source.clone(),
            inlined: Vec::new(),
        }));
    }
    if !walk.local.is_empty() {
        return Ok(Expansion::NeedsLocalMaterialization { cells: walk.local });
    }
    let names = cte_names(doc, &walk.inline, target, db, opts.dialects)?;
    let name_for = |c: &Cell| {
        relation_sql(
            db,
            names.get(&c.id).map_or(c.relation_name(), String::as_str),
        )
    };
    let hoist = !nests_with(db);

    // (CTE name, owning cell, text), in emission order.
    let mut ctes: Vec<(String, String, String)> = Vec::new();
    // (owning cell, names hoisted out of it).
    let mut hoisted: Vec<(String, BTreeSet<String>)> = Vec::new();
    let mut pieces: Vec<(String, String)> = Vec::new();
    // A hoisted upstream's `WITH RECURSIVE` keeps its keyword on the merged
    // list (Teradata, Informix, SAP HANA need it there).
    let mut upstream_recursive = false;
    for id in &walk.inline {
        let cell = doc.cell(id).expect("walked cells exist");
        let body = trim_tail(db, &replace_refs(doc, cell, opts.dialects, &name_for)?);
        check_inlinable(db, id, &body)?;
        let refuse = |reason| ExpandError::NotInlinable {
            cell: id.clone(),
            reason,
        };
        let name = name_for(cell);
        if hoist {
            let (inner, main) = match split_with(db, &body) {
                None => (Vec::new(), body.clone()),
                Some(Ok(parts)) => {
                    upstream_recursive |= parts.recursive;
                    (
                        parts
                            .items
                            .iter()
                            .map(|c| (c.name.clone(), body[c.start..c.end].to_string()))
                            .collect(),
                        body[parts.main_start..].to_string(),
                    )
                }
                Some(Err(())) => return Err(refuse(InlineRefusal::UnsupportedWith)),
            };
            let main = if db.family() == Dialect::SqlServer {
                tsql_inline_body(&main).map_err(refuse)?
            } else {
                main
            };
            hoisted.push((id.clone(), inner.iter().map(|(n, _)| n.clone()).collect()));
            for (inner_name, text) in inner {
                ctes.push((inner_name, id.clone(), text));
            }
            pieces.push((id.clone(), main.clone()));
            ctes.push((
                name.to_ascii_lowercase(),
                id.clone(),
                format!("{name} AS ({main})"),
            ));
        } else {
            pieces.push((id.clone(), body.clone()));
            ctes.push((
                name.to_ascii_lowercase(),
                id.clone(),
                format!("{name} AS ({body})"),
            ));
        }
    }

    let (before, statement, after) = referencing_statement(doc, target, db, opts.dialects)?;
    let mut statement_cell = target.clone();
    statement_cell.source = statement;
    let text = trim_tail(
        db,
        &replace_refs(doc, &statement_cell, opts.dialects, &name_for)?,
    );
    let tail = if StrictLexer::tokenize(&after, db.family())
        .iter()
        .all(|t| t.is_trivia() || t.kind == TokenKind::Semicolon)
    {
        String::new()
    } else {
        after
    };
    let (recursive, main) = match split_with(db, &text) {
        None => (false, text.clone()),
        Some(Ok(parts)) => {
            for c in &parts.items {
                ctes.push((
                    c.name.clone(),
                    cell_id.to_string(),
                    text[c.start..c.end].to_string(),
                ));
            }
            (parts.recursive, text[parts.main_start..].to_string())
        }
        Some(Err(())) => {
            return Err(ExpandError::NotInlinable {
                cell: cell_id.to_string(),
                reason: InlineRefusal::UnsupportedWith,
            });
        }
    };
    pieces.push((cell_id.to_string(), text.clone()));

    let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (name, owner, _) in &ctes {
        let list = owners.entry(name.as_str()).or_default();
        if !list.contains(&owner.as_str()) {
            list.push(owner);
        }
    }
    for (name, list) in &owners {
        let count = ctes.iter().filter(|(n, _, _)| n == name).count();
        if count > 1 {
            return Err(ExpandError::CteNameConflict {
                name: name.to_string(),
                cells: list.iter().map(|s| s.to_string()).collect(),
            });
        }
    }
    for (owner, names) in &hoisted {
        for (other, text) in pieces.iter().filter(|(o, _)| o != owner) {
            if let Some(name) = identifiers(db, text).intersection(names).next() {
                return Err(ExpandError::CteNameConflict {
                    name: name.clone(),
                    cells: vec![owner.clone(), other.clone()],
                });
            }
        }
    }

    let keyword = if recursive || upstream_recursive {
        "WITH RECURSIVE"
    } else {
        "WITH"
    };
    let list: Vec<&str> = ctes.iter().map(|(_, _, t)| t.as_str()).collect();
    Ok(Expansion::Sql(ExpandedSql {
        sql: format!("{before}{keyword} {} {main}{tail}", list.join(", ")),
        inlined: walk.inline,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::document::ConnectionRef;

    fn doc(cells: &[(&str, &str)]) -> NotebookDoc {
        let mut doc = NotebookDoc::empty("n", "t");
        for (id, src) in cells {
            let mut cell = Cell::new(*id, CellKind::Sql, Default::default());
            cell.source = src.to_string();
            doc.cells.push(cell);
        }
        doc
    }

    fn expand_on(d: &NotebookDoc, id: &str, db: DatabaseType) -> Result<Expansion, ExpandError> {
        let dialects = Dialects::single(db);
        let materialized = BTreeSet::new();
        expand_sql(
            d,
            id,
            ExpandOptions {
                dialects: &dialects,
                materialized: &materialized,
            },
        )
    }

    fn sql(d: &NotebookDoc, id: &str, db: DatabaseType) -> String {
        match expand_on(d, id, db).unwrap() {
            Expansion::Sql(e) => e.sql,
            other => panic!("expected SQL, got {other:?}"),
        }
    }

    const PG: DatabaseType = DatabaseType::Postgres;
    const SQLITE: DatabaseType = DatabaseType::Sqlite;
    const MS: DatabaseType = DatabaseType::SqlServer;

    #[test]
    fn no_references_is_identity() {
        let d = doc(&[("cell_1", "SELECT 1 AS x;")]);
        assert_eq!(sql(&d, "cell_1", MS), "SELECT 1 AS x;");
    }

    #[test]
    fn single_and_transitive_references() {
        let d = doc(&[
            ("cell_1", "SELECT 1 AS x"),
            ("cell_2", "SELECT x FROM {{cell_1}}"),
            ("cell_3", "SELECT x FROM {{cell_2}};"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", MS),
            "WITH cell_1 AS (SELECT 1 AS x) SELECT x FROM cell_1"
        );
        assert_eq!(
            sql(&d, "cell_3", PG),
            "WITH cell_1 AS (SELECT 1 AS x), cell_2 AS (SELECT x FROM cell_1) SELECT x FROM cell_2"
        );
        match expand_on(&d, "cell_3", PG).unwrap() {
            Expansion::Sql(e) => assert_eq!(e.inlined, vec!["cell_1", "cell_2"]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn names_are_used_as_relations() {
        let mut d = doc(&[
            ("c_a", "SELECT 1 AS x"),
            ("c_b", "SELECT * FROM {{orders}} o JOIN {{c_a}} a ON 1=1"),
        ]);
        d.cells[0].name = Some("orders".into());
        assert_eq!(
            sql(&d, "c_b", PG),
            "WITH orders AS (SELECT 1 AS x) SELECT * FROM orders o JOIN orders a ON 1=1"
        );
    }

    #[test]
    fn a_cell_named_like_the_table_it_reads_gets_a_distinct_cte_name() {
        let mut d = doc(&[
            ("c_a", "SELECT id FROM orders WHERE total > 10"),
            ("c_b", "SELECT count(*) FROM {{orders}}"),
        ]);
        d.cells[0].name = Some("orders".into());
        let out = sql(&d, "c_b", SQLITE);
        assert_eq!(
            out,
            "WITH orders_cell AS (SELECT id FROM orders WHERE total > 10) SELECT count(*) FROM orders_cell"
        );
    }

    #[test]
    fn suffixed_cte_names_skip_other_cells_names() {
        let mut d = doc(&[
            ("c_a", "SELECT 1 AS orders"),
            ("c_x", "SELECT 2 AS y"),
            (
                "c_b",
                "SELECT * FROM {{orders}} JOIN {{orders_cell}} ON 1=1",
            ),
        ]);
        d.cells[0].name = Some("orders".into());
        d.cells[1].name = Some("orders_cell".into());
        let out = sql(&d, "c_b", PG);
        assert!(
            out.contains("orders_cell2 AS (SELECT 1 AS orders)"),
            "{out}"
        );
        assert!(out.contains("orders_cell AS (SELECT 2 AS y)"), "{out}");
    }

    #[test]
    fn nested_with_is_kept_inside_on_engines_that_allow_it() {
        let d = doc(&[
            ("cell_1", "WITH a AS (SELECT 1 AS x) SELECT x FROM a"),
            (
                "cell_2",
                "WITH a AS (SELECT 2 AS x) SELECT * FROM {{cell_1}} JOIN a ON true",
            ),
        ]);
        assert_eq!(
            sql(&d, "cell_2", PG),
            "WITH cell_1 AS (WITH a AS (SELECT 1 AS x) SELECT x FROM a), a AS (SELECT 2 AS x) SELECT * FROM cell_1 JOIN a ON true"
        );
    }

    #[test]
    fn sql_server_hoists_upstream_ctes() {
        let d = doc(&[
            ("cell_1", "WITH a AS (SELECT 1 AS x) SELECT x FROM a"),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", MS),
            "WITH a AS (SELECT 1 AS x), cell_1 AS (SELECT x FROM a) SELECT * FROM cell_1"
        );
        let d = doc(&[
            (
                "cell_1",
                "WITH [my cte] (n) AS (SELECT 1), b AS (SELECT 2 AS m) SELECT n FROM [my cte] CROSS JOIN b",
            ),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", MS),
            "WITH [my cte] (n) AS (SELECT 1), b AS (SELECT 2 AS m), cell_1 AS (SELECT n FROM [my cte] CROSS JOIN b) SELECT * FROM cell_1"
        );
    }

    #[test]
    fn sql_server_refuses_colliding_or_shadowing_hoists() {
        let d = doc(&[
            ("cell_1", "WITH a AS (SELECT 1 AS x) SELECT x FROM a"),
            ("cell_2", "WITH a AS (SELECT 2 AS x) SELECT x FROM a"),
            ("cell_3", "SELECT * FROM {{cell_1}} JOIN {{cell_2}} ON 1=1"),
        ]);
        assert!(matches!(
            expand_on(&d, "cell_3", MS),
            Err(ExpandError::CteNameConflict { ref name, .. }) if name == "a"
        ));
        // Postgres nests, so the same notebook works there.
        assert!(matches!(expand_on(&d, "cell_3", PG), Ok(Expansion::Sql(_))));
        let shadow = doc(&[
            (
                "cell_1",
                "WITH orders AS (SELECT 1 AS x) SELECT x FROM orders",
            ),
            ("cell_2", "SELECT * FROM {{cell_1}} JOIN orders ON 1=1"),
        ]);
        assert!(matches!(
            expand_on(&shadow, "cell_2", MS),
            Err(ExpandError::CteNameConflict { ref name, .. }) if name == "orders"
        ));
    }

    #[test]
    fn sql_server_drops_a_meaningless_trailing_order_by() {
        let d = doc(&[
            ("cell_1", "SELECT x FROM t ORDER BY x -- sorted\n;"),
            ("cell_2", "SELECT TOP 5 x FROM t ORDER BY x"),
            (
                "cell_3",
                "SELECT * FROM {{cell_1}} JOIN {{cell_2}} b ON 1=1",
            ),
            ("cell_4", "SELECT x FROM t FOR JSON PATH"),
            ("cell_5", "SELECT * FROM {{cell_4}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_3", MS),
            "WITH cell_1 AS (SELECT x FROM t), cell_2 AS (SELECT TOP 5 x FROM t ORDER BY x) SELECT * FROM cell_1 JOIN cell_2 b ON 1=1"
        );
        assert_eq!(
            expand_on(&d, "cell_5", MS),
            Err(ExpandError::NotInlinable {
                cell: "cell_4".into(),
                reason: InlineRefusal::TrailingClause
            })
        );
        assert!(
            sql(&d, "cell_3", PG).contains("ORDER BY x), cell_2"),
            "{}",
            sql(&d, "cell_3", PG)
        );
    }

    #[test]
    fn the_ctes_go_on_the_statement_that_references_cells() {
        let up = ("cell_1", "SELECT 1 AS x");
        let with = "WITH cell_1 AS (SELECT 1 AS x) ";
        for (db, target, expected) in [
            (
                MS,
                "SET NOCOUNT ON; SELECT * FROM {{cell_1}}",
                format!("SET NOCOUNT ON; {with}SELECT * FROM cell_1"),
            ),
            (
                MS,
                "DECLARE @n int = 5;\nSELECT TOP (@n) * FROM {{cell_1}};",
                format!("DECLARE @n int = 5;\n{with}SELECT TOP (@n) * FROM cell_1"),
            ),
            (
                MS,
                "SET NOCOUNT ON\nGO\nSELECT * FROM {{cell_1}}",
                format!("SET NOCOUNT ON\nGO\n{with}SELECT * FROM cell_1"),
            ),
            (
                MS,
                ";WITH y AS (SELECT * FROM {{cell_1}}) SELECT * FROM y",
                "WITH cell_1 AS (SELECT 1 AS x), y AS (SELECT * FROM cell_1) SELECT * FROM y"
                    .to_string(),
            ),
            (
                PG,
                "SET search_path TO app;\nSELECT * FROM {{cell_1}};",
                format!("SET search_path TO app;\n{with}SELECT * FROM cell_1"),
            ),
            (
                PG,
                "SELECT * FROM {{cell_1}}; SELECT 2;",
                format!("{with}SELECT * FROM cell_1; SELECT 2;"),
            ),
        ] {
            let d = doc(&[up, ("cell_2", target)]);
            assert_eq!(sql(&d, "cell_2", db), expected, "{db:?}: {target}");
        }
        let d = doc(&[
            up,
            (
                "cell_2",
                "SELECT * FROM {{cell_1}}; SELECT x FROM {{cell_1}}",
            ),
        ]);
        assert_eq!(
            expand_on(&d, "cell_2", MS),
            Err(ExpandError::ReferencesSpanStatements {
                cell: "cell_2".into()
            })
        );
    }

    /// Runs `sql` statement by statement on an in-memory SQLite and returns
    /// the last statement's rows.
    fn run_on_sqlite(sql: &str) -> Vec<Vec<i64>> {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let statements = split_statements(sql, Dialect::Sqlite);
        let (last, rest) = statements.split_last().unwrap();
        for statement in rest {
            conn.execute_batch(statement.text).unwrap();
        }
        let mut query = conn.prepare(last.text).unwrap();
        let width = query.column_count();
        query
            .query_map([], |row| (0..width).map(|i| row.get::<_, i64>(i)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn expanded_multi_statement_and_recursive_cells_run_on_sqlite() {
        let d = doc(&[
            ("cell_1", "SELECT 1 AS x UNION ALL SELECT 2"),
            (
                "cell_2",
                "CREATE TEMP TABLE t (y INTEGER);\nINSERT INTO t VALUES (10);\nSELECT x, (SELECT y FROM t) AS y FROM {{cell_1}} ORDER BY x;",
            ),
            (
                "cell_3",
                "WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 3) SELECT i FROM n",
            ),
            ("cell_4", "SELECT sum(i) FROM {{cell_3}}"),
        ]);
        assert_eq!(
            run_on_sqlite(&sql(&d, "cell_2", SQLITE)),
            vec![vec![1, 10], vec![2, 10]]
        );
        assert_eq!(run_on_sqlite(&sql(&d, "cell_4", SQLITE)), vec![vec![6]]);
    }

    #[test]
    fn sql_server_bodies_refuse_query_hints_and_read_order_by_around_unions() {
        let refused = doc(&[
            ("cell_1", "SELECT x FROM t OPTION (RECOMPILE)"),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        assert_eq!(
            expand_on(&refused, "cell_2", MS),
            Err(ExpandError::NotInlinable {
                cell: "cell_1".into(),
                reason: InlineRefusal::TrailingClause
            })
        );
        for (upstream, body) in [
            (
                "SELECT TOP 5 x FROM t UNION SELECT y FROM u ORDER BY 1",
                "SELECT TOP 5 x FROM t UNION SELECT y FROM u",
            ),
            (
                "SELECT x, offset FROM t ORDER BY offset",
                "SELECT x, offset FROM t",
            ),
            (
                "SELECT x FROM t ORDER BY x OFFSET 10 ROWS",
                "SELECT x FROM t ORDER BY x OFFSET 10 ROWS",
            ),
            (
                "SELECT TOP (3) x FROM t ORDER BY x",
                "SELECT TOP (3) x FROM t ORDER BY x",
            ),
        ] {
            let d = doc(&[("cell_1", upstream), ("cell_2", "SELECT * FROM {{cell_1}}")]);
            assert_eq!(
                sql(&d, "cell_2", MS),
                format!("WITH cell_1 AS ({body}) SELECT * FROM cell_1"),
                "{upstream}"
            );
        }
    }

    #[test]
    fn hoisted_recursive_upstreams_keep_the_keyword() {
        let d = doc(&[
            (
                "cell_1",
                "WITH RECURSIVE n (i) AS (SELECT 1 FROM t UNION ALL SELECT i + 1 FROM n WHERE i < 3) SELECT i FROM n",
            ),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        for db in [
            DatabaseType::Teradata,
            DatabaseType::Informix,
            DatabaseType::SapHana,
        ] {
            let out = sql(&d, "cell_2", db);
            assert!(
                out.starts_with("WITH RECURSIVE n (i) AS ("),
                "{db:?}: {out}"
            );
            assert!(out.ends_with("cell_1 AS (SELECT i FROM n) SELECT * FROM cell_1"));
        }
    }

    #[test]
    fn trailing_comments_never_swallow_the_closing_paren() {
        let d = doc(&[
            ("cell_1", "SELECT 1 AS x -- note"),
            ("cell_2", "SELECT * FROM {{cell_1}} /* end */"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", PG),
            "WITH cell_1 AS (SELECT 1 AS x) SELECT * FROM cell_1"
        );
    }

    #[test]
    fn recursive_target_keeps_its_keyword() {
        let d = doc(&[
            ("cell_1", "SELECT 1 AS n"),
            (
                "cell_2",
                "WITH RECURSIVE r(n) AS (SELECT n FROM {{cell_1}} UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r",
            ),
        ]);
        assert_eq!(
            sql(&d, "cell_2", PG),
            "WITH RECURSIVE cell_1 AS (SELECT 1 AS n), r(n) AS (SELECT n FROM cell_1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT * FROM r"
        );
    }

    #[test]
    fn references_inside_literals_are_not_expanded() {
        let d = doc(&[
            ("cell_1", "SELECT 1 AS x"),
            ("cell_2", "SELECT '{{cell_1}}' AS s -- {{cell_1}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", MS),
            "SELECT '{{cell_1}}' AS s -- {{cell_1}}"
        );
    }

    #[test]
    fn refuses_writes_batches_and_non_queries() {
        let refusal = |src: &str, db| {
            let d = doc(&[("cell_1", src), ("cell_2", "SELECT * FROM {{cell_1}}")]);
            match expand_on(&d, "cell_2", db) {
                Err(ExpandError::NotInlinable { reason, .. }) => reason,
                other => panic!("{src}: {other:?}"),
            }
        };
        assert_eq!(refusal("DELETE FROM t", MS), InlineRefusal::NotAQuery);
        assert_eq!(
            refusal("SELECT 1; SELECT 2", MS),
            InlineRefusal::MultipleStatements
        );
        assert_eq!(
            refusal("SELECT 1\nSELECT 2", MS),
            InlineRefusal::MultipleStatements
        );
        assert_eq!(
            refusal("SELECT 1\nGO\nSELECT 2", MS),
            InlineRefusal::MultipleStatements
        );
        assert_eq!(refusal("  -- nothing\n", MS), InlineRefusal::Empty);
        assert_eq!(
            refusal("SELECT * INTO #t FROM x", MS),
            InlineRefusal::NotReadOnly
        );
        assert_eq!(
            refusal("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", PG),
            InlineRefusal::NotReadOnly
        );
    }

    #[test]
    fn reference_errors() {
        let d = doc(&[
            ("cell_1", "SELECT * FROM {{ghost}}"),
            ("cell_2", "SELECT * FROM {{cell_2}}"),
        ]);
        assert_eq!(
            expand_on(&d, "cell_1", MS),
            Err(ExpandError::UnknownReference {
                cell: "cell_1".into(),
                token: "ghost".into()
            })
        );
        assert_eq!(
            expand_on(&d, "cell_2", MS),
            Err(ExpandError::Cycle {
                path: vec!["cell_2".into(), "cell_2".into()]
            })
        );
        let d = doc(&[
            ("cell_1", "SELECT * FROM {{cell_2}}"),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        assert_eq!(
            expand_on(&d, "cell_1", MS),
            Err(ExpandError::Cycle {
                path: vec!["cell_1".into(), "cell_2".into(), "cell_1".into()]
            })
        );
        let mut d = doc(&[
            ("cell_1", "# notes"),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        d.cells[0].kind = CellKind::Markdown;
        assert!(matches!(
            expand_on(&d, "cell_2", MS),
            Err(ExpandError::NotARelation { .. })
        ));
        assert!(matches!(
            expand_on(&d, "cell_1", MS),
            Err(ExpandError::NotSql { .. })
        ));
        assert!(matches!(
            expand_on(&d, "zz", MS),
            Err(ExpandError::UnknownCell { .. })
        ));
        assert_eq!(
            ExpandError::Cycle {
                path: vec!["a".into(), "a".into()]
            }
            .message()
            .key(),
            Some("notebook.core.cycle.loop")
        );
    }

    #[test]
    fn cross_connection_or_local_upstreams_need_materialization() {
        let mut d = doc(&[
            ("cell_1", "SELECT 1 AS x"),
            ("cell_2", "SELECT 2 AS y"),
            ("cell_3", "SELECT * FROM {{cell_1}} JOIN {{cell_2}} ON 1=1"),
        ]);
        d.connection_refs.push(ConnectionRef {
            id: "a".into(),
            profile_id: Some("p".into()),
            label: "A".into(),
            database: None,
            extra: Default::default(),
        });
        d.cells[1].connection_ref = Some("b".into());
        assert_eq!(
            expand_on(&d, "cell_3", MS),
            Ok(Expansion::NeedsLocalMaterialization {
                cells: vec!["cell_2".into()]
            })
        );
        d.cells[1].connection_ref = None;
        let dialects = Dialects::single(MS);
        let materialized = BTreeSet::from(["cell_1".to_string()]);
        assert_eq!(
            expand_sql(
                &d,
                "cell_3",
                ExpandOptions {
                    dialects: &dialects,
                    materialized: &materialized
                }
            ),
            Ok(Expansion::NeedsLocalMaterialization {
                cells: vec!["cell_1".into()]
            })
        );
        d.cells[0].kind = CellKind::Transform;
        assert_eq!(
            expand_on(&d, "cell_3", MS),
            Ok(Expansion::NeedsLocalMaterialization {
                cells: vec!["cell_1".into()]
            })
        );
        let relations =
            substitute_relations(&d, "cell_3", &dialects, &|c| format!("\"nb_{}\"", c.id)).unwrap();
        assert_eq!(
            relations,
            "SELECT * FROM \"nb_cell_1\" JOIN \"nb_cell_2\" ON 1=1"
        );
    }

    #[test]
    fn bracket_and_quoted_identifiers_survive() {
        let d = doc(&[
            ("cell_1", "SELECT [a b] AS [x)y] FROM [dbo].[t (1)]"),
            ("cell_2", "SELECT [x)y] FROM {{cell_1}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", MS),
            "WITH cell_1 AS (SELECT [a b] AS [x)y] FROM [dbo].[t (1)]) SELECT [x)y] FROM cell_1"
        );
        let d = doc(&[
            ("cell_1", "SELECT \"a)\" FROM \"t(\""),
            ("cell_2", "SELECT * FROM {{cell_1}}"),
        ]);
        assert_eq!(
            sql(&d, "cell_2", PG),
            "WITH cell_1 AS (SELECT \"a)\" FROM \"t(\") SELECT * FROM cell_1"
        );
    }

    /// Fixed-seed random chains: the expanded SQL always has balanced
    /// parentheses, names each inlined cell once, and parses as a single
    /// SELECT-only statement.
    #[test]
    fn randomized_chains_expand_to_one_read_statement() {
        use crate::notebook::document::{IdSource, SeededIds};
        let mut ids = SeededIds(99);
        let mut next = || {
            let mut b = [0u8; 1];
            ids.fill(&mut b);
            b[0] as usize
        };
        for db in [MS, PG, DatabaseType::MySql, DatabaseType::Sqlite] {
            for _ in 0..60 {
                let n = 1 + next() % 6;
                let mut cells: Vec<(String, String)> = Vec::new();
                for i in 0..n {
                    let refs: Vec<String> = (0..i)
                        .filter(|_| next() % 2 == 0)
                        .map(|j| format!("{{{{cell_{j}}}}}"))
                        .collect();
                    let from = if refs.is_empty() {
                        "t".to_string()
                    } else {
                        refs.join(" CROSS JOIN ")
                    };
                    let tail = match next() % 4 {
                        0 => " -- trailing",
                        1 => ";",
                        2 => " WHERE 'x' = 'y)'",
                        _ => "",
                    };
                    cells.push((
                        format!("cell_{i}"),
                        format!("SELECT {i} AS c{i} FROM {from}{tail}"),
                    ));
                }
                let refs: Vec<(&str, &str)> = cells
                    .iter()
                    .map(|(a, b)| (a.as_str(), b.as_str()))
                    .collect();
                let d = doc(&refs);
                let last = format!("cell_{}", n - 1);
                let out = sql(&d, &last, db);
                let masked = crate::notebook::scan::mask_sql(db, &out);
                assert_eq!(
                    masked.matches('(').count(),
                    masked.matches(')').count(),
                    "{out}"
                );
                assert_eq!(split_statements(&out, db.family()).len(), 1, "{out}");
                assert!(classify_select_only_for(db, &out).select_only, "{out}");
                for (id, _) in &cells[..n - 1] {
                    assert!(out.matches(&format!("{id} AS (")).count() <= 1, "{out}");
                }
            }
        }
    }
}
