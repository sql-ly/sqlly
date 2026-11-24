//! Review mode's deterministic checks, run before any model sees the
//! notebook: fanout joins, ambiguous aggregates, stale or reused results,
//! cells that skip the notebook's filters, sampled data, clock functions
//! that depend on the server's time zone, and safety concerns. SQL is read
//! with the shared tokenizer (comments and strings never match), and data
//! checks use the analytics profiler over the cell's own output.

use std::collections::BTreeMap;

use sqlly_core::StoredMessage;
use sqlly_lang::tokenizer::{TokenKind, tokenize};

use super::context::{NotebookContext, hidden_columns, view_completeness};
use crate::analytics::OutputView;
use crate::analytics::profile::{ProfileOptions, profile_output};
use crate::notebook::cells::ai::{Evidence, Finding, FindingKind, FindingOrigin, ReviewCheck};
use crate::notebook::{Cell, CellKind, Completeness, Freshness};

/// A table in a FROM or JOIN clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    /// As written (last name part, quotes removed).
    pub name: String,
    /// Lower-case alias, or the lower-case name when there is none.
    pub alias: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    pub qualifier: Option<String>,
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Aggregate {
    pub function: String,
    pub distinct: bool,
    pub star: bool,
    pub argument: Option<ColumnRef>,
    /// As written, for messages.
    pub text: String,
}

/// The shape of one SQL statement, enough for the checks.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SqlShape {
    pub tables: Vec<TableRef>,
    pub joins: usize,
    pub equalities: Vec<(ColumnRef, ColumnRef)>,
    pub aggregates: Vec<Aggregate>,
    pub has_where: bool,
    pub select_star: bool,
    pub clock_functions: Vec<String>,
}

impl SqlShape {
    pub fn table_for(&self, qualifier: &str) -> Option<&TableRef> {
        let q = qualifier.to_lowercase();
        self.tables
            .iter()
            .find(|t| t.alias == q || t.name.to_lowercase() == q)
    }
}

const CLOCK_FUNCTIONS: &[&str] = &[
    "getdate",
    "sysdatetime",
    "sysdatetimeoffset",
    "current_timestamp",
    "current_date",
    "current_time",
    "localtimestamp",
    "localtime",
    "now",
    "sysdate",
    "curdate",
    "curtime",
];

const AGGREGATES: &[&str] = &["sum", "avg", "count", "count_big"];

const CLAUSE_WORDS: &[&str] = &[
    "on", "where", "group", "order", "having", "join", "inner", "left", "right", "full", "cross",
    "outer", "union", "limit", "offset", "with", "using", "natural", "lateral", "apply", "window",
    "qualify", "fetch", "for", "option", "into", "values", "select", "set", "pivot", "unpivot",
];

fn unquote(text: &str) -> String {
    text.trim_matches(|c| matches!(c, '[' | ']' | '"' | '`'))
        .replace("]]", "]")
}

/// Words (identifiers, keywords, quoted names), symbols and numbers, with
/// whitespace, comments and string literals dropped.
fn words(sql: &str) -> Vec<String> {
    let tokens = tokenize(sql);
    let mut out = vec![];
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        match token.kind {
            TokenKind::Whitespace | TokenKind::Comment => {}
            TokenKind::StringLiteral => out.push("'…'".to_string()),
            TokenKind::Symbol if token.text == "\"" || token.text == "`" => {
                let mut j = i + 1;
                while j < tokens.len() && tokens[j].text != token.text {
                    j += 1;
                }
                let end = tokens.get(j).map(|t| t.start).unwrap_or(sql.len());
                out.push(format!("\"{}\"", &sql[token.end..end]));
                i = j;
            }
            _ => out.push(token.text.to_string()),
        }
        i += 1;
    }
    out
}

fn is_name(word: &str) -> bool {
    word.starts_with(|c: char| {
        c.is_alphabetic() || c == '_' || c == '[' || c == '"' || c == '#' || c == '@'
    })
}

