//! PostgreSQL text `EXPLAIN` / `EXPLAIN ANALYZE`, the format people paste
//! most. Every line is turned into the keys `EXPLAIN (FORMAT JSON)` would
//! have printed, so the JSON walker in `pg` fills the model for both.
//!
//! Also reads the same layout as printed by engines built on PostgreSQL's
//! planner output: Amazon Redshift (`XN` prefixed operators), Greenplum
//! (`Gather Motion 3:1  (slice1; segments: 3)`), and auto_explain log
//! entries.

use serde_json::{Map, Value};

use super::pg::{EXTRA_KEY, PgPlanBuilder};
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::ExecutionPlan;

/// Lines deeper than this are not a plan a person can read.
const MAX_NODES: usize = 100_000;

/// Parse PostgreSQL text EXPLAIN output. One plan per statement: a rule
/// that rewrites a query into several prints several plans separated by a
/// blank line.
pub fn parse_pg_explain_text(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let lines = prepare_lines(doc);
    let has_stats = lines.iter().any(|line| stats_start(&line.text).is_some());
    // Without a single cost or timing the first line has to name an
    // operator, or any prose would read as a one-operator plan.
    let first_is_operator = lines
        .iter()
        .find(|line| !line.text.is_empty())
        .is_some_and(|line| starts_with_pg_operator(&line.text));
    if !has_stats && !first_is_operator {
        return Err(no_plan_lines());
    }
    let mut plans = Vec::new();
    let mut current = RawPlan::default();
    let mut pending_query_text: Option<String> = None;
    let mut blank_before = false;

    for line in &lines {
        if line.text.is_empty() {
            blank_before = true;
            current.last_property = None;
            continue;
        }
        let was_blank = std::mem::take(&mut blank_before);
        let arrow = line.text.starts_with("->");
        let stats = stats_start(&line.text);

        if current.nodes.is_empty() {
            if let Some(text) = line.text.strip_prefix("Query Text:") {
                pending_query_text = Some(text.trim().to_string());
                continue;
            }
            // Root: the first line with a cost, or (COSTS OFF) the first line.
            let is_root = if has_stats {
                stats.is_some()
            } else {
                !arrow && !is_noise(&line.text)
            };
            if !is_root {
                continue;
            }
            if let Some(text) = pending_query_text.take() {
                current.top.insert("Query Text".into(), Value::String(text));
            }
            current.root_indent = line.indent;
            current.push_node(&line.text, line.indent, None);
            continue;
        }

        if !arrow && line.indent <= current.root_indent {
            // Back at the left margin: statement-level lines, or a new plan.
            let starts_plan = if has_stats {
                stats.is_some()
            } else {
                was_blank && label_of(&line.text).is_none()
            };
            if starts_plan {
                plans.push(std::mem::take(&mut current));
                current.root_indent = line.indent;
                current.push_node(&line.text, line.indent, None);
            } else {
                current.trailer_line(&line.text);
            }
            continue;
        }
        if current.in_trailer {
            current.trailer_subline(&line.text);
            continue;
        }
        if arrow {
            if current.nodes.len() >= MAX_NODES {
                break;
            }
            let text = line.text.trim_start_matches("->").trim_start();
            current.push_node(text, line.indent, Some(line.indent));
            continue;
        }
        current.node_line(&line.text, line.indent);
    }
    plans.push(current);

    let mut out = Vec::new();
    for raw in plans {
        if let Some(plan) = raw.finish() {
            out.push(plan);
        }
    }
    if out.is_empty() {
        return Err(no_plan_lines());
    }
    Ok(out)
}

fn no_plan_lines() -> PlanError {
    PlanError::InvalidStructure(
        "PostgreSQL EXPLAIN text: no plan lines found. A plan line looks like \"Seq Scan on orders  (cost=0.00..7054.00 rows=400000 width=27)\".".into(),
    )
}

/// True when `doc` is PostgreSQL-style text EXPLAIN output that carries
/// costs or run-time numbers. PostgreSQL prints `(cost=a..b rows=N
/// width=W)`; MySQL's tree format prints `(cost=N rows=N)` without the
/// range and starts every line, the first included, with `-> `.
pub fn looks_like_pg_explain_text(doc: &str) -> bool {
    let lines = prepare_lines(doc);
    let mut first = true;
    let mut strong = false;
    for line in lines.iter().filter(|line| !line.text.is_empty()) {
        if line.text.starts_with("->") {
            // PostgreSQL never puts an arrow on the root, and always follows
            // the arrow with two spaces.
            if first || !line.text.starts_with("->  ") {
                return false;
            }
        }
        if let Some(at) = stats_start(&line.text) {
            let stats = &line.text[at..];
            if pg_cost(stats).is_some()
                || stats.starts_with("(never executed)")
                || pg_actual(stats).is_some()
            {
                strong = true;
            }
            first = false;
        } else if first && !strong {
            // Leading junk (a prompt, the statement) is allowed before the root.
            continue;
        }
    }
    strong
}

/// True for `EXPLAIN (COSTS OFF)` output: no numbers at all, only the
/// operator tree. Weaker evidence than [`looks_like_pg_explain_text`], so
/// it wants a known operator on the first line and PostgreSQL's `->  `
/// arrows or property lines below it.
pub fn looks_like_pg_explain_text_without_costs(doc: &str) -> bool {
    let lines = prepare_lines(doc);
    let mut content = lines.iter().filter(|line| !line.text.is_empty());
    let Some(root) = content.next() else {
        return false;
    };
    if root.text.starts_with("->") || !starts_with_pg_operator(&root.text) {
        return false;
    }
    let mut arrows = 0;
    let mut properties = 0;
    for line in content {
        if line.text.starts_with("->") {
            let rest = line.text.trim_start_matches("->");
            if !line.text.starts_with("->  ") || !starts_with_pg_operator(rest.trim_start()) {
                return false;
            }
            arrows += 1;
        } else if label_of(&line.text).is_some_and(|(label, _)| is_pg_property(label)) {
            properties += 1;
        } else if !is_subplan_header(&line.text) && line.indent <= root.indent {
            return false;
        }
    }
    arrows + properties > 0
}

const PG_OPERATORS: &[&str] = &[
    "Seq Scan",
    "Index Scan",
    "Index Only Scan",
    "Bitmap Heap Scan",
    "Bitmap Index Scan",
    "BitmapAnd",
    "BitmapOr",
    "Tid Scan",
    "Tid Range Scan",
    "Sample Scan",
    "Subquery Scan",
    "Function Scan",
    "Table Function Scan",
    "Values Scan",
    "CTE Scan",
    "WorkTable Scan",
    "Named Tuplestore Scan",
    "Foreign Scan",
    "Custom Scan",
    "Nested Loop",
    "Hash Join",
    "Hash Left Join",
    "Hash Right Join",
    "Hash Full Join",
    "Hash Semi Join",
    "Hash Anti Join",
    "Hash Right Semi Join",
    "Hash Right Anti Join",
    "Merge Join",
    "Merge Left Join",
    "Merge Right Join",
    "Merge Full Join",
    "Merge Semi Join",
    "Merge Anti Join",
    "Merge Append",
    "Hash",
    "Sort",
    "Incremental Sort",
    "Limit",
    "Aggregate",
    "HashAggregate",
    "GroupAggregate",
    "MixedAggregate",
    "Group",
    "WindowAgg",
    "Unique",
    "SetOp",
    "HashSetOp",
    "Append",
    "Recursive Union",
    "Result",
    "ProjectSet",
    "Materialize",
    "Memoize",
    "Gather",
    "Gather Merge",
    "LockRows",
    "Insert on",
    "Update on",
    "Delete on",
    "Merge on",
];

