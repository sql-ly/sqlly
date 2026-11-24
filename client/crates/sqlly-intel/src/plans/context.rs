//! What the viewer knows about a plan beyond the plan document itself: the
//! table definitions and the text of the statement the plan belongs to.
//!
//! The statement text matters most for MySQL, MariaDB and SQLite. Their
//! plans name each table by the alias the query gave it (`o`, `c`), so
//! without the query the story reads "scans o" and index advice has to say
//! `CREATE INDEX ... ON o`. [`resolve_plan_aliases`] replaces those aliases
//! with the real table names, in the plan itself, so the analysis, the
//! story, the findings and the diagram all agree.
//!
//! Formats that already print the real name are left alone: PostgreSQL
//! (relation and alias), SQL Server, Oracle, MySQL JSON format 2
//! (`table_name` and `alias`), SQLite before 3.36 (`SCAN TABLE orders AS o`).

use std::collections::{HashMap, HashSet};

use sqlly_lang::sqllex::{StrictLexer, Token, TokenKind};
use sqlly_lang::sqlmodel::Dialect;

use crate::plans::analysis::{PlanAnalysis, PlanObjectKind, analyze_plan, resolve_dialect};
use crate::plans::dialects::PlanDialect;
use crate::plans::model::ExecutionPlan;
use crate::plans::schema::{SchemaContext, SchemaTable, bare_name};

/// Optional knowledge about the statement a plan belongs to. Every field is
/// optional; an empty context changes nothing.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlanContext<'a> {
    /// Table definitions (pasted DDL or catalog metadata).
    pub schema: Option<&'a SchemaContext>,
    /// SQL text of the statement, or of the whole batch, the plan was
    /// captured for.
    pub query_text: Option<&'a str>,
}

impl<'a> PlanContext<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_schema(mut self, schema: Option<&'a SchemaContext>) -> Self {
        self.schema = schema;
        self
    }

    pub fn with_query_text(mut self, query_text: Option<&'a str>) -> Self {
        self.query_text = query_text.filter(|text| !text.trim().is_empty());
        self
    }

    fn usable_schema(&self) -> Option<&'a SchemaContext> {
        self.schema.filter(|schema| !schema.is_empty())
    }
}

/// How one table name in a plan was settled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedAlias {
    pub node_id: i32,
    /// The name the plan printed.
    pub printed: String,
    /// The table's real name.
    pub table: String,
    /// Schema or database qualifier the statement wrote, when it wrote one.
    pub schema: Option<String>,
    pub source: AliasSource,
}

/// Where a resolved table name came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AliasSource {
    /// The statement text declares the alias (`FROM orders o`) or uses the
    /// table without one.
    QueryText,
    /// No statement text; exactly one table in the schema has the index and
    /// the columns the plan uses through this name.
    Schema,
}

/// True when plans in `dialect` print query aliases in place of table names
/// (MySQL, MariaDB, SQLite), so a [`PlanContext`] with the statement text
/// improves what the viewer can say.
pub fn dialect_prints_aliases(dialect: PlanDialect) -> bool {
    matches!(
        dialect,
        PlanDialect::MySql | PlanDialect::MariaDb | PlanDialect::Sqlite
    )
}

/// Replace query aliases in `plan` with real table names, using the
/// statement text when there is one (the context's `query_text`, else the
/// plan's own `statement_text`) and the schema otherwise.
///
/// A resolved node gets `relation` = the real table, `alias` = the name the
/// plan printed, and `relation_resolved` = true. A name is resolved only
/// when the evidence is unambiguous: an alias the statement declares for
/// two different tables, or a name several schema tables could match, is
/// left as printed. Plans from engines that print real names are returned
/// unchanged. Returns what was resolved, in plan order.
pub fn resolve_plan_aliases(
    plan: &mut ExecutionPlan,
    context: &PlanContext<'_>,
) -> Vec<ResolvedAlias> {
    let dialect = resolve_dialect(plan);
    if !dialect_prints_aliases(dialect) || plan.nodes.is_empty() {
        return Vec::new();
    }
    let text = context
        .query_text
        .filter(|text| !text.trim().is_empty())
        .or_else(|| Some(plan.statement_text.as_str()).filter(|text| !text.trim().is_empty()));
    let sources = text.map(|text| StatementSources::read(text, dialect));
    resolve_with(plan, dialect, sources.as_ref(), context.usable_schema())
}