fn lower(word: &str) -> String {
    word.to_ascii_lowercase()
}

/// Read a dotted name starting at `i`; returns the parts and the next index.
fn dotted(words: &[String], mut i: usize) -> (Vec<String>, usize) {
    let mut parts = vec![];
    while i < words.len() && is_name(&words[i]) {
        parts.push(unquote(&words[i]));
        if words.get(i + 1).map(String::as_str) == Some(".") {
            i += 2;
        } else {
            i += 1;
            break;
        }
    }
    (parts, i)
}

/// The shape of `sql` (first statement's worth of clauses; good enough for
/// notebook cells, which hold one query).
pub fn shape(sql: &str) -> SqlShape {
    let words = words(sql);
    let mut shape = SqlShape::default();
    let mut i = 0;
    while i < words.len() {
        let w = lower(&words[i]);
        if w == "join" {
            shape.joins += 1;
        }
        if w == "where" {
            shape.has_where = true;
        }
        if w == "*" && i > 0 && matches!(lower(&words[i - 1]).as_str(), "select" | ",") {
            shape.select_star = true;
        }
        if CLOCK_FUNCTIONS.contains(&w.as_str()) {
            let call = words.get(i + 1).map(String::as_str) == Some("(");
            if call
                || matches!(
                    w.as_str(),
                    "current_timestamp"
                        | "current_date"
                        | "current_time"
                        | "localtimestamp"
                        | "localtime"
                        | "sysdate"
                )
            {
                let name = words[i].to_uppercase() + if call { "()" } else { "" };
                if !shape.clock_functions.contains(&name) {
                    shape.clock_functions.push(name);
                }
            }
        }
        if (w == "from" || w == "join") && i + 1 < words.len() {
            // A notebook reference {{name}} reads as a table named `name`.
            let mut j = i + 1;
            let mut name = None;
            if words[j] == "{" && words.get(j + 1).map(String::as_str) == Some("{") {
                if let Some(inner) = words.get(j + 2) {
                    name = Some(unquote(inner));
                }
                j += 2;
                while j < words.len() && words[j] != "}" {
                    j += 1;
                }
                j += 2;
            } else if is_name(&words[j]) && !CLAUSE_WORDS.contains(&lower(&words[j]).as_str()) {
                let (parts, next) = dotted(&words, j);
                name = parts.last().cloned();
                j = next;
            }
            if let Some(name) = name {
                if words.get(j).map(|w| lower(w)) == Some("as".into()) {
                    j += 1;
                }
                let alias = match words.get(j) {
                    Some(word)
                        if is_name(word) && !CLAUSE_WORDS.contains(&lower(word).as_str()) =>
                    {
                        j += 1;
                        lower(&unquote(word))
                    }
                    _ => name.to_lowercase(),
                };
                shape.tables.push(TableRef { name, alias });
                i = j;
                continue;
            }
        }
        if AGGREGATES.contains(&w.as_str()) && words.get(i + 1).map(String::as_str) == Some("(") {
            let mut j = i + 2;
            let mut aggregate = Aggregate {
                function: w.to_uppercase(),
                distinct: false,
                star: false,
                argument: None,
                text: String::new(),
            };
            if words.get(j).map(|w| lower(w)) == Some("distinct".into()) {
                aggregate.distinct = true;
                j += 1;
            }
            if words.get(j).map(String::as_str) == Some("*") {
                aggregate.star = true;
            } else if words.get(j).is_some_and(|w| is_name(w)) {
                let (parts, next) = dotted(&words, j);
                if words.get(next).map(String::as_str) == Some(")") {
                    aggregate.argument = match parts.as_slice() {
                        [column] => Some(ColumnRef {
                            qualifier: None,
                            column: column.clone(),
                        }),
                        [.., qualifier, column] => Some(ColumnRef {
                            qualifier: Some(qualifier.to_lowercase()),
                            column: column.clone(),
                        }),
                        [] => None,
                    };
                }
            }
            let mut depth = 0usize;
            let mut end = i + 1;
            while end < words.len() {
                match words[end].as_str() {
                    "(" => depth += 1,
                    ")" => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                end += 1;
            }
            aggregate.text = words[i..=end.min(words.len() - 1)]
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join("")
                .replace("DISTINCT", "DISTINCT ")
                .replace("distinct", "DISTINCT ");
            shape.aggregates.push(aggregate);
        }
        if words[i] == "="
            && i >= 3
            && words[i - 2] == "."
            && words.get(i + 2).map(String::as_str) == Some(".")
            && i + 3 < words.len()
        {
            let left = ColumnRef {
                qualifier: Some(lower(&unquote(&words[i - 3]))),
                column: unquote(&words[i - 1]),
            };
            let right = ColumnRef {
                qualifier: Some(lower(&unquote(&words[i + 1]))),
                column: unquote(&words[i + 3]),
            };
            shape.equalities.push((left, right));
        }
        i += 1;
    }
    shape
}

fn compact(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_alphanumeric())
        .collect::<String>()
        .to_lowercase()
}