fn starts_with_pg_operator(text: &str) -> bool {
    let mut text = text;
    for prefix in ["Parallel ", "Partial ", "Finalize "] {
        text = text.strip_prefix(prefix).unwrap_or(text);
    }
    PG_OPERATORS.iter().any(|operator| {
        text.strip_prefix(operator)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    })
}

fn is_pg_property(label: &str) -> bool {
    matches!(
        label,
        "Filter"
            | "Index Cond"
            | "Recheck Cond"
            | "Hash Cond"
            | "Merge Cond"
            | "Join Filter"
            | "Sort Key"
            | "Group Key"
            | "Output"
            | "Workers Planned"
            | "One-Time Filter"
            | "TID Cond"
            | "Presorted Key"
            | "Cache Key"
            | "Cache Mode"
            | "Window"
            | "Inner Unique"
            | "Subplans Removed"
    )
}

struct Line {
    indent: usize,
    text: String,
}

/// Strip what clients add around the plan (psql's header, rule and row
/// count, table borders, pgAdmin's quotes) and measure each line's indent.
fn prepare_lines(doc: &str) -> Vec<Line> {
    let mut out = Vec::new();
    for raw in doc.lines() {
        let mut line = raw.trim_end_matches(['\r', ' ', '\t']).to_string();
        // pgAdmin and CSV exports quote every line and double inner quotes.
        if line.len() >= 2 && line.starts_with('"') && line.ends_with('"') {
            line = line[1..line.len() - 1].replace("\"\"", "\"");
        }
        // psql `\pset border 2` and Markdown tables.
        if let Some(rest) = line.strip_prefix('|') {
            line = rest
                .strip_suffix('|')
                .unwrap_or(rest)
                .trim_end()
                .to_string();
        }
        let expanded = line.replace('\t', "    ");
        let text = expanded.trim_start();
        let indent = expanded.len() - text.len();
        if is_noise(text) {
            // Decoration counts as nothing, not as a blank line.
            if text.is_empty() {
                out.push(Line {
                    indent: 0,
                    text: String::new(),
                });
            }
            continue;
        }
        out.push(Line {
            indent,
            text: text.to_string(),
        });
    }
    // A selection that starts at the first letter of the plan loses the one
    // space psql puts before every line, on the first line only. Unaligned
    // output never indents by exactly one, so this is unambiguous.
    let mut content = out.iter().filter(|line| !line.text.is_empty());
    let first_indent = content.next().map(|line| line.indent);
    let least_other = content.map(|line| line.indent).min();
    if first_indent == Some(0)
        && least_other == Some(1)
        && let Some(first) = out.iter_mut().find(|line| !line.text.is_empty())
    {
        first.indent = 1;
    }
    out
}

fn is_noise(text: &str) -> bool {
    if text.is_empty() || text == "QUERY PLAN" {
        return true;
    }
    if text.len() >= 3 && text.chars().all(|c| matches!(c, '-' | '+' | '=' | '─')) {
        return true;
    }
    // "(42 rows)" / "(1 row)"
    if let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) {
        let mut parts = inner.split(' ');
        if let (Some(count), Some("row" | "rows"), None) =
            (parts.next(), parts.next(), parts.next())
            && !count.is_empty()
            && count.chars().all(|c| c.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

/// Byte offset of the first statistics group on an operator line.
fn stats_start(text: &str) -> Option<usize> {
    ["(cost=", "(actual ", "(never executed)"]
        .iter()
        .filter_map(|marker| text.find(marker))
        .min()
}

/// `(cost=0.42..12.98 rows=203 width=38)` -> startup, total, rows, width.
fn pg_cost(stats: &str) -> Option<(f64, f64, f64, Option<f64>)> {
    let inner = stats.strip_prefix("(cost=")?;
    let inner = &inner[..inner.find(')')?];
    let mut parts = inner.split_whitespace();
    let (startup, total) = parts.next()?.split_once("..")?;
    let rows = parts.next()?.strip_prefix("rows=")?;
    let width = parts
        .next()
        .and_then(|part| part.strip_prefix("width="))
        .and_then(|width| width.parse().ok());
    Some((
        startup.parse().ok()?,
        total.parse().ok()?,
        rows.parse().ok()?,
        width,
    ))
}

struct Actual {
    startup: Option<f64>,
    total: Option<f64>,
    rows: f64,
    loops: f64,
}

/// `(actual time=0.011..0.088 rows=199.00 loops=1)`, or without the time
/// when `TIMING OFF` was used. Also reads the bare form on `Worker N:` lines.
fn pg_actual(stats: &str) -> Option<Actual> {
    let inner = stats.strip_prefix('(').unwrap_or(stats);
    let inner = inner.strip_prefix("actual ")?;
    let inner = &inner[..inner.find(')').unwrap_or(inner.len())];
    let mut actual = Actual {
        startup: None,
        total: None,
        rows: 0.0,
        loops: 0.0,
    };
    let mut seen_rows = false;
    let mut seen_loops = false;
    for part in inner.split_whitespace() {
        if let Some(time) = part.strip_prefix("time=") {
            let (startup, total) = time.split_once("..")?;
            actual.startup = startup.parse().ok();
            actual.total = total.parse().ok();
        } else if let Some(rows) = part.strip_prefix("rows=") {
            actual.rows = rows.parse().ok()?;
            seen_rows = true;
        } else if let Some(loops) = part.strip_prefix("loops=") {
            actual.loops = loops.parse().ok()?;
            seen_loops = true;
        }
    }
    (seen_rows && seen_loops).then_some(actual)
}

/// `Label: value` -> (label, value). The label must read like a property
/// name, so expressions that happen to hold a colon are not split.
fn label_of(text: &str) -> Option<(&str, &str)> {
    let (label, value) = match text.find(": ") {
        Some(at) => (&text[..at], text[at + 2..].trim()),
        None => (text.strip_suffix(':')?, ""),
    };
    let ok = !label.is_empty()
        && label.len() <= 48
        && label.starts_with(|c: char| c.is_ascii_alphabetic())
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '/' | '_' | '.'));
    ok.then_some((label, value))
}

fn is_subplan_header(text: &str) -> bool {
    (text.starts_with("InitPlan") || text.starts_with("SubPlan") || text.starts_with("CTE "))
        && !text.starts_with("CTE Scan")
        && !text.contains(": ")
}

/// Split on a separator that is outside parentheses, brackets and quotes.
fn split_top_level(text: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for c in text.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
                current.push(c);
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    current.push(c);
                }
                '(' | '[' | '{' => {
                    depth += 1;
                    current.push(c);
                }
                ')' | ']' | '}' => {
                    depth -= 1;
                    current.push(c);
                }
                c if c == separator && depth <= 0 => {
                    parts.push(current.trim().to_string());
                    current.clear();
                }
                c => current.push(c),
            },
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

/// `quicksort  Memory: 25kB` -> ["quicksort", "Memory: 25kB"].
fn split_groups(text: &str) -> Vec<&str> {
    text.split("  ")
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect()
}