/// [`resolve_plan_aliases`] for every statement of a batch. When
/// `query_text` holds as many statements as there are plans, each plan is
/// resolved against its own statement, so the same alias may mean different
/// tables in different statements.
pub fn resolve_batch_aliases(
    plans: &mut [ExecutionPlan],
    context: &PlanContext<'_>,
) -> Vec<Vec<ResolvedAlias>> {
    let Some(first) = plans.first() else {
        return Vec::new();
    };
    let dialect = resolve_dialect(first);
    let per_statement: Option<Vec<StatementSources>> = context
        .query_text
        .filter(|text| !text.trim().is_empty())
        .filter(|_| plans.len() > 1 && dialect_prints_aliases(dialect))
        .map(|text| StatementSources::read_each(text, dialect))
        .filter(|sources| sources.len() == plans.len());
    match per_statement {
        Some(sources) => plans
            .iter_mut()
            .zip(sources)
            .map(|(plan, sources)| {
                let dialect = resolve_dialect(plan);
                if !dialect_prints_aliases(dialect) {
                    return Vec::new();
                }
                resolve_with(plan, dialect, Some(&sources), context.usable_schema())
            })
            .collect(),
        None => plans
            .iter_mut()
            .map(|plan| resolve_plan_aliases(plan, context))
            .collect(),
    }
}

/// A copy of `plan` with its aliases resolved; see [`resolve_plan_aliases`].
pub fn plan_with_context(plan: &ExecutionPlan, context: &PlanContext<'_>) -> ExecutionPlan {
    let mut resolved = plan.clone();
    resolve_plan_aliases(&mut resolved, context);
    resolved
}

/// [`analyze_plan`] with aliases resolved first.
pub fn analyze_plan_with_context(plan: &ExecutionPlan, context: &PlanContext<'_>) -> PlanAnalysis {
    if needs_resolution(plan) {
        analyze_plan(&plan_with_context(plan, context))
    } else {
        analyze_plan(plan)
    }
}

/// True when `plan` could change under [`resolve_plan_aliases`]: its engine
/// prints aliases and at least one table name is not yet settled.
pub(crate) fn needs_resolution(plan: &ExecutionPlan) -> bool {
    dialect_prints_aliases(resolve_dialect(plan))
        && plan
            .nodes
            .iter()
            .any(|node| !node.relation_resolved && node.alias.is_none())
}

// ---------------------------------------------------------------------------
// Statement text
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SourceTable {
    name: String,
    schema: Option<String>,
}

/// The table sources of one or more statements: which table each alias
/// names, and which tables are used under their own name.
#[derive(Debug, Default)]
struct StatementSources {
    /// Lower-cased alias -> the distinct tables it names.
    aliases: HashMap<String, Vec<SourceTable>>,
    /// Lower-cased names of tables used without an alias.
    plain: HashMap<String, SourceTable>,
    /// Lower-cased CTE names: not tables, never resolved to.
    ctes: HashSet<String>,
    /// Lower-cased aliases of derived tables (`FROM (SELECT ...) d`).
    derived: HashSet<String>,
}

/// Scanner state for one parenthesis level of a statement.
#[derive(Debug, Default)]
struct Frame {
    /// A SELECT or DELETE was seen at this level, so `FROM` starts a table
    /// list here (and not inside `EXTRACT(YEAR FROM d)`).
    has_select: bool,
    /// Inside a FROM / JOIN / UPDATE table list.
    in_from: bool,
    /// The next name is a table source.
    expect_source: bool,
    /// Between `WITH` and the statement it belongs to.
    with_prelude: bool,
    /// A parenthesis opened where a table source was expected.
    derived_open: bool,
}