fn singular(name: &str) -> String {
    let n = compact(name);
    if let Some(stem) = n.strip_suffix("ies") {
        format!("{stem}y")
    } else if let Some(stem) = n.strip_suffix("sses") {
        format!("{stem}ss")
    } else if let Some(stem) = n.strip_suffix('s') {
        stem.to_string()
    } else {
        n
    }
}

/// Whether `column` is `table`'s own key by naming convention: `id`,
/// `<table>id` or `<table>_id` (singular or plural).
pub fn is_own_key(table: &str, column: &str) -> bool {
    let column = compact(column);
    column == "id"
        || column == format!("{}id", singular(table))
        || column == format!("{}id", compact(table))
}

/// A join where one side's own key meets the other side's foreign key: the
/// key side is repeated once per row of the other.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneToMany {
    pub one: TableRef,
    pub many: TableRef,
    pub key: String,
}

pub fn one_to_many(shape: &SqlShape) -> Vec<OneToMany> {
    let mut out = vec![];
    for (left, right) in &shape.equalities {
        let (Some(lq), Some(rq)) = (&left.qualifier, &right.qualifier) else {
            continue;
        };
        let (Some(lt), Some(rt)) = (shape.table_for(lq), shape.table_for(rq)) else {
            continue;
        };
        if lt == rt {
            continue;
        }
        let left_key = is_own_key(&lt.name, &left.column);
        let right_key = is_own_key(&rt.name, &right.column);
        let found = match (left_key, right_key) {
            (true, false) => Some((lt, rt, &left.column)),
            (false, true) => Some((rt, lt, &right.column)),
            _ => None,
        };
        if let Some((one, many, key)) = found {
            out.push(OneToMany {
                one: one.clone(),
                many: many.clone(),
                key: key.clone(),
            });
        }
    }
    out
}

struct Builder<'a> {
    ctx: &'a NotebookContext,
    out: Vec<Finding>,
}

impl Builder<'_> {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        cell: &Cell,
        check: ReviewCheck,
        kind: FindingKind,
        key: &str,
        english: String,
        arguments: &[(&str, String)],
        group: Option<String>,
    ) {
        let id = format!("r{}", self.out.len() + 1);
        let mut finding = Finding::new(id, kind, english);
        finding.message = Some(StoredMessage {
            key: key.to_string(),
            arguments: arguments
                .iter()
                .map(|(k, v)| (k.to_string(), v.clone()))
                .collect(),
        });
        finding.check = Some(check);
        finding.origin = FindingOrigin::Deterministic;
        finding.evidence.push(Evidence {
            cell: Some(cell.id.clone()),
            run: self.ctx.outputs.get(&cell.id).map(|o| o.run_id.clone()),
            group,
            ..Evidence::default()
        });
        finding.verified =
            kind != FindingKind::ObservedFact || self.ctx.outputs.contains_key(&cell.id);
        self.out.push(finding);
    }
}