/// Leading number of `25kB`, `131072 (originally 65536)`, `0.465 ms`.
fn leading_number(text: &str) -> Option<f64> {
    let end = text
        .find(|c: char| !(c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E')))
        .unwrap_or(text.len());
    text[..end]
        .trim_end_matches(['e', 'E'])
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
}

fn number_value(value: f64) -> Value {
    // Whole numbers stay integers, as the JSON format prints them.
    if value.fract() == 0.0 && value.abs() < 1e15 {
        return Value::from(value as i64);
    }
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn put_number(map: &mut Map<String, Value>, key: &str, text: &str) {
    if let Some(number) = leading_number(text) {
        map.insert(key.to_string(), number_value(number));
    }
}

fn title_case(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// `shared hit=1 read=3, temp written=5` -> `Shared Hit Blocks` etc.
fn put_buffers(map: &mut Map<String, Value>, value: &str) {
    for group in value.split(", ") {
        let mut scope = "Shared";
        for token in group.split_whitespace() {
            match token.split_once('=') {
                Some((name, number)) => {
                    put_number(map, &format!("{scope} {} Blocks", title_case(name)), number)
                }
                None => {
                    scope = match token {
                        "local" => "Local",
                        "temp" => "Temp",
                        _ => "Shared",
                    }
                }
            }
        }
    }
}

/// `shared read=1.2 write=0.3, temp read=0.1` (17+) or `read=1.2
/// write=0.3` (16 and older).
fn put_io_timings(map: &mut Map<String, Value>, value: &str) {
    for group in value.split(", ") {
        let mut scope = "";
        for token in group.split_whitespace() {
            match token.split_once('=') {
                Some((name, number)) => put_number(
                    map,
                    &format!("{scope}I/O {} Time", title_case(name)),
                    number,
                ),
                None => {
                    scope = match token {
                        "shared" => "Shared ",
                        "local" => "Local ",
                        "temp" => "Temp ",
                        _ => "",
                    }
                }
            }
        }
    }
}

fn put_sort_method(map: &mut Map<String, Value>, value: &str) {
    let groups = split_groups(value);
    if let Some(method) = groups.first() {
        map.insert("Sort Method".into(), Value::String((*method).to_string()));
    }
    for group in groups.iter().skip(1) {
        if let Some((kind, size)) = group.split_once(": ") {
            map.insert("Sort Space Type".into(), Value::String(kind.to_string()));
            put_number(map, "Sort Space Used", size);
        }
    }
}

/// `(originally 65536)` after a count.
fn originally(text: &str) -> Option<f64> {
    let at = text.find("(originally ")?;
    leading_number(&text[at + "(originally ".len()..])
}

/// Apply one `Label: value` line to a node (or worker) object.
fn put_property(map: &mut Map<String, Value>, label: &str, value: &str, is_hash_agg: bool) {
    match label {
        "Sort Key" | "Group Key" | "Presorted Key" | "Output" => {
            let mut items: Vec<Value> = split_top_level(value, ',')
                .into_iter()
                .map(Value::String)
                .collect();
            match map.get_mut(label) {
                Some(Value::Array(existing)) => existing.append(&mut items),
                _ => {
                    map.insert(label.to_string(), Value::Array(items));
                }
            }
        }
        "Sort Method" => put_sort_method(map, value),
        "Buffers" => put_buffers(map, value),
        "I/O Timings" => put_io_timings(map, value),
        "Heap Blocks" => {
            for token in value.split_whitespace() {
                if let Some((name, number)) = token.split_once('=') {
                    put_number(map, &format!("{} Heap Blocks", title_case(name)), number);
                }
            }
        }
        "WAL" => {
            for token in value.split_whitespace() {
                if let Some((name, number)) = token.split_once('=') {
                    let name = match name {
                        "records" => "WAL Records",
                        "fpi" => "WAL FPI",
                        "bytes" => "WAL Bytes",
                        "buffers" => "WAL Buffers Full",
                        _ => continue,
                    };
                    put_number(map, name, number);
                }
            }
        }
        // `Buckets: 131072  Batches: 1  Memory Usage: 4960kB` on Hash,
        // `Batches: 1  Memory Usage: 32kB  Disk Usage: 0kB` on HashAggregate,
        // `Hits: 2958  Misses: 41  Evictions: 0  Overflows: 0  Memory Usage: 5kB` on Memoize.
        "Buckets" | "Batches" | "Hits" | "Peak Memory Usage" | "Memory Usage" => {
            let line = format!("{label}: {value}");
            for group in split_groups(&line) {
                let Some((name, text)) = group.split_once(": ") else {
                    continue;
                };
                match name {
                    "Buckets" => {
                        put_number(map, "Hash Buckets", text);
                        if let Some(original) = originally(text) {
                            map.insert("Original Hash Buckets".into(), number_value(original));
                        }
                    }
                    "Batches" if is_hash_agg => put_number(map, "HashAgg Batches", text),
                    "Batches" => {
                        put_number(map, "Hash Batches", text);
                        if let Some(original) = originally(text) {
                            map.insert("Original Hash Batches".into(), number_value(original));
                        }
                    }
                    "Memory Usage" | "Peak Memory Usage" => {
                        put_number(map, "Peak Memory Usage", text)
                    }
                    "Disk Usage" => put_number(map, "Disk Usage", text),
                    "Hits" => put_number(map, "Cache Hits", text),
                    "Misses" => put_number(map, "Cache Misses", text),
                    "Evictions" => put_number(map, "Cache Evictions", text),
                    "Overflows" => put_number(map, "Cache Overflows", text),
                    other => {
                        map.insert(other.to_string(), Value::String(text.to_string()));
                    }
                }
            }
        }
        _ => {
            // A repeated label (several `Hash Key:` lines under grouping
            // sets) keeps every value.
            let merged = match map.get(label) {
                Some(Value::String(existing)) => format!("{existing}; {value}"),
                _ => value.to_string(),
            };
            map.insert(label.to_string(), Value::String(merged));
        }
    }
}

#[derive(Default)]
struct RawNode {
    map: Map<String, Value>,
    parent: Option<usize>,
    /// Column of the `->` arrow; `None` for the root.
    arrow: Option<usize>,
    workers: Vec<Map<String, Value>>,
    worker_summaries: Vec<String>,
    /// Indent of the open `Worker N:` line, while its sublines follow.
    worker_indent: Option<usize>,
    extra: Vec<Value>,
    children: usize,
}

#[derive(Default)]
struct RawPlan {
    nodes: Vec<RawNode>,
    /// Open ancestors, deepest last.
    stack: Vec<usize>,
    root_indent: usize,
    top: Map<String, Value>,
    in_trailer: bool,
    section: Option<String>,
    /// `InitPlan 2` / `SubPlan 4` / `CTE recent`, waiting for its node.
    pending_subplan: Option<String>,
    /// Node and label of the last property line, for wrapped values.
    last_property: Option<(usize, String, usize)>,
    triggers: Vec<Value>,
}

impl RawPlan {
    fn push_node(&mut self, text: &str, indent: usize, arrow: Option<usize>) {
        self.in_trailer = false;
        self.last_property = None;
        let parent = match arrow {
            None => None,
            Some(column) => {
                while let Some(&top) = self.stack.last() {
                    let top_column = self.nodes[top].arrow;
                    if top_column.is_some_and(|c| c >= column) {
                        self.stack.pop();
                    } else {
                        break;
                    }
                }
                self.stack.last().copied().or(Some(0))
            }
        };
        if arrow.is_none() {
            self.stack.clear();
        }
        let _ = indent;
        let mut node = RawNode {
            map: operator_line(text),
            parent,
            arrow,
            ..Default::default()
        };
        if let Some(parent) = parent {
            let parent_node = &mut self.nodes[parent];
            let relationship = match self.pending_subplan.take() {
                Some(name) => {
                    let kind = if name.starts_with("SubPlan") {
                        "SubPlan"
                    } else {
                        "InitPlan"
                    };
                    node.map.insert("Subplan Name".into(), Value::String(name));
                    kind
                }
                None => {
                    let parent_type = parent_node
                        .map
                        .get("Node Type")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let ordinal = parent_node.children;
                    parent_node.children += 1;
                    match parent_type {
                        "Append" | "Merge Append" | "BitmapAnd" | "BitmapOr" => "Member",
                        "Subquery Scan" => "Subquery",
                        _ if ordinal == 0 => "Outer",
                        _ => "Inner",
                    }
                }
            };
            node.map.insert(
                "Parent Relationship".into(),
                Value::String(relationship.to_string()),
            );
        }
        self.pending_subplan = None;
        self.nodes.push(node);
        self.stack.push(self.nodes.len() - 1);
    }

    /// A line under an operator: a property, a worker line, a subplan
    /// header, or the wrapped rest of the previous value.
    fn node_line(&mut self, text: &str, indent: usize) {
        // The node this line belongs to: the deepest open one it is indented under.
        let owner = self
            .stack
            .iter()
            .rev()
            .copied()
            .find(|&id| match self.nodes[id].arrow {
                Some(column) => column < indent,
                None => true,
            })
            .unwrap_or(0);
        if is_subplan_header(text) {
            self.pending_subplan = Some(text.to_string());
            self.last_property = None;
            self.nodes[owner].worker_indent = None;
            return;
        }
        let node = &mut self.nodes[owner];
        let is_hash_agg = node.map.get("Strategy").and_then(Value::as_str) == Some("Hashed");

        if let Some(worker_indent) = node.worker_indent {
            if indent > worker_indent {
                if let (Some(worker), Some((label, value))) =
                    (node.workers.last_mut(), label_of(text))
                {
                    put_property(worker, label, value, is_hash_agg);
                }
                if let Some(summary) = node.worker_summaries.last_mut() {
                    summary.push_str("; ");
                    summary.push_str(text);
                }
                return;
            }
            node.worker_indent = None;
        }

        let Some((label, value)) = label_of(text) else {
            // Not `Label: value`: the wrapped rest of the previous value
            // (a string literal holding a line break), else a bare note.
            if let Some((id, label, at)) = &self.last_property
                && *id == owner
                && indent >= *at
                && let Some(Value::String(existing)) = self.nodes[owner].map.get_mut(label)
            {
                existing.push(' ');
                existing.push_str(text);
                return;
            }
            self.nodes[owner]
                .extra
                .push(Value::String(text.to_string()));
            return;
        };

        if let Some(number) = label.strip_prefix("Worker ")
            && let Ok(number) = number.parse::<u32>()
        {
            let mut worker = Map::new();
            worker.insert("Worker Number".into(), Value::from(number));
            if let Some(actual) = pg_actual(value) {
                if let Some(total) = actual.total {
                    worker.insert("Actual Total Time".into(), number_value(total));
                }
                worker.insert("Actual Rows".into(), number_value(actual.rows));
                worker.insert("Actual Loops".into(), number_value(actual.loops));
            } else if let Some((label, value)) = label_of(value) {
                // 12 and older: `Worker 0:  Sort Method: quicksort  Memory: 25kB`.
                put_property(&mut worker, label, value, is_hash_agg);
            }
            node.workers.push(worker);
            node.worker_summaries.push(value.to_string());
            node.worker_indent = Some(indent);
            self.last_property = None;
            return;
        }

        put_property(&mut node.map, label, value, is_hash_agg);
        self.last_property = Some((owner, label.to_string(), indent));
    }

    /// A statement-level line at the left margin.
    fn trailer_line(&mut self, text: &str) {
        self.in_trailer = true;
        self.section = None;
        self.last_property = None;
        let Some((label, value)) = label_of(text) else {
            // `Trigger for constraint orders_customer_id_fkey: time=.. calls=..`
            // is caught by label_of; anything else is kept as a note.
            push_note(&mut self.top, text);
            return;
        };
        match label {
            "Planning Time" | "Planning time" => put_number(&mut self.top, "Planning Time", value),
            "Execution Time" | "Execution time" => {
                put_number(&mut self.top, "Execution Time", value)
            }
            "Total runtime" => put_number(&mut self.top, "Total Runtime", value),
            "Planning" | "JIT" => {
                self.section = Some(label.to_string());
                self.top
                    .entry(label.to_string())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            "Settings" => {
                let mut settings = Map::new();
                for item in split_top_level(value, ',') {
                    if let Some((name, setting)) = item.split_once(" = ") {
                        settings.insert(
                            name.trim().to_string(),
                            Value::String(setting.trim().trim_matches('\'').to_string()),
                        );
                    }
                }
                self.top.insert("Settings".into(), Value::Object(settings));
            }
            "Query Identifier" => {
                self.top
                    .insert("Query Identifier".into(), Value::String(value.to_string()));
            }
            "Query Text" => {
                self.top
                    .insert("Query Text".into(), Value::String(value.to_string()));
            }
            "Serialization" => {
                let mut serialization = Map::new();
                for token in value.split_whitespace() {
                    match token.split_once('=') {
                        Some(("time", number)) => put_number(&mut serialization, "Time", number),
                        Some(("output", number)) => {
                            put_number(&mut serialization, "Output Volume", number)
                        }
                        Some(("format", name)) => {
                            serialization.insert("Format".into(), Value::String(name.to_string()));
                        }
                        _ => {}
                    }
                }
                self.top
                    .insert("Serialization".into(), Value::Object(serialization));
            }
            _ if label.starts_with("Trigger ") => {
                let mut trigger = Map::new();
                let name = label.trim_start_matches("Trigger ");
                let (name, relation) = match name.split_once(" on ") {
                    Some((name, relation)) => (name, Some(relation)),
                    None => (name, None),
                };
                let name = name.trim_start_matches("for constraint ");
                trigger.insert("Trigger Name".into(), Value::String(name.to_string()));
                if let Some(relation) = relation {
                    trigger.insert("Relation".into(), Value::String(relation.to_string()));
                }
                for token in value.split_whitespace() {
                    match token.split_once('=') {
                        Some(("time", number)) => put_number(&mut trigger, "Time", number),
                        Some(("calls", number)) => put_number(&mut trigger, "Calls", number),
                        _ => {}
                    }
                }
                self.triggers.push(Value::Object(trigger));
            }
            _ => push_note(&mut self.top, text),
        }
    }

    /// An indented line under `Planning:` or `JIT:`.
    fn trailer_subline(&mut self, text: &str) {
        let Some(section) = self.section.clone() else {
            push_note(&mut self.top, text);
            return;
        };
        let Some(Value::Object(target)) = self.top.get_mut(&section) else {
            return;
        };
        let Some((label, value)) = label_of(text) else {
            return;
        };
        match (section.as_str(), label) {
            ("Planning", "Buffers") => put_buffers(target, value),
            ("Planning", "I/O Timings") => put_io_timings(target, value),
            ("Planning", "Memory") => {
                for token in value.split_whitespace() {
                    match token.split_once('=') {
                        Some(("used", number)) => put_number(target, "Memory Used", number),
                        Some(("allocated", number)) => {
                            put_number(target, "Memory Allocated", number)
                        }
                        _ => {}
                    }
                }
            }
            ("JIT", "Functions") => put_number(target, "Functions", value),
            ("JIT", "Timing") => {
                // `Generation 1.647 ms (Deform 0.569 ms), Inlining 130.801 ms, ..., Total 300.927 ms`
                let mut timing = Map::new();
                for item in split_top_level(value, ',') {
                    if let Some((name, number)) = item.split_once(' ') {
                        put_number(&mut timing, name, number);
                    }
                }
                target.insert("Timing".into(), Value::Object(timing));
            }
            _ => {
                target.insert(label.to_string(), Value::String(value.to_string()));
            }
        }
    }

    fn finish(mut self) -> Option<ExecutionPlan> {
        if self.nodes.is_empty() {
            return None;
        }
        if !self.triggers.is_empty() {
            self.top
                .insert("Triggers".into(), Value::Array(self.triggers));
        }
        let mut builder = PgPlanBuilder::default();
        let mut ids: Vec<i32> = Vec::with_capacity(self.nodes.len());
        for mut node in self.nodes {
            if !node.workers.is_empty() {
                let workers = node
                    .workers
                    .into_iter()
                    .zip(node.worker_summaries)
                    .map(|(mut worker, summary)| {
                        worker.insert("Summary".into(), Value::String(summary));
                        Value::Object(worker)
                    })
                    .collect();
                node.map.insert("Workers".into(), Value::Array(workers));
            }
            if !node.extra.is_empty() {
                node.map.insert(EXTRA_KEY.into(), Value::Array(node.extra));
            }
            let parent = node.parent.and_then(|parent| ids.get(parent).copied());
            ids.push(builder.push(&node.map, parent));
        }
        if builder.is_empty() {
            return None;
        }
        Some(builder.finish(&self.top, "postgresql-text"))
    }
}

fn push_note(top: &mut Map<String, Value>, text: &str) {
    match top
        .entry("Notes".to_string())
        .or_insert_with(|| Value::Array(Vec::new()))
    {
        Value::Array(notes) if notes.len() < 200 => notes.push(Value::String(text.to_string())),
        _ => {}
    }
}

/// One operator line without its arrow: name, relation and the statistics
/// groups, as the keys FORMAT JSON uses.
fn operator_line(text: &str) -> Map<String, Value> {
    let mut map = Map::new();
    let (name, stats) = match stats_start(text) {
        Some(at) => (text[..at].trim_end(), &text[at..]),
        None => (text.trim_end(), ""),
    };

    // Statistics groups, in whatever order they come.
    let mut rest = stats;
    while let Some(open) = rest.find('(') {
        let group = &rest[open..];
        if let Some((startup, total, rows, width)) = pg_cost(group) {
            map.insert("Startup Cost".into(), number_value(startup));
            map.insert("Total Cost".into(), number_value(total));
            map.insert("Plan Rows".into(), number_value(rows));
            if let Some(width) = width {
                map.insert("Plan Width".into(), number_value(width));
            }
        } else if let Some(actual) = pg_actual(group) {
            if let Some(startup) = actual.startup {
                map.insert("Actual Startup Time".into(), number_value(startup));
            }
            if let Some(total) = actual.total {
                map.insert("Actual Total Time".into(), number_value(total));
            }
            map.insert("Actual Rows".into(), number_value(actual.rows));
            map.insert("Actual Loops".into(), number_value(actual.loops));
        } else if group.starts_with("(never executed)") {
            map.insert("Actual Rows".into(), number_value(0.0));
            map.insert("Actual Loops".into(), number_value(0.0));
        }
        rest = &group[1..];
    }

    let mut name = name;
    // Greenplum: `Gather Motion 3:1  (slice1; segments: 3)`.
    if name.ends_with(')')
        && let Some(open) = name.rfind("  (")
    {
        let detail = name[open..].trim().trim_matches(['(', ')']);
        map.insert("Slice".into(), Value::String(detail.to_string()));
        name = name[..open].trim_end();
    }
    // Redshift prefixes every operator with XN.
    if let Some(rest) = name.strip_prefix("XN ") {
        name = rest;
    }
    if let Some(rest) = name.strip_prefix("Parallel ") {
        map.insert("Parallel Aware".into(), Value::Bool(true));
        name = rest;
    }
    for mode in ["Partial", "Finalize"] {
        if let Some(rest) = name.strip_prefix(mode).and_then(|r| r.strip_prefix(' ')) {
            map.insert("Partial Mode".into(), Value::String(mode.to_string()));
            name = rest;
        }
    }
    let put = |map: &mut Map<String, Value>, key: &str, value: &str| {
        map.insert(key.to_string(), Value::String(value.to_string()));
    };

    // `Insert on orders`, `Update on public.orders o`.
    for operation in ["Insert", "Update", "Delete", "Merge"] {
        if let Some(target) = name
            .strip_prefix(operation)
            .and_then(|r| r.strip_prefix(" on "))
        {
            put(&mut map, "Node Type", "ModifyTable");
            put(&mut map, "Operation", operation);
            put_relation(&mut map, target);
            return map;
        }
    }

    let (head, target) = match name.split_once(" on ") {
        Some((head, target)) if head.contains("Scan") => (head, Some(target)),
        _ => (name, None),
    };
    let (head, index) = match head.split_once(" using ") {
        Some((head, index)) => (head, Some(index)),
        None => (head, None),
    };
    let head = match head.strip_suffix(" Backward") {
        Some(head) => {
            put(&mut map, "Scan Direction", "Backward");
            head
        }
        None => head,
    };
    if let Some(index) = index {
        put(&mut map, "Index Name", index.trim());
    }

    let node_type: String = match head {
        "HashAggregate" => {
            put(&mut map, "Strategy", "Hashed");
            "Aggregate".into()
        }
        "GroupAggregate" => {
            put(&mut map, "Strategy", "Sorted");
            "Aggregate".into()
        }
        "MixedAggregate" => {
            put(&mut map, "Strategy", "Mixed");
            "Aggregate".into()
        }
        "Aggregate" => {
            put(&mut map, "Strategy", "Plain");
            "Aggregate".into()
        }
        "Nested Loop" | "Hash Join" | "Merge Join" => {
            put(&mut map, "Join Type", "Inner");
            head.into()
        }
        _ => {
            if let Some(command) = head.strip_prefix("HashSetOp ") {
                put(&mut map, "Strategy", "Hashed");
                put(&mut map, "Command", command);
                "SetOp".into()
            } else if let Some(command) = head.strip_prefix("SetOp ") {
                put(&mut map, "Strategy", "Sorted");
                put(&mut map, "Command", command);
                "SetOp".into()
            } else if let Some(kind) = join_kind(head, "Nested Loop ") {
                put(&mut map, "Join Type", kind);
                "Nested Loop".into()
            } else if let Some(kind) = join_kind(head, "Hash ") {
                put(&mut map, "Join Type", kind);
                "Hash Join".into()
            } else if let Some(kind) = join_kind(head, "Merge ") {
                put(&mut map, "Join Type", kind);
                "Merge Join".into()
            } else {
                head.to_string()
            }
        }
    };

    if let Some(target) = target {
        match node_type.as_str() {
            "Bitmap Index Scan" => put(&mut map, "Index Name", target.trim()),
            "CTE Scan" | "WorkTable Scan" => {
                let (cte, alias) = split_name_alias(target);
                put(&mut map, "CTE Name", &cte);
                if let Some(alias) = alias {
                    put(&mut map, "Alias", &alias);
                }
            }
            "Subquery Scan" | "Values Scan" | "Named Tuplestore Scan" => {
                put(&mut map, "Alias", target.trim())
            }
            "Function Scan" | "Table Function Scan" => {
                let (function, alias) = split_name_alias(target);
                put(&mut map, "Function Name", &function);
                if let Some(alias) = alias {
                    put(&mut map, "Alias", &alias);
                }
            }
            _ => put_relation(&mut map, target),
        }
    }
    put(&mut map, "Node Type", &node_type);
    map
}

/// `Left`, `Right Anti`... from `Hash Left Join` given the `Hash ` prefix.
fn join_kind<'a>(head: &'a str, method: &str) -> Option<&'a str> {
    let kind = head.strip_prefix(method)?.strip_suffix(" Join")?;
    matches!(
        kind,
        "Left" | "Right" | "Full" | "Semi" | "Anti" | "Right Semi" | "Right Anti"
    )
    .then_some(kind)
}

/// `public.orders o` / `"My Table" t` / `orders` -> name and alias.
fn split_name_alias(target: &str) -> (String, Option<String>) {
    let target = target.trim();
    let mut in_quote = false;
    for (at, c) in target.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            ' ' if !in_quote => {
                let alias = target[at + 1..].trim();
                return (
                    target[..at].to_string(),
                    (!alias.is_empty()).then(|| alias.to_string()),
                );
            }
            _ => {}
        }
    }
    (target.to_string(), None)
}

fn unquote(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => name.to_string(),
    }
}