fn word_is(token: &Token<'_>, word: &str) -> bool {
    token.is_word() && !token.ident_quoted() && token.text.eq_ignore_ascii_case(word)
}

/// Words that can follow a table name and are not its alias.
const NOT_AN_ALIAS: &[&str] = &[
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "STRAIGHT_JOIN",
    "ON",
    "USING",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "WINDOW",
    "FOR",
    "PARTITION",
    "USE",
    "FORCE",
    "IGNORE",
    "INDEXED",
    "NOT",
    "VALUES",
    "SELECT",
    "OFFSET",
    "FETCH",
    "RETURNING",
    "AS",
    "OUTER",
    "LOCK",
    "TABLESAMPLE",
    "WITH",
    "DEFAULT",
    "LATERAL",
];

fn is_alias(token: &Token<'_>) -> bool {
    token.is_word()
        && (token.ident_quoted()
            || !NOT_AN_ALIAS
                .iter()
                .any(|word| token.text.eq_ignore_ascii_case(word)))
}

/// Index of the `)` that closes the `(` at `open`.
fn matching_close(tokens: &[Token<'_>], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, token) in tokens.iter().enumerate().skip(open) {
        match token.kind {
            TokenKind::LParen => depth += 1,
            TokenKind::RParen => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
}

fn lex_dialect(dialect: PlanDialect) -> Dialect {
    match dialect {
        PlanDialect::Sqlite => Dialect::Sqlite,
        _ => Dialect::MySql,
    }
}

impl StatementSources {
    /// Sources of every statement in `text`, merged.
    fn read(text: &str, dialect: PlanDialect) -> Self {
        let mut merged = Self::default();
        for sources in Self::read_each(text, dialect) {
            merged.ctes.extend(sources.ctes);
            for (alias, tables) in sources.aliases {
                let known = merged.aliases.entry(alias).or_default();
                for table in tables {
                    if !known.contains(&table) {
                        known.push(table);
                    }
                }
            }
            for (name, table) in sources.plain {
                merged.plain.entry(name).or_insert(table);
            }
            merged.derived.extend(sources.derived);
        }
        merged
    }

    /// Sources of each statement in `text`, in order.
    fn read_each(text: &str, dialect: PlanDialect) -> Vec<Self> {
        let text = strip_explain_prefix(text);
        let tokens: Vec<Token<'_>> = StrictLexer::tokenize(text, lex_dialect(dialect))
            .into_iter()
            .filter(|token| !token.is_trivia())
            .collect();
        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut start = 0usize;
        for (at, token) in tokens.iter().enumerate() {
            match token.kind {
                TokenKind::LParen => depth += 1,
                TokenKind::RParen => depth = depth.saturating_sub(1),
                TokenKind::Semicolon if depth == 0 => {
                    if at > start {
                        out.push(Self::scan(&tokens[start..at]));
                    }
                    start = at + 1;
                }
                _ => {}
            }
        }
        if start < tokens.len() {
            out.push(Self::scan(&tokens[start..]));
        }
        out
    }

    /// Table sources of one statement, subqueries included.
    fn scan(tokens: &[Token<'_>]) -> Self {
        let mut sources = Self::default();
        let mut frames = vec![Frame::default()];
        let mut at = 0usize;
        while at < tokens.len() {
            let token = &tokens[at];
            let top = frames.len() - 1;
            match token.kind {
                TokenKind::LParen => {
                    if frames[top].expect_source {
                        frames[top].expect_source = false;
                        frames[top].derived_open = true;
                    }
                    frames.push(Frame::default());
                    at += 1;
                    continue;
                }
                TokenKind::RParen => {
                    if frames.len() > 1 {
                        frames.pop();
                    }
                    at += 1;
                    let top = frames.len() - 1;
                    if frames[top].derived_open {
                        frames[top].derived_open = false;
                        // `FROM (SELECT ...) [AS] d`: the alias names a
                        // derived table, not a real one.
                        if tokens.get(at).is_some_and(|token| word_is(token, "AS")) {
                            at += 1;
                        }
                        if let Some(alias) = tokens.get(at).filter(|token| is_alias(token)) {
                            sources
                                .derived
                                .insert(alias.ident_text().to_ascii_lowercase());
                            at += 1;
                        }
                    }
                    continue;
                }
                TokenKind::Comma => {
                    if frames[top].in_from {
                        frames[top].expect_source = true;
                    }
                    at += 1;
                    continue;
                }
                _ => {}
            }
            if !token.is_word() {
                frames[top].expect_source = false;
                at += 1;
                continue;
            }
            let frame = &mut frames[top];
            if !token.ident_quoted() {
                let upper = token.text.to_ascii_uppercase();
                let mut handled = true;
                match upper.as_str() {
                    "WITH" if !frame.has_select && !frame.in_from => frame.with_prelude = true,
                    "SELECT" => {
                        frame.has_select = true;
                        frame.with_prelude = false;
                        frame.in_from = false;
                        frame.expect_source = false;
                    }
                    "DELETE" => {
                        frame.has_select = true;
                        frame.with_prelude = false;
                    }
                    "UPDATE" if !frame.in_from && !frame.has_select => {
                        frame.with_prelude = false;
                        frame.in_from = true;
                        frame.expect_source = true;
                    }
                    "INSERT" | "REPLACE" if !frame.has_select => frame.with_prelude = false,
                    "INTO" => frame.expect_source = true,
                    "FROM" if frame.has_select => {
                        frame.in_from = true;
                        frame.expect_source = true;
                    }
                    "JOIN" | "STRAIGHT_JOIN" if frame.in_from => frame.expect_source = true,
                    "LATERAL" | "ONLY" | "LOW_PRIORITY" | "QUICK" | "IGNORE"
                        if frame.expect_source => {}
                    "WHERE" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION" | "EXCEPT"
                    | "INTERSECT" | "SET" | "WINDOW" | "RETURNING" | "VALUES" => {
                        frame.in_from = false;
                        frame.expect_source = false;
                    }
                    _ => handled = false,
                }
                if handled {
                    at += 1;
                    continue;
                }
            }
            if frame.with_prelude {
                // `name AS (` or `name (columns) AS (` defines a CTE.
                let mut next = at + 1;
                if tokens
                    .get(next)
                    .is_some_and(|t| t.kind == TokenKind::LParen)
                {
                    next = matching_close(tokens, next).map_or(tokens.len(), |close| close + 1);
                }
                if tokens.get(next).is_some_and(|token| word_is(token, "AS")) {
                    sources.ctes.insert(token.ident_text().to_ascii_lowercase());
                    at = next + 1;
                    continue;
                }
            }
            if !frame.expect_source {
                at += 1;
                continue;
            }
            frame.expect_source = false;
            // `name`, `schema.name`, `catalog.schema.name`.
            let mut parts = vec![token.ident_text().into_owned()];
            at += 1;
            while tokens.get(at).is_some_and(|t| t.kind == TokenKind::Dot)
                && tokens.get(at + 1).is_some_and(|t| t.is_word())
            {
                parts.push(tokens[at + 1].ident_text().into_owned());
                at += 2;
            }
            if tokens.get(at).is_some_and(|t| t.kind == TokenKind::LParen) {
                // A table function: `json_each(...)`, `generate_series(...)`.
                // Its alias, if any, names no table.
                continue;
            }
            let name = parts.pop().unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let table = SourceTable {
                name,
                schema: parts.pop().filter(|schema| !schema.is_empty()),
            };
            let had_as = tokens.get(at).is_some_and(|token| word_is(token, "AS"));
            if had_as {
                at += 1;
            }
            let alias = tokens
                .get(at)
                .filter(|token| is_alias(token))
                .map(|token| token.ident_text().into_owned());
            if alias.is_some() {
                at += 1;
            }
            match alias.filter(|alias| !alias.eq_ignore_ascii_case(&table.name)) {
                Some(alias) => {
                    let known = sources
                        .aliases
                        .entry(alias.to_ascii_lowercase())
                        .or_default();
                    if !known.contains(&table) {
                        known.push(table);
                    }
                }
                None => {
                    let key = table.name.to_ascii_lowercase();
                    match sources.plain.get(&key) {
                        Some(known) if known.schema != table.schema && table.schema.is_some() => {
                            // The same name in two schemas: keep the first.
                        }
                        Some(_) => {}
                        None => {
                            sources.plain.insert(key, table);
                        }
                    }
                }
            }
        }
        sources
    }

    /// The real table behind `printed`, when the statement settles it.
    fn resolve(&self, printed: &str) -> Option<SourceTable> {
        let key = printed.to_ascii_lowercase();
        if let Some(tables) = self.aliases.get(&key) {
            // An alias declared for two tables (in two subqueries, say), or
            // one that is also a table used under its own name, is ambiguous.
            return match tables.as_slice() {
                [table]
                    if !self.plain.contains_key(&key)
                        && !self.derived.contains(&key)
                        && !self.ctes.contains(&table.name.to_ascii_lowercase()) =>
                {
                    Some(table.clone())
                }
                _ => None,
            };
        }
        if self.ctes.contains(&key) || self.derived.contains(&key) {
            return None;
        }
        self.plain.get(&key).cloned()
    }
}

/// `EXPLAIN ANALYZE SELECT ...` -> `SELECT ...`, so a pasted capture command
/// parses as the statement it explains.
fn strip_explain_prefix(text: &str) -> &str {
    let trimmed = text.trim_start();
    let Some(head) = trimmed.get(..7) else {
        return text;
    };
    if !head.eq_ignore_ascii_case("explain") {
        return text;
    }
    let mut rest = &trimmed[7..];
    if !rest.starts_with(|ch: char| ch.is_whitespace()) {
        return text;
    }
    loop {
        rest = rest.trim_start();
        let word_end = rest
            .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '='))
            .unwrap_or(rest.len());
        let word = &rest[..word_end];
        let lowered = word.to_ascii_lowercase();
        let is_option = matches!(
            lowered.as_str(),
            "analyze" | "query" | "plan" | "extended" | "partitions" | "format"
        ) || lowered.starts_with("format=");
        if !is_option {
            return rest;
        }
        rest = &rest[word_end..];
        if lowered == "format" {
            // `FORMAT = JSON` / `FORMAT JSON`.
            rest = rest.trim_start();
            rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
            let value_end = rest
                .find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                .unwrap_or(rest.len());
            rest = &rest[value_end..];
        }
    }
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

fn resolve_with(
    plan: &mut ExecutionPlan,
    dialect: PlanDialect,
    sources: Option<&StatementSources>,
    schema: Option<&SchemaContext>,
) -> Vec<ResolvedAlias> {
    if sources.is_none() && schema.is_none() {
        return Vec::new();
    }
    let analysis = crate::plans::analysis::analyze_plan_as(plan, dialect);
    let mut resolved = Vec::new();
    for (position, facts) in analysis.nodes.iter().enumerate() {
        let Some(node) = plan.nodes.get(position) else {
            break;
        };
        if node.relation_resolved || node.alias.is_some() {
            continue;
        }
        let Some(object) = &facts.object else {
            continue;
        };
        if object.kind != PlanObjectKind::Table || object.alias.is_some() {
            continue;
        }
        let printed = bare_name(&object.name);
        if printed.is_empty() || printed.starts_with('<') {
            continue;
        }
        let from_query = sources.and_then(|sources| sources.resolve(&printed));
        let (table, source) = match from_query {
            Some(table) => (table, AliasSource::QueryText),
            None => {
                // The statement, when there is one, has the last word: a
                // name it does not mention is not guessed from the schema.
                if sources.is_some_and(|sources| {
                    sources.aliases.contains_key(&printed.to_ascii_lowercase())
                }) {
                    continue;
                }
                let Some(schema) = schema else {
                    continue;
                };
                if schema.table(object.schema.as_deref(), &printed).is_some() {
                    // The printed name is a table in the schema already.
                    continue;
                }
                let predicates = [
                    node.index_cond.as_deref(),
                    node.filter.as_deref(),
                    node.join_predicate.as_deref(),
                    facts.seek_predicate.as_ref().map(|p| p.raw.as_str()),
                    facts.filter.as_ref().map(|p| p.raw.as_str()),
                ];
                let mut columns: Vec<String> = Vec::new();
                for predicate in predicates.into_iter().flatten() {
                    for column in qualified_columns(predicate, &printed) {
                        if !columns
                            .iter()
                            .any(|known| known.eq_ignore_ascii_case(&column))
                        {
                            columns.push(column);
                        }
                    }
                }
                match table_from_schema(schema, facts.index.as_deref(), &columns) {
                    Some(table) => (
                        SourceTable {
                            name: table.name.clone(),
                            schema: Some(table.schema.clone()).filter(|s| !s.is_empty()),
                        },
                        AliasSource::Schema,
                    ),
                    None => continue,
                }
            }
        };
        resolved.push((position, printed, table, source));
    }
    resolved
        .into_iter()
        .map(|(position, printed, table, source)| {
            let node = &mut plan.nodes[position];
            node.relation_resolved = true;
            if !table.name.eq_ignore_ascii_case(&printed) {
                node.alias = Some(printed.clone());
            }
            node.relation = Some(table.name.clone());
            if node.schema.is_none() {
                node.schema = table.schema.clone();
            }
            ResolvedAlias {
                node_id: node.id,
                printed,
                table: table.name,
                schema: table.schema,
                source,
            }
        })
        .collect()
}

/// The one table in `schema` that owns `index` and has every column in
/// `columns`. `None` when nothing identifies a table or more than one fits.
fn table_from_schema<'s>(
    schema: &'s SchemaContext,
    index: Option<&str>,
    columns: &[String],
) -> Option<&'s SchemaTable> {
    let index = index
        .map(bare_name)
        .filter(|index| !index.is_empty() && !index.eq_ignore_ascii_case("PRIMARY"));
    if index.is_none() && columns.is_empty() {
        return None;
    }
    let mut fits = schema.tables.iter().filter(|table| {
        index
            .as_deref()
            .is_none_or(|index| table.index(index).is_some())
            && columns.iter().all(|column| table.column(column).is_some())
            // A table known only by name proves nothing about its columns.
            && (index.is_some() || table.has_columns())
    });
    let first = fits.next()?;
    fits.next().is_none().then_some(first)
}

/// Column names written as `qualifier.column` in `predicate`.
fn qualified_columns(predicate: &str, qualifier: &str) -> Vec<String> {
    let mut out = Vec::new();
    let lowered = predicate.to_ascii_lowercase();
    let needle = qualifier.to_ascii_lowercase();
    if needle.is_empty() {
        return out;
    }
    let is_word = |ch: char| ch.is_alphanumeric() || ch == '_' || ch == '$';
    let mut from = 0;
    while let Some(found) = lowered[from..].find(&needle) {
        let start = from + found;
        let end = start + needle.len();
        from = end;
        let before = lowered[..start].chars().next_back();
        if before.is_some_and(|ch| is_word(ch) || ch == '.') {
            continue;
        }
        // Skip a closing quote, then require the dot.
        let mut rest = &predicate[end..];
        rest = rest.strip_prefix(['`', '"', ']']).unwrap_or(rest);
        let Some(rest) = rest.strip_prefix('.') else {
            continue;
        };
        let rest = rest.strip_prefix(['`', '"', '[']).unwrap_or(rest);
        let name: String = rest.chars().take_while(|ch| is_word(*ch)).collect();
        if !name.is_empty() {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests;