fn uses_parameter(cell: &Cell, name: &str) -> bool {
    let source = cell.source.to_lowercase();
    let name = name.to_lowercase();
    [
        format!(":{name}"),
        format!("@{name}"),
        format!("{{{{param.{name}"),
    ]
    .iter()
    .any(|needle| source.contains(needle.as_str()))
}

fn freshness_message(freshness: Freshness) -> (&'static str, &'static str) {
    match freshness {
        Freshness::Live => ("", ""),
        Freshness::TtlCached => (
            "notebook.ai.review.cached_result_ttl",
            "This result was reused from the cache, not fetched by its last run.",
        ),
        Freshness::Pinned => (
            "notebook.ai.review.cached_result_pinned",
            "This result is a pinned copy, not fetched by its last run.",
        ),
        Freshness::SourceVerified => (
            "notebook.ai.review.cached_result_verified",
            "This result was reused because the source reported no change, not fetched again.",
        ),
        Freshness::Snapshot => (
            "notebook.ai.review.cached_result_snapshot",
            "This result comes from a saved snapshot, not a live run.",
        ),
    }
}

fn completeness_key(completeness: Completeness) -> (&'static str, &'static str) {
    match completeness {
        Completeness::Sampled { .. } => (
            "notebook.ai.review.sampled_data_sample",
            "This result is a sample of {shown} of {total} rows; totals and averages over it do not describe all the data.",
        ),
        Completeness::Partial => (
            "notebook.ai.review.sampled_data_partial",
            "This result stopped early after {shown} of {total} rows; totals and averages over it do not describe all the data.",
        ),
        _ => (
            "notebook.ai.review.sampled_data_truncated",
            "This result keeps the first {shown} of {total} rows; totals and averages over it do not describe all the data.",
        ),
    }
}