fn put_relation(map: &mut Map<String, Value>, target: &str) {
    let (name, alias) = split_name_alias(target);
    // Split `schema.table` at a dot outside quotes.
    let mut in_quote = false;
    let mut dot = None;
    for (at, c) in name.char_indices() {
        match c {
            '"' => in_quote = !in_quote,
            '.' if !in_quote => {
                dot = Some(at);
                break;
            }
            _ => {}
        }
    }
    let (schema, relation) = match dot {
        Some(at) => (Some(&name[..at]), &name[at + 1..]),
        None => (None, name.as_str()),
    };
    if let Some(schema) = schema {
        map.insert("Schema".into(), Value::String(unquote(schema)));
    }
    map.insert("Relation Name".into(), Value::String(unquote(relation)));
    if let Some(alias) = alias {
        map.insert("Alias".into(), Value::String(unquote(&alias)));
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::assert_survives_mangling;
    use super::super::pg::parse_pg_explain_json_multi;
    use super::super::pg_structured::{parse_pg_explain_xml, parse_pg_explain_yaml};
    use super::*;
    use crate::plans::model::PlanNode;

    macro_rules! fx {
        ($name:literal) => {
            include_str!(concat!("../../../tests/golden/plans/postgresql/", $name))
        };
    }

    fn one(doc: &str) -> ExecutionPlan {
        let mut plans = parse_pg_explain_text(doc).expect("text plan parses");
        assert_eq!(plans.len(), 1);
        plans.remove(0)
    }

    fn json(doc: &str) -> ExecutionPlan {
        // The captures are psql output; JSON ignores the decoration once
        // the cell is unwrapped.
        let doc = super::super::pg_structured::unwrap_psql_cell(doc);
        parse_pg_explain_json_multi(&doc)
            .expect("json plan parses")
            .remove(0)
    }

    fn find<'a>(plan: &'a ExecutionPlan, op: &str) -> &'a PlanNode {
        plan.nodes
            .iter()
            .find(|node| node.logical_op == op)
            .unwrap_or_else(|| panic!("no {op} node"))
    }

    fn parent_of(plan: &ExecutionPlan, id: i32) -> Option<i32> {
        plan.edges
            .iter()
            .find(|edge| edge.to_node == id)
            .map(|edge| edge.from_node)
    }

    /// Everything that does not vary between two runs of the same query.
    fn shape(plan: &ExecutionPlan) -> Vec<String> {
        plan.nodes
            .iter()
            .map(|node| {
                format!(
                    "{:?}<-{} {} | {} | rel={:?} schema={:?} alias={:?} idx={:?} est={} cost={} startup={:?} width={:?} rows={:?} loops={:?} filter={:?} cond={:?} join={:?} jt={:?} sort={:?} group={:?} method={:?} removed={:?} batches={:?} buckets={:?} planned={:?} launched={:?} sub={:?} cte={:?} rel={:?} never={} par={} spill={:?} out={:?}",
                    parent_of(plan, node.id),
                    node.id,
                    node.logical_op,
                    node.physical_op,
                    node.relation,
                    node.schema,
                    node.alias,
                    node.index_name,
                    node.estimated_rows,
                    node.estimated_cost,
                    node.estimated_startup_cost,
                    node.estimated_row_width,
                    node.actual_rows,
                    node.loops,
                    node.filter,
                    node.index_cond,
                    node.join_predicate,
                    node.join_type,
                    node.sort_keys,
                    node.group_keys,
                    node.sort_method,
                    // JSON prints a zero here, text leaves the line out.
                    node.rows_removed_by_filter.filter(|rows| *rows > 0.0),
                    node.hash_batches,
                    node.hash_buckets,
                    node.workers_planned,
                    node.workers_launched,
                    node.subplan_name,
                    node.cte_name,
                    node.parent_relationship,
                    node.never_executed,
                    node.parallel,
                    node.spill.as_ref().map(|spill| spill.kind.clone()),
                    node.output_list,
                )
            })
            .collect()
    }

    /// The text parser must reach the same model as the JSON of the same
    /// statement on the same server.
    #[test]
    fn text_and_json_of_the_same_statement_agree() {
        for (name, text, json_doc) in [
            (
                "join-estimated",
                fx!("join-estimated.txt"),
                fx!("join-estimated.json"),
            ),
            // The JSON capture of this statement is the VERBOSE one.
            (
                "join-analyze",
                fx!("join-analyze-verbose.txt"),
                fx!("join-analyze.json"),
            ),
            ("sort-disk", fx!("sort-disk.txt"), fx!("sort-disk.json")),
            (
                "hash-batches",
                fx!("hash-batches.txt"),
                fx!("hash-batches.json"),
            ),
            (
                "nested-loop",
                fx!("nested-loop.txt"),
                fx!("nested-loop.json"),
            ),
            (
                "nested-loop-unaligned",
                fx!("nested-loop-unaligned.txt"),
                fx!("nested-loop-unaligned.json"),
            ),
            (
                "cte-subplan",
                fx!("cte-subplan.txt"),
                fx!("cte-subplan.json"),
            ),
            (
                "never-executed",
                fx!("never-executed.txt"),
                fx!("never-executed.json"),
            ),
            ("bitmap", fx!("bitmap.txt"), fx!("bitmap.json")),
            (
                "update-trigger",
                fx!("update-trigger.txt"),
                fx!("update-trigger.json"),
            ),
            (
                "jit-analyze",
                fx!("jit-analyze.txt"),
                fx!("jit-analyze.json"),
            ),
        ] {
            let from_text = one(text);
            let from_json = json(json_doc);
            let (a, b) = (shape(&from_text), shape(&from_json));
            assert_eq!(a.len(), b.len(), "{name}: node count");
            for (left, right) in a.iter().zip(&b) {
                assert_eq!(left, right, "{name}");
            }
            assert_eq!(from_text.has_actuals, from_json.has_actuals, "{name}");
            assert_eq!(
                from_text.execution_time_ms.is_some(),
                from_json.execution_time_ms.is_some(),
                "{name}"
            );
            assert_eq!(from_text.triggers.len(), from_json.triggers.len(), "{name}");
            assert_eq!(from_text.jit_functions, from_json.jit_functions, "{name}");
        }
    }

    #[test]
    fn yaml_and_xml_agree_with_json() {
        for (name, yaml, xml, json_doc) in [
            (
                "join-estimated",
                fx!("join-estimated.yaml"),
                fx!("join-estimated.xml"),
                fx!("join-estimated.json"),
            ),
            (
                "join-analyze",
                fx!("join-analyze.yaml"),
                fx!("join-analyze.xml"),
                fx!("join-analyze.json"),
            ),
        ] {
            // The YAML and XML captures of the ANALYZE statement are not
            // VERBOSE while its JSON capture is, so those compare with text.
            let from_json = if name == "join-analyze" {
                shape(&one(fx!("join-analyze.txt")))
            } else {
                shape(&json(json_doc))
            };
            let from_yaml = parse_pg_explain_yaml(yaml).expect("yaml parses").remove(0);
            let from_xml = parse_pg_explain_xml(xml).expect("xml parses").remove(0);
            assert_eq!(shape(&from_yaml), from_json, "{name} yaml");
            assert_eq!(shape(&from_xml), from_json, "{name} xml");
            assert_eq!(from_yaml.source_format.as_deref(), Some("postgresql-yaml"));
            assert_eq!(from_xml.source_format.as_deref(), Some("postgresql-xml"));
            if name == "join-analyze" {
                assert!(from_yaml.execution_time_ms.is_some());
                assert!(from_xml.planning_time_ms.is_some());
                assert!(from_xml.nodes[0].buffers.is_some());
            }
        }
    }

    #[test]
    fn analyze_buffers_and_statement_lines_are_read() {
        let plan = one(fx!("join-analyze-verbose.txt"));
        assert!(plan.has_actuals);
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert_eq!(plan.planning_time_ms, Some(0.465));
        assert_eq!(plan.execution_time_ms, Some(102.102));
        assert_eq!(plan.settings.len(), 2);
        assert_eq!(plan.settings[0].name, "jit");
        assert_eq!(plan.settings[0].value, "off");
        assert_eq!(plan.settings[1].value, "8MB");
        assert!(
            plan.notes
                .iter()
                .any(|note| note == "Planning buffers: shared hit=371")
        );

        let root = &plan.nodes[0];
        assert_eq!(root.logical_op, "Sort");
        assert_eq!(root.sort_method.as_deref(), Some("quicksort"));
        assert_eq!(root.sort_space_kb, Some(25.0));
        assert_eq!(root.sort_space_type.as_deref(), Some("Memory"));
        assert_eq!(root.buffers.as_ref().unwrap().shared_hit, Some(13415.0));
        assert_eq!(root.output_columns.len(), 3);
        assert_eq!(root.actual_time_ms, Some(102.001));

        let gather = find(&plan, "Gather Merge");
        assert_eq!(gather.workers_planned, Some(2));
        assert_eq!(gather.workers_launched, Some(2));

        // Three processes each took about 12.5 ms: wall clock, not 37.6 ms.
        let scan = find(&plan, "Parallel Seq Scan");
        assert_eq!(scan.physical_op, "Seq Scan");
        assert!(scan.parallel);
        assert_eq!(scan.schema.as_deref(), Some("public"));
        assert_eq!(scan.relation.as_deref(), Some("order_items"));
        assert_eq!(scan.alias.as_deref(), Some("oi"));
        assert_eq!(scan.loops, Some(3.0));
        assert_eq!(scan.actual_rows, Some(400000.0));
        assert_eq!(scan.actual_rows_total, Some(1200000.0));
        assert_eq!(scan.actual_time_ms, Some(12.548));
        assert_eq!(scan.thread_rows, vec![396648.0, 398344.0]);
        assert!(scan.property("Worker 0").is_some());

        let agg = find(&plan, "Partial HashAggregate");
        assert_eq!(agg.physical_op, "Aggregate");
        assert_eq!(agg.group_keys, vec!["c.country"]);
        assert_eq!(agg.peak_memory_kb, Some(32.0));
        assert!(agg.spill.is_none());

        let filtered = plan
            .nodes
            .iter()
            .find(|node| node.relation.as_deref() == Some("orders"))
            .unwrap();
        assert_eq!(
            filtered.filter.as_deref(),
            Some("(o.status = 'paid'::text)")
        );
        // 100000 per loop, three loops.
        assert_eq!(filtered.rows_removed_by_filter, Some(300000.0));
    }

    #[test]
    fn spills_subplans_triggers_and_pruning_are_read() {
        let sort = one(fx!("sort-disk.txt"));
        let spill = sort.nodes[0].spill.as_ref().expect("sort spilled");
        assert_eq!(spill.kind, "sort");
        assert_eq!(spill.written_kb, Some(16128.0));
        assert_eq!(sort.nodes[0].sort_keys, vec!["total", "placed_at"]);
        assert_eq!(
            sort.nodes[0].buffers.as_ref().unwrap().temp_written,
            Some(8526.0)
        );

        let hash = one(fx!("hash-batches.txt"));
        let build = find(&hash, "Hash");
        assert_eq!(build.hash_batches, Some(32));
        assert_eq!(build.hash_buckets, Some(32768.0));
        assert_eq!(build.spill.as_ref().unwrap().kind, "hash");
        assert_eq!(
            find(&hash, "Hash Join").join_predicate.as_deref(),
            Some("(oi.order_id = o.id)")
        );

        let cte = one(fx!("cte-subplan.txt"));
        let recent = cte
            .nodes
            .iter()
            .find(|node| node.subplan_name.as_deref() == Some("CTE recent"))
            .unwrap();
        assert_eq!(recent.cte_name.as_deref(), Some("recent"));
        assert_eq!(recent.parent_relationship.as_deref(), Some("InitPlan"));
        assert_eq!(parent_of(&cte, recent.id), Some(1));
        let scan = find(&cte, "CTE Scan");
        assert_eq!(scan.cte_name.as_deref(), Some("recent"));
        assert_eq!(scan.alias.as_deref(), Some("r"));
        assert_eq!(parent_of(&cte, scan.id), Some(1));
        let subplan = cte
            .nodes
            .iter()
            .find(|node| node.subplan_name.as_deref() == Some("SubPlan 4"))
            .unwrap();
        assert_eq!(subplan.parent_relationship.as_deref(), Some("SubPlan"));
        assert_eq!(parent_of(&cte, subplan.id), Some(scan.id));
        assert_eq!(subplan.rows_removed_by_filter, Some(960000.0));
        // Self time never goes negative and never exceeds the inclusive time.
        for node in &cte.nodes {
            if let (Some(own), Some(total)) = (node.actual_self_time_ms, node.actual_time_ms) {
                assert!(own >= 0.0 && own <= total + 1e-9, "{}", node.logical_op);
            }
        }

        let update = one(fx!("update-trigger.txt"));
        assert_eq!(update.nodes[0].logical_op, "Update");
        assert_eq!(update.nodes[0].physical_op, "ModifyTable");
        assert_eq!(update.nodes[0].relation.as_deref(), Some("orders"));
        assert_eq!(update.nodes[0].property("WAL Records"), Some("1004"));
        assert_eq!(update.triggers.len(), 1);
        assert_eq!(update.triggers[0].name, "orders_audit");
        assert_eq!(update.triggers[0].time_ms, Some(2.005));
        assert_eq!(update.triggers[0].calls, Some(199.0));
        let scan = find(&update, "Index Scan");
        assert_eq!(scan.index_name.as_deref(), Some("orders_pkey"));
        assert_eq!(scan.index_cond.as_deref(), Some("(id < 200)"));

        let pruned = one(fx!("pruned-generic.txt"));
        let append = find(&pruned, "Append");
        assert_eq!(append.partitions_scanned, Some(1));
        assert_eq!(append.partitions_total, Some(2));

        let never = one(fx!("never-executed.txt"));
        assert!(never.nodes.iter().any(|node| node.never_executed));

        let jit = one(fx!("jit-analyze.txt"));
        assert_eq!(jit.jit_functions, Some(87));
        assert_eq!(jit.jit_time_ms, Some(300.927));

        let serialize = one(fx!("serialize-memory.txt"));
        assert_eq!(serialize.nodes[0].logical_op, "Index Only Scan");
        assert_eq!(serialize.nodes[0].heap_fetches, Some(1.0));
        assert!(
            serialize
                .notes
                .iter()
                .any(|note| note.starts_with("Serialization: time=0.03"))
        );
        assert!(
            serialize
                .notes
                .iter()
                .any(|note| note == "Planning memory used: 11 kB")
        );

        let bitmap = one(fx!("bitmap.txt"));
        assert_eq!(bitmap.nodes[0].exact_heap_blocks, Some(35.0));
        assert_eq!(find(&bitmap, "BitmapOr").actual_rows, Some(0.0));
        let index = find(&bitmap, "Bitmap Index Scan");
        assert_eq!(index.index_name.as_deref(), Some("orders_customer_idx"));
    }

    #[test]
    fn loops_scale_rows_and_time() {
        let plan = one(fx!("nested-loop-unaligned.txt"));
        let inner = plan
            .nodes
            .iter()
            .find(|node| node.relation.as_deref() == Some("orders"))
            .unwrap();
        assert_eq!(inner.loops, Some(41.0));
        assert_eq!(inner.actual_rows, Some(3.61));
        assert!((inner.actual_rows_total.unwrap() - 148.01).abs() < 0.01);
        assert!((inner.actual_time_ms.unwrap() - 0.861).abs() < 1e-9);
        assert_eq!(inner.rows_removed_by_filter, Some(164.0));
        assert_eq!(inner.property("Index Searches"), Some("41"));
    }

    #[test]
    fn older_and_related_layouts_parse() {
        // PostgreSQL 9.x: lower-case "Planning time", "Total runtime" before 9.4.
        let old = "\
 Hash Left Join  (cost=1.09..2.25 rows=5 width=8) (actual time=0.030..0.035 rows=5 loops=1)
   Hash Cond: (a.id = b.id)
   ->  Seq Scan on a  (cost=0.00..1.05 rows=5 width=4) (actual time=0.004..0.005 rows=5 loops=1)
   ->  Hash  (cost=1.04..1.04 rows=4 width=4) (actual time=0.010..0.010 rows=4 loops=1)
         Buckets: 1024  Batches: 1  Memory Usage: 9kB
         ->  Seq Scan on b  (cost=0.00..1.04 rows=4 width=4) (never executed)
 Total runtime: 0.083 ms
";
        let plan = one(old);
        assert_eq!(plan.nodes[0].logical_op, "Hash Left Join");
        assert_eq!(plan.nodes[0].physical_op, "Hash Join");
        assert_eq!(plan.nodes[0].join_type.as_deref(), Some("Left"));
        assert_eq!(plan.execution_time_ms, Some(0.083));
        assert!(plan.nodes[3].never_executed);
        assert_eq!(plan.nodes[2].peak_memory_kb, Some(9.0));

        // Redshift prefixes operators with XN and names the distribution.
        let redshift = "\
XN Hash Join DS_DIST_NONE  (cost=0.04..0.14 rows=3 width=8)
  Hash Cond: (\"outer\".id = \"inner\".id)
  ->  XN Seq Scan on sales  (cost=0.00..0.03 rows=3 width=4)
  ->  XN Hash  (cost=0.03..0.03 rows=3 width=4)
        ->  XN Seq Scan on listing  (cost=0.00..0.03 rows=3 width=4)
";
        let plan = one(redshift);
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.nodes[1].relation.as_deref(), Some("sales"));
        assert_eq!(parent_of(&plan, 4), Some(3));

        // COSTS OFF: no numbers, the first line is the root.
        let costs_off = "\
 Limit
   ->  Sort
         Sort Key: total DESC
         ->  Seq Scan on orders
               Filter: (total > '100'::numeric)
";
        assert!(looks_like_pg_explain_text_without_costs(costs_off));
        assert!(!looks_like_pg_explain_text(costs_off));
        let plan = one(costs_off);
        assert_eq!(plan.nodes.len(), 3);
        assert!(!plan.has_actuals);
        assert_eq!(
            plan.nodes[2].filter.as_deref(),
            Some("(total > '100'::numeric)")
        );

        // pgAdmin copies each line as a quoted CSV cell.
        let quoted = "\"QUERY PLAN\"\n\"Seq Scan on orders  (cost=0.00..7054.00 rows=400000 width=27)\"\n\"  Filter: (status = \"\"x\"\")\"\n";
        let plan = one(quoted);
        assert_eq!(plan.nodes[0].filter.as_deref(), Some("(status = \"x\")"));

        // auto_explain log entry.
        let logged = "LOG:  duration: 3.651 ms  plan:\n\tQuery Text: SELECT count(*) FROM orders;\n\tAggregate  (cost=8054.00..8054.01 rows=1 width=8)\n\t  ->  Seq Scan on orders  (cost=0.00..7054.00 rows=400000 width=0)\n";
        let plan = one(logged);
        assert_eq!(plan.statement_text, "SELECT count(*) FROM orders;");
        assert_eq!(plan.nodes.len(), 2);
    }

    #[test]
    fn two_statements_give_two_plans() {
        let doc = "\
 Insert on a  (cost=0.00..0.01 rows=0 width=0)
   ->  Result  (cost=0.00..0.01 rows=1 width=4)

 Insert on b  (cost=0.00..0.01 rows=0 width=0)
   ->  Result  (cost=0.00..0.01 rows=1 width=4)
";
        let plans = parse_pg_explain_text(doc).unwrap();
        assert_eq!(plans.len(), 2);
        assert_eq!(plans[1].nodes[0].relation.as_deref(), Some("b"));
    }

    #[test]
    fn detection_accepts_postgres_and_rejects_other_engines() {
        for doc in [
            fx!("join-estimated.txt"),
            fx!("join-analyze-verbose.txt"),
            fx!("nested-loop-unaligned.txt"),
            fx!("cte-subplan-estimated.txt"),
            fx!("partition.txt"),
            fx!("window.txt"),
            fx!("memoize.txt"),
            fx!("index-only.txt"),
            fx!("join-analyze-buffers.txt"),
        ] {
            assert!(looks_like_pg_explain_text(doc));
            assert!(parse_pg_explain_text(doc).is_ok());
        }
        // MySQL's tree format: arrow on the root, single cost, one space.
        let mysql = "-> Nested loop inner join  (cost=1.2 rows=3) (actual time=0.1..0.2 rows=3 loops=1)\n    -> Table scan on a  (cost=0.5 rows=3) (actual time=0.05..0.06 rows=3 loops=1)\n";
        assert!(!looks_like_pg_explain_text(mysql));
        assert!(!looks_like_pg_explain_text_without_costs(mysql));
        for doc in [
            fx!("join-analyze.json"),
            fx!("join-analyze.yaml"),
            fx!("join-analyze.xml"),
            "SELECT * FROM orders WHERE total > 5",
            "",
            "Limit\n",
        ] {
            assert!(!looks_like_pg_explain_text(doc));
            assert!(!looks_like_pg_explain_text_without_costs(doc));
        }
        assert!(parse_pg_explain_text("nothing here").is_err());
    }

    #[test]
    fn mangled_documents_never_panic() {
        for doc in [
            fx!("join-analyze-verbose.txt"),
            fx!("cte-subplan.txt"),
            fx!("jit-analyze.txt"),
            fx!("update-trigger.txt"),
        ] {
            assert_survives_mangling(doc, |doc| {
                let _ = parse_pg_explain_text(doc);
                let _ = looks_like_pg_explain_text(doc);
                let _ = looks_like_pg_explain_text_without_costs(doc);
            });
        }
        for doc in [
            fx!("join-analyze.yaml"),
            fx!("join-analyze.xml"),
            fx!("jit-analyze.json"),
        ] {
            assert_survives_mangling(doc, |doc| {
                let _ = parse_pg_explain_yaml(doc);
                let _ = parse_pg_explain_xml(doc);
                let _ = parse_pg_explain_json_multi(doc);
            });
        }
    }
}