fn check_cell(b: &mut Builder<'_>, cell: &Cell) {
    let ctx = b.ctx;
    let shape = shape(&cell.source);
    let links = one_to_many(&shape);
    let mut fanout_flagged = false;
    for link in &links {
        for aggregate in &shape.aggregates {
            if aggregate.distinct || aggregate.star {
                continue;
            }
            let Some(argument) = &aggregate.argument else {
                continue;
            };
            let on_one_side = argument
                .qualifier
                .as_deref()
                .and_then(|q| shape.table_for(q))
                .is_some_and(|t| t == &link.one);
            if !on_one_side {
                continue;
            }
            fanout_flagged = true;
            let arguments = [
                ("aggregate", aggregate.text.clone()),
                ("one_table", link.one.name.clone()),
                ("many_table", link.many.name.clone()),
                ("key", link.key.clone()),
            ];
            b.push(
                cell,
                ReviewCheck::FanoutJoin,
                FindingKind::Hypothesis,
                "notebook.ai.review.fanout_join",
                format!(
                    "{} adds each {} row once per matching {} row: the join on {} repeats it, so the total is inflated.",
                    aggregate.text, link.one.name, link.many.name, link.key
                ),
                &arguments,
                Some(aggregate.text.clone()),
            );
            b.push(
                cell,
                ReviewCheck::FanoutJoin,
                FindingKind::Recommendation,
                "notebook.ai.review.fanout_fix",
                format!(
                    "Aggregate {} by {} in a subquery before computing {}, then join it to {}, so each {} row counts once.",
                    link.many.name, link.key, aggregate.text, link.one.name, link.one.name
                ),
                &arguments,
                None,
            );
        }
    }
    if let Some(output) = ctx.outputs.get(&cell.id)
        && !links.is_empty()
    {
        let profile = profile_output(
            &OutputView::new(&output.columns, &output.rows, view_completeness(output)),
            &ProfileOptions::default(),
        );
        for link in &links {
            let Some(column) = profile.column(&link.key) else {
                continue;
            };
            let non_null = column.non_null_count();
            if column.distinct.value < non_null && !column.distinct.approximate {
                fanout_flagged = true;
                b.push(
                    cell,
                    ReviewCheck::FanoutJoin,
                    FindingKind::ObservedFact,
                    "notebook.ai.review.fanout_rows",
                    format!(
                        "{} repeats in this result: {} rows but {} distinct values, so totals over {} double count.",
                        column.name, non_null, column.distinct.value, link.one.name
                    ),
                    &[
                        ("column", column.name.clone()),
                        ("rows", non_null.to_string()),
                        ("distinct", column.distinct.value.to_string()),
                        ("one_table", link.one.name.clone()),
                    ],
                    Some(column.name.clone()),
                );
            }
        }
    }
    if shape.joins > 0 && !fanout_flagged {
        for aggregate in shape.aggregates.iter().filter(|a| a.star && !a.distinct) {
            b.push(
                cell,
                ReviewCheck::AmbiguousAggregate,
                FindingKind::Hypothesis,
                "notebook.ai.review.count_star_join",
                format!(
                    "{} after a join counts joined rows, not rows of any one table; use COUNT(DISTINCT key) for entities.",
                    aggregate.text
                ),
                &[("aggregate", aggregate.text.clone())],
                Some(aggregate.text.clone()),
            );
        }
    }
    for aggregate in shape
        .aggregates
        .iter()
        .filter(|a| a.distinct && a.function != "COUNT" && a.function != "COUNT_BIG")
    {
        b.push(
            cell,
            ReviewCheck::AmbiguousAggregate,
            FindingKind::Hypothesis,
            "notebook.ai.review.distinct_aggregate",
            format!(
                "{} drops repeated values that may be legitimate, such as two orders with the same amount.",
                aggregate.text
            ),
            &[("aggregate", aggregate.text.clone())],
            Some(aggregate.text.clone()),
        );
    }
    let runtime = ctx.runtime.get(&cell.id).cloned().unwrap_or_default();
    if runtime.stale {
        b.push(
            cell,
            ReviewCheck::StaleResult,
            FindingKind::ObservedFact,
            "notebook.ai.review.stale_result",
            "This result is out of date: the cell or one of its inputs changed after it ran."
                .into(),
            &[],
            None,
        );
    }
    if let Some(output) = ctx.outputs.get(&cell.id) {
        if output.freshness != Freshness::Live {
            let (key, english) = freshness_message(output.freshness);
            b.push(
                cell,
                ReviewCheck::StaleResult,
                FindingKind::ObservedFact,
                key,
                english.to_string(),
                &[],
                None,
            );
        }
        if !output.completeness.supports_summaries() {
            let (key, template) = completeness_key(output.completeness);
            let shown = output.rows.len().to_string();
            let total = output.row_count.to_string();
            b.push(
                cell,
                ReviewCheck::SampledData,
                FindingKind::ObservedFact,
                key,
                template
                    .replace("{shown}", &shown)
                    .replace("{total}", &total),
                &[("shown", shown.clone()), ("total", total.clone())],
                None,
            );
        }
        let hidden: Vec<String> = hidden_columns(output)
            .into_iter()
            .map(|i| output.columns[i].name.clone())
            .collect();
        if !hidden.is_empty() {
            let columns = hidden.join(", ");
            b.push(
                cell,
                ReviewCheck::Security,
                FindingKind::ObservedFact,
                "notebook.ai.review.sensitive_columns",
                format!(
                    "The result includes sensitive-looking columns ({columns}); AI context and exports hide their values."
                ),
                &[("columns", columns.clone())],
                None,
            );
        }
    }
    let params: Vec<&str> = ctx.doc.parameters.iter().map(|p| p.name.as_str()).collect();
    if !params.is_empty()
        && !params.iter().any(|p| uses_parameter(cell, p))
        && !cell.source.contains("{{")
        && ctx
            .doc
            .cells
            .iter()
            .filter(|c| c.kind == CellKind::Sql && c.id != cell.id)
            .any(|c| params.iter().any(|p| uses_parameter(c, p)))
    {
        let list = params.join(", ");
        b.push(
            cell,
            ReviewCheck::MissingFilter,
            FindingKind::Hypothesis,
            "notebook.ai.review.missing_filter",
            format!(
                "This cell uses none of the notebook's parameters ({list}), while other cells filter by them."
            ),
            &[("parameters", list.clone())],
            None,
        );
    }
    for function in &shape.clock_functions {
        b.push(
            cell,
            ReviewCheck::Timezone,
            FindingKind::Hypothesis,
            "notebook.ai.review.timezone",
            format!(
                "{function} uses the database server's clock and time zone, so dates near midnight can shift. Prefer a UTC function or a date parameter."
            ),
            &[("function", function.clone())],
            Some(function.clone()),
        );
    }
    if !cell.execution_policy.read_only {
        b.push(
            cell,
            ReviewCheck::Security,
            FindingKind::ObservedFact,
            "notebook.ai.review.write_cell",
            "This cell is allowed to change data; the agent never edits or runs it.".into(),
            &[],
            None,
        );
    }
}

/// Run every deterministic check over `targets` (all SQL cells when empty).
pub fn review(ctx: &NotebookContext, targets: &[String]) -> Vec<Finding> {
    let mut builder = Builder { ctx, out: vec![] };
    let cells: Vec<&Cell> = if targets.is_empty() {
        ctx.doc
            .cells
            .iter()
            .filter(|c| c.kind == CellKind::Sql)
            .collect()
    } else {
        targets
            .iter()
            .filter_map(|t| ctx.resolve_cell(t))
            .filter(|c| c.kind == CellKind::Sql)
            .collect()
    };
    for cell in cells {
        check_cell(&mut builder, cell);
    }
    builder.out
}

/// Ambiguous-term findings for a prompt the model answered without asking.
pub fn ambiguous_term_findings(terms: &[String], first: usize) -> Vec<Finding> {
    terms
        .iter()
        .enumerate()
        .map(|(i, term)| {
            let mut finding = Finding::new(
                format!("r{}", first + i),
                FindingKind::Hypothesis,
                format!(
                    "“{term}” can mean different things and the notebook does not define it; the answer assumed a meaning."
                ),
            );
            finding.message = Some(StoredMessage {
                key: "notebook.ai.review.ambiguous_term".into(),
                arguments: vec![("term".into(), term.clone())],
            });
            finding.check = Some(ReviewCheck::AmbiguousTerm);
            finding.origin = FindingOrigin::Deterministic;
            finding.unverified_reason = Some(crate::notebook::cells::ai::UnverifiedReason::NoEvidence);
            finding
        })
        .collect()
}

/// Count findings per check, for tests and the log.
pub fn by_check(findings: &[Finding]) -> BTreeMap<ReviewCheck, usize> {
    let mut out = BTreeMap::new();
    for check in findings.iter().filter_map(|f| f.check) {
        *out.entry(check).or_insert(0) += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notebook::fingerprint::Fingerprint;
    use crate::notebook::params::{ParamType, ParameterDef};
    use crate::notebook::{CellOutput, ExecutionPolicy, NotebookDoc, Provenance, SeededIds};
    use crate::notebook_security::TrustDecision;
    use sqlly_core::{CellValue, ResultColumn};
    use std::sync::Arc;

    const FANOUT: &str = "SELECT c.Country, SUM(o.Freight) AS Freight, COUNT(*) AS Lines\n\
        FROM dbo.Orders AS o\n\
        JOIN dbo.[Order Details] d ON d.OrderID = o.OrderID -- SUM(o.x) in a comment\n\
        JOIN dbo.Customers c ON c.CustomerID = o.CustomerID\n\
        WHERE o.OrderDate >= DATEADD(day, -30, GETDATE())\n\
        GROUP BY c.Country";

    #[test]
    fn shapes_read_tables_aliases_joins_and_aggregates() {
        let shape = shape(FANOUT);
        assert_eq!(shape.joins, 2);
        assert_eq!(
            shape
                .tables
                .iter()
                .map(|t| (t.name.as_str(), t.alias.as_str()))
                .collect::<Vec<_>>(),
            vec![("Orders", "o"), ("Order Details", "d"), ("Customers", "c")]
        );
        assert_eq!(shape.aggregates.len(), 2, "the comment is ignored");
        assert_eq!(shape.aggregates[0].text, "SUM(o.Freight)");
        assert!(shape.aggregates[1].star);
        assert!(shape.has_where);
        assert_eq!(shape.clock_functions, vec!["GETDATE()"]);
        let links = one_to_many(&shape);
        assert_eq!(links.len(), 2);
        assert_eq!(
            (links[0].one.name.as_str(), links[0].many.name.as_str()),
            ("Orders", "Order Details")
        );
        assert_eq!(
            (links[1].one.name.as_str(), links[1].many.name.as_str()),
            ("Customers", "Orders")
        );
        let refs = super::shape(
            "SELECT SUM(r.Revenue) FROM {{orders}} AS r JOIN customers c ON c.id = r.customer_id",
        );
        assert_eq!(refs.tables[0].name, "orders");
        assert_eq!(refs.tables[0].alias, "r");
        assert_eq!(one_to_many(&refs)[0].one.name, "customers");
        assert!(is_own_key("Categories", "CategoryID"));
        assert!(is_own_key("order_items", "id"));
        assert!(!is_own_key("Order Details", "OrderID"));
    }

    fn ctx(cells: Vec<Cell>, outputs: Vec<(&str, CellOutput)>) -> NotebookContext {
        let mut doc = NotebookDoc::new("t", &mut SeededIds(7));
        doc.cells = cells;
        NotebookContext {
            trust: TrustDecision::untrusted(&doc),
            doc,
            outputs: outputs
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            runtime: BTreeMap::new(),
            connections: BTreeMap::new(),
            ai_cell: "c_ai".into(),
        }
    }

    fn ctx_of(cells: Vec<Cell>) -> NotebookContext {
        ctx(cells, vec![])
    }

    fn sql(id: &str, source: &str) -> Cell {
        let mut cell = Cell::new(id, CellKind::Sql, ExecutionPolicy::default());
        cell.source = source.into();
        cell
    }

    fn output(
        columns: &[&str],
        rows: Vec<Vec<CellValue>>,
        completeness: Completeness,
    ) -> CellOutput {
        CellOutput {
            columns: columns
                .iter()
                .map(|c| ResultColumn {
                    name: c.to_string(),
                    data_type: "int".into(),
                    nullable: true,
                })
                .collect(),
            row_count: 1_000,
            rows: Arc::new(rows),
            completeness,
            elapsed_ms: 1,
            run_id: "run_5".into(),
            fingerprint: Fingerprint("f".into()),
            provenance: Provenance::default(),
            freshness: Freshness::Live,
        }
    }

    #[test]
    fn a_fanout_join_is_flagged_with_a_fix_and_its_evidence() {
        let ctx = ctx(vec![sql("c_freight", FANOUT)], vec![]);
        let findings = review(&ctx, &[]);
        let checks = by_check(&findings);
        assert_eq!(checks.get(&ReviewCheck::FanoutJoin), Some(&2));
        assert_eq!(checks.get(&ReviewCheck::Timezone), Some(&1));
        assert!(
            !checks.contains_key(&ReviewCheck::AmbiguousAggregate),
            "COUNT(*) is covered by the fanout finding"
        );
        let fanout = &findings[0];
        assert_eq!(fanout.kind, FindingKind::Hypothesis);
        assert!(
            fanout.text.contains(
                "SUM(o.Freight) adds each Orders row once per matching Order Details row"
            )
        );
        assert_eq!(
            fanout.message.as_ref().unwrap().key,
            "notebook.ai.review.fanout_join"
        );
        assert_eq!(fanout.evidence[0].cell.as_deref(), Some("c_freight"));
        assert_eq!(fanout.evidence[0].group.as_deref(), Some("SUM(o.Freight)"));
        assert_eq!(findings[1].kind, FindingKind::Recommendation);
        assert!(
            findings
                .iter()
                .all(|f| f.origin == FindingOrigin::Deterministic)
        );

        let safe = "SELECT c.Country, SUM(t.Freight) FROM Customers c JOIN \
            (SELECT CustomerID, SUM(Freight) AS Freight FROM Orders GROUP BY CustomerID) t ON t.CustomerID = c.CustomerID GROUP BY c.Country";
        let safe_checks = by_check(&review(&ctx_of(vec![sql("c", safe)]), &[]));
        assert!(!safe_checks.contains_key(&ReviewCheck::FanoutJoin));
    }

    #[test]
    fn repeated_keys_in_a_joined_result_are_an_observed_fanout() {
        let rows: Vec<Vec<CellValue>> = (0..30)
            .map(|i| vec![CellValue::I64(i / 3), CellValue::I64(i)])
            .collect();
        let ctx = ctx(
            vec![sql(
                "c_lines",
                "SELECT o.OrderID, d.ProductID FROM Orders o JOIN OrderLines d ON d.OrderID = o.OrderID",
            )],
            vec![(
                "c_lines",
                output(&["OrderID", "ProductID"], rows, Completeness::Complete),
            )],
        );
        let findings = review(&ctx, &[]);
        let fact = findings
            .iter()
            .find(|f| f.message.as_ref().unwrap().key == "notebook.ai.review.fanout_rows")
            .unwrap();
        assert_eq!(fact.kind, FindingKind::ObservedFact);
        assert!(fact.verified);
        assert!(fact.text.contains("30 rows but 10 distinct"));
        assert_eq!(fact.evidence[0].run.as_deref(), Some("run_5"));
    }

    #[test]
    fn stale_sampled_unfiltered_and_unsafe_cells_are_reported() {
        let mut doc_param = ParameterDef::new("region", ParamType::String);
        doc_param.id = "p_region".into();
        let mut writer = sql("c_write", "UPDATE t SET x = 1");
        writer.execution_policy.read_only = false;
        let cells = vec![
            sql("c_filtered", "SELECT * FROM sales WHERE region = :region"),
            sql("c_all", "SELECT SUM(DISTINCT amount) FROM sales"),
            writer,
        ];
        let mut sampled = output(
            &["amount", "card_number"],
            vec![vec![CellValue::I64(1), CellValue::Null]],
            Completeness::Sampled { n: 1 },
        );
        sampled.freshness = Freshness::TtlCached;
        let mut context = ctx(cells, vec![("c_all", sampled)]);
        context.doc.parameters.push(doc_param);
        context.runtime.insert(
            "c_all".into(),
            super::super::context::CellRuntime {
                stale: true,
                ..Default::default()
            },
        );
        let findings = review(&context, &["c_all".into(), "c_write".into()]);
        let keys: Vec<&str> = findings
            .iter()
            .map(|f| f.message.as_ref().unwrap().key.as_str())
            .collect();
        for key in [
            "notebook.ai.review.distinct_aggregate",
            "notebook.ai.review.stale_result",
            "notebook.ai.review.cached_result_ttl",
            "notebook.ai.review.sampled_data_sample",
            "notebook.ai.review.sensitive_columns",
            "notebook.ai.review.missing_filter",
            "notebook.ai.review.write_cell",
        ] {
            assert!(keys.contains(&key), "{key} missing from {keys:?}");
        }
        assert!(!keys.contains(&"notebook.ai.review.timezone"));
    }
}
