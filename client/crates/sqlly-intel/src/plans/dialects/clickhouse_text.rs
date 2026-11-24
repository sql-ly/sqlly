//! ClickHouse: the text of `EXPLAIN` (`EXPLAIN PLAN`) and `EXPLAIN PIPELINE`.
//!
//! `EXPLAIN` prints one step per line. Two layouts exist:
//!
//! - Up to the 25.x releases a child step is indented two spaces under its
//!   parent, and a step's detail lines (`indexes = 1`, `actions = 1`,
//!   `header = 1`) sit at the same indent as the step itself:
//!
//!   ```text
//!   Expression ((Project names + Projection))
//!     Aggregating
//!     Keys: __table1.status
//!       ReadFromMergeTree (default.orders)
//!       Indexes:
//!         PrimaryKey
//!           Condition: (customer_id in [100, +Inf))
//!           Parts: 10/10
//!           Granules: 10/389
//!   ```
//!
//! - Newer servers (seen on 26.9) draw the tree with `├──` / `└──`, hang
//!   details off a `│` rail, start with an `Output:` line, and add planner
//!   estimates to joins (`Output rows: estimated 36.79 thousand`).
//!
//! `EXPLAIN PIPELINE` prints the processors that execute the plan: a
//! `(Step)` line naming the plan step, then its processors, each optionally
//! followed by `× N` (parallel streams) and `A → B` (input and output
//! ports).
//!
//! Both are accepted bare, with a TSV `explain` header, inside the box
//! `clickhouse-client` draws in its Pretty formats, and in `FORMAT Vertical`.
//! `EXPLAIN ESTIMATE`, `SYNTAX`, `AST`, `QUERY TREE` and
//! `EXPLAIN PIPELINE graph = 1` are recognised and refused with a message
//! saying what to run instead.

use super::clickhouse::{
    IndexInfo, MAX_DEPTH, Step, finish_clickhouse_plan, parse_readable_quantity, split_top_level,
    step_node,
};
use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};

/// Parse the text of a ClickHouse `EXPLAIN` (query plan) in either layout.
pub fn parse_clickhouse_explain_text(doc: &str) -> PlanResult<ExecutionPlan> {
    if let Some(reason) = refused_shape(doc) {
        return Err(PlanError::InvalidStructure(reason.into()));
    }
    let lines = clean(doc);
    if pipeline_start(&lines).is_some() {
        return pipeline_from_lines(&lines);
    }
    let (output, steps) = read_steps(&lines)?;
    if steps.is_empty() {
        return Err(PlanError::InvalidStructure(
            "ClickHouse EXPLAIN: no plan steps found".into(),
        ));
    }
    let mut nodes = Vec::with_capacity(steps.len());
    let mut edges = Vec::new();
    for (index, raw) in steps.into_iter().enumerate() {
        let id = index as i32 + 1;
        if let Some(parent) = raw.parent {
            edges.push(PlanEdge {
                from_node: parent as i32 + 1,
                to_node: id,
            });
        }
        let mut step = Step {
            name: raw.name,
            description: raw.description,
            ..Default::default()
        };
        read_details(&mut step, &raw.details, 0);
        nodes.push(step_node(id, step, false));
    }
    // The leading `Output:` line of the newer layout names the query's
    // result columns, which is what the root step returns.
    if let (Some(root), Some(output)) = (nodes.first_mut(), output)
        && root.output_columns.is_empty()
    {
        root.output_columns = split_top_level(&output);
    }
    let mut plan = ExecutionPlan {
        statement_type: "EXPLAIN".to_string(),
        nodes,
        edges,
        source_format: Some("clickhouse-text".to_string()),
        ..Default::default()
    };
    finish_clickhouse_plan(&mut plan);
    Ok(plan)
}

/// True for the text of a ClickHouse `EXPLAIN` plan: the first line (after
/// the optional `Output:` line) is a query-plan step such as
/// `Expression ((Project names + Projection))`, and nothing in the document
/// is PostgreSQL's `->` child marker.
pub fn looks_like_clickhouse_explain_text(doc: &str) -> bool {
    if refused_shape(doc).is_some() {
        return false;
    }
    let lines = clean(doc);
    let mut content = lines.iter().filter(|line| !line.trim().is_empty());
    let Some(mut first) = content.next() else {
        return false;
    };
    if first.starts_with("Output:") {
        match content.next() {
            Some(next) => first = next,
            None => return false,
        }
    }
    let Some((name, description)) = split_step(first) else {
        return false;
    };
    if !is_known_step(name) {
        return false;
    }
    // `Limit (actual time=0.01..0.02 rows=5 loops=1)` is PostgreSQL.
    if description.is_some_and(|d| d.contains("rows=") || d.contains("cost=")) {
        return false;
    }
    !lines.iter().any(|line| line.trim_start().starts_with("->"))
}

/// Parse the text of a ClickHouse `EXPLAIN PIPELINE`. Each processor is a
/// node; the plan step it belongs to is its `Step` property.
pub fn parse_clickhouse_explain_pipeline(doc: &str) -> PlanResult<ExecutionPlan> {
    if let Some(reason) = refused_shape(doc) {
        return Err(PlanError::InvalidStructure(reason.into()));
    }
    pipeline_from_lines(&clean(doc))
}

/// True for `EXPLAIN PIPELINE` text: it opens with a `(Step)` line followed
/// by a processor line at the same indent.
pub fn looks_like_clickhouse_explain_pipeline(doc: &str) -> bool {
    refused_shape(doc).is_none() && pipeline_start(&clean(doc)).is_some()
}

/// Query-plan step names that can stand at the root of a plan.
const KNOWN_STEPS: [&str; 26] = [
    "Expression",
    "Filter",
    "Aggregating",
    "MergingAggregated",
    "Sorting",
    "Limit",
    "LimitBy",
    "Offset",
    "Join",
    "JoinLogical",
    "FilledJoin",
    "ArrayJoin",
    "Union",
    "Distinct",
    "Window",
    "CreatingSets",
    "CreatingSet",
    "DelayedCreatingSets",
    "Extremes",
    "Rollup",
    "Cube",
    "TotalsHaving",
    "Filling",
    "IntersectOrExcept",
    "BuildRuntimeFilter",
    "ReadNothing",
];

fn is_known_step(name: &str) -> bool {
    name.starts_with("ReadFrom") || KNOWN_STEPS.contains(&name)
}

/// The other things ClickHouse's `EXPLAIN` family prints, which are not
/// plans. Returns the message to show.
fn refused_shape(doc: &str) -> Option<&'static str> {
    if doc.contains("digraph") && doc.contains("subgraph cluster_") {
        return Some(
            "This is ClickHouse's EXPLAIN PIPELINE graph = 1 output, a Graphviz drawing. Run \
             EXPLAIN PIPELINE without graph = 1, or EXPLAIN indexes = 1, and paste that.",
        );
    }
    if doc.contains("\u{2500}parts\u{2500}") && doc.contains("\u{2500}marks\u{2500}") {
        return Some(ESTIMATE_MESSAGE);
    }
    let lines: Vec<&str> = doc
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .take(64)
        .collect();
    let first = lines.first()?.trim_start();
    if first.starts_with("QUERY id:") {
        return Some(
            "This is ClickHouse's EXPLAIN QUERY TREE output, which shows the analysed query \
             rather than how it runs. Run EXPLAIN indexes = 1 (optionally json = 1) and paste \
             that.",
        );
    }
    if first.contains("(children ") || first.starts_with("SelectWithUnionQuery") {
        return Some(
            "This is ClickHouse's EXPLAIN AST output, the parsed syntax tree rather than a query \
             plan. Run EXPLAIN indexes = 1 (optionally json = 1) and paste that.",
        );
    }
    let is_estimate_row = |line: &&str| {
        let fields: Vec<&str> = line.split('\t').collect();
        fields.len() == 5
            && fields
                .iter()
                .skip(2)
                .all(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
    };
    let header = first == "database\ttable\tparts\trows\tmarks";
    if lines.iter().skip(usize::from(header)).all(is_estimate_row)
        && (header || !lines.is_empty())
        && lines.len() > usize::from(header)
    {
        return Some(ESTIMATE_MESSAGE);
    }
    let keyword = first
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or("");
    if matches!(
        keyword,
        "SELECT" | "WITH" | "INSERT" | "CREATE" | "ALTER" | "DELETE" | "UPDATE"
    ) {
        return Some(
            "This is SQL text (the output of ClickHouse's EXPLAIN SYNTAX), not a query plan. \
             Run EXPLAIN indexes = 1 (optionally json = 1) and paste that.",
        );
    }
    None
}

const ESTIMATE_MESSAGE: &str = "This is ClickHouse's EXPLAIN ESTIMATE output: one row per table \
    with the parts, rows and marks to read, but no plan steps. Run EXPLAIN indexes = 1 \
    (optionally json = 1) and paste that.";

const FRAME: char = '\u{2502}';

/// Undo client decoration: the Pretty formats' box and row numbers,
/// `FORMAT Vertical`, a TSV header line and TSV quote escaping. Returns the
/// plan's own lines, right-trimmed.
fn clean(doc: &str) -> Vec<String> {
    let framed = doc
        .lines()
        .any(|line| line.trim_start().starts_with("\u{250c}\u{2500}"));
    let vertical = !framed
        && doc.lines().any(|line| line.starts_with("Row 1:"))
        && doc.lines().any(|line| line.starts_with("explain:"));
    let escaped = doc.contains("\\'");
    let mut lines = Vec::new();
    for line in doc.lines().take(200_000) {
        let line = line.trim_end();
        let text = if framed {
            let Some(inner) = unframe(line) else {
                continue;
            };
            inner
        } else if vertical {
            let Some(rest) = line.strip_prefix("explain:") else {
                continue;
            };
            rest.strip_prefix(' ').unwrap_or(rest)
        } else {
            line
        };
        if lines.is_empty() && text.trim() == "explain" {
            continue;
        }
        // `12 rows in set. Elapsed: 0.003 sec.`
        if text.contains(" in set. Elapsed:") {
            continue;
        }
        lines.push(if escaped {
            text.replace("\\'", "'").replace("\\\\", "\\")
        } else {
            text.to_string()
        });
    }
    lines
}

/// ` 3. │ Aggregating        │` gives `Aggregating`. Borders give `None`.
fn unframe(line: &str) -> Option<&str> {
    let (before, rest) = line.split_once(FRAME)?;
    if !before
        .chars()
        .all(|c| c.is_whitespace() || c.is_ascii_digit() || c == '.')
    {
        return None;
    }
    let inner = rest.strip_suffix(FRAME)?;
    Some(inner.strip_prefix(' ').unwrap_or(inner).trim_end())
}

/// `Expression ((Project names + Projection))` gives the step name and its
/// description; `Aggregating` gives the name alone. Anything else is not a
/// step line.
fn split_step(text: &str) -> Option<(&str, Option<&str>)> {
    let text = text.trim();
    let end = text
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(text.len());
    let (name, rest) = text.split_at(end);
    if !name.starts_with(|c: char| c.is_ascii_uppercase()) {
        return None;
    }
    if rest.is_empty() {
        return Some((name, None));
    }
    let description = rest.strip_prefix(" (")?.strip_suffix(')')?;
    Some((name, Some(description)))
}

/// One line of step detail: its column and its text.
#[derive(Debug, Clone)]
struct Detail {
    indent: usize,
    text: String,
}

struct RawStep {
    name: String,
    description: Option<String>,
    parent: Option<usize>,
    details: Vec<Detail>,
}

/// Split the document into steps with their detail lines. Returns the
/// leading `Output:` value too.
fn read_steps(lines: &[String]) -> PlanResult<(Option<String>, Vec<RawStep>)> {
    let is_rail = |c: char| matches!(c, '\u{2502}' | '\u{251c}' | '\u{2514}');
    let compact = lines
        .iter()
        .any(|line| line.trim_start().starts_with(is_rail));
    let mut output = None;
    let mut steps: Vec<RawStep> = Vec::new();
    // Open steps, outermost first: the column of the name and the index.
    let mut stack: Vec<(usize, usize)> = Vec::new();
    // Classic layout: the column of an open `Indexes:` block, whose lines
    // (`PrimaryKey`, `Skip`) would otherwise read as child steps.
    let mut block: Option<usize> = None;
    for line in lines {
        let chars: Vec<char> = line.chars().collect();
        let Some(column) = chars.iter().position(|c| *c != ' ' && *c != FRAME) else {
            continue;
        };
        let rest: String = chars.iter().skip(column).collect();
        let marker = rest.starts_with("\u{251c}\u{2500}\u{2500}")
            || rest.starts_with("\u{2514}\u{2500}\u{2500}");
        let (name_column, text) = if marker {
            (column + 3, rest.chars().skip(3).collect::<String>())
        } else {
            (column, rest)
        };
        let text = text.trim_end().to_string();
        if text.is_empty() {
            continue;
        }
        if steps.is_empty() && !marker {
            if let Some(columns) = text.strip_prefix("Output:") {
                output = Some(columns.trim().to_string());
                continue;
            }
            // Anything before the first step line is not part of the plan.
            if split_step(&text).is_none() {
                continue;
            }
        }
        let is_step = if marker || steps.is_empty() {
            true
        } else if compact {
            false
        } else {
            if block.is_some_and(|at| column <= at) {
                block = None;
            }
            let current = stack.last().map(|(at, _)| *at).unwrap_or(0);
            block.is_none()
                && column <= current + 2
                && column.is_multiple_of(2)
                && split_step(&text).is_some()
        };
        if is_step {
            while stack.last().is_some_and(|(at, _)| *at >= name_column) {
                stack.pop();
            }
            if stack.len() >= MAX_DEPTH {
                return Err(PlanError::InvalidStructure(
                    "ClickHouse EXPLAIN: the plan nests too deeply".into(),
                ));
            }
            let (name, description) = match split_step(&text) {
                Some((name, description)) => (name.to_string(), description.map(str::to_string)),
                None => (text.clone(), None),
            };
            let index = steps.len();
            steps.push(RawStep {
                name,
                description: description.filter(|d| !d.trim().is_empty()),
                parent: stack.last().map(|(_, at)| *at),
                details: Vec::new(),
            });
            stack.push((name_column, index));
            block = None;
            continue;
        }
        if !compact && text == "Indexes:" {
            block = Some(column);
        }
        if let Some(step) = stack.last().and_then(|(_, at)| steps.get_mut(*at)) {
            step.details.push(Detail {
                indent: column,
                text,
            });
        }
    }
    Ok((output, steps))
}

/// `Key: value` or `Key:`; keys are short and start with a letter.
fn split_key(text: &str) -> Option<(&str, &str)> {
    let (key, value) = match text.split_once(": ") {
        Some((key, value)) => (key, value.trim()),
        None => (text.strip_suffix(':')?, ""),
    };
    let key = key.trim();
    let valid = !key.is_empty()
        && key.len() <= 40
        && key.starts_with(|c: char| c.is_ascii_alphabetic())
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '(' | ')' | '-' | '_'));
    valid.then_some((key, value))
}

/// Read a run of detail lines into `step`. A line owns the deeper lines
/// that follow it: as the rest of its value when they are aligned under the
/// value, as a nested block otherwise.
fn read_details(step: &mut Step, details: &[Detail], depth: usize) {
    if depth > 32 {
        return;
    }
    let mut index = 0usize;
    while let Some(detail) = details.get(index) {
        let mut end = index + 1;
        while details.get(end).is_some_and(|d| d.indent > detail.indent) {
            end += 1;
        }
        let nested = details.get(index + 1..end).unwrap_or(&[]);
        index = end;
        read_detail(step, detail, nested, depth);
    }
}

fn read_detail(step: &mut Step, detail: &Detail, nested: &[Detail], depth: usize) {
    // `Parts: 1 | Granules: 6` and `Type: inner | Strictness: all | ...`.
    let pieces: Vec<&str> = detail.text.split(" | ").collect();
    if pieces.len() > 1 && pieces.iter().all(|piece| split_key(piece).is_some()) {
        for piece in pieces {
            if let Some((key, value)) = split_key(piece) {
                apply(step, key, value.to_string(), &[]);
            }
        }
        read_details(step, nested, depth + 1);
        return;
    }
    let Some((key, value)) = split_key(&detail.text) else {
        let text = detail.text.trim();
        let mut words = text.split(' ');
        match (words.next(), words.next(), words.next()) {
            // `Limit 5`, `Offset 0`.
            (Some(name), Some(number), None)
                if !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) =>
            {
                step.property(name, number);
            }
            _ => {
                // Headings whose content follows on their own lines.
                if !matches!(
                    text,
                    "Prewhere info" | "Prewhere filter" | "Row level filter"
                ) {
                    step.extra.push(text.to_string());
                }
            }
        }
        read_details(step, nested, depth + 1);
        return;
    };
    // Lines aligned under a non-empty value continue it (`Header:`,
    // `Actions:`); anything else below is a block of its own lines.
    let continues = !value.is_empty() && nested.iter().all(|d| d.indent >= detail.indent + 4);
    if continues {
        let mut text = value.to_string();
        for line in nested {
            text.push('\n');
            text.push_str(line.text.trim());
        }
        apply(step, key, text, &[]);
    } else if apply(step, key, value.to_string(), nested) {
        // The handler consumed the block.
    } else {
        read_details(step, nested, depth + 1);
    }
}

/// Put one `key: value` (with the block under it, if any) into the step.
/// Returns true when the nested lines were consumed.
fn apply(step: &mut Step, key: &str, value: String, nested: &[Detail]) -> bool {
    let is_join = step.name.contains("Join");
    // The lines of a block at its shallowest indent: the items of a list.
    let items = || -> Vec<String> {
        let Some(level) = nested.iter().map(|d| d.indent).min() else {
            return Vec::new();
        };
        nested
            .iter()
            .filter(|d| d.indent == level)
            .map(|d| d.text.trim().to_string())
            .collect()
    };
    let list = |value: &str| -> Vec<String> {
        if value.is_empty() {
            items()
        } else {
            split_top_level(value)
        }
    };
    match key.to_ascii_lowercase().as_str() {
        "indexes" => {
            let (indexes, rest) = read_index_block(nested);
            step.indexes = Some(indexes);
            for (name, value) in rest {
                step.property(&name, value);
            }
            true
        }
        "keys" => {
            let keys = list(&value);
            if step.name.contains("Aggregat") || step.name == "Distinct" {
                step.group_keys = keys;
            } else {
                step.property(key, keys.join(", "));
            }
            true
        }
        "aggregates" => {
            step.property("Aggregates", list(&value).join(", "));
            true
        }
        "sort description" => {
            step.sort_keys = list(&value);
            true
        }
        "type" if is_join => {
            step.join_type = Some(value);
            false
        }
        "join conditions" | "clauses" => {
            step.join_predicate = Some(value);
            false
        }
        "prewhere filter column" | "filter column" | "row level filter column" => {
            step.add_filter(&value);
            false
        }
        "output" => {
            step.output_columns = split_top_level(&value);
            false
        }
        "header" => {
            if step.output_columns.is_empty() {
                step.output_columns = value.lines().map(header_column).collect();
            }
            step.property("Header", value);
            false
        }
        "output rows" => {
            if let Some(rows) = value
                .strip_prefix("estimated ")
                .and_then(parse_readable_quantity)
            {
                step.estimated_rows = Some(rows);
            }
            step.property(key, value);
            false
        }
        "granules" => {
            if let Ok(granules) = value.parse::<f64>()
                && granules.is_finite()
            {
                step.granules = Some(granules);
            }
            step.property(key, value);
            false
        }
        "readtype" => {
            step.property("Read type", value);
            false
        }
        _ => {
            step.property(key, value);
            false
        }
    }
}

/// `sum(__table2.total) Decimal(38, 2)` gives `sum(__table2.total)`: the
/// type is what follows the last space outside brackets.
fn header_column(line: &str) -> String {
    let line = line.trim();
    let mut depth = 0i32;
    let mut cut = None;
    for (at, c) in line.char_indices() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth -= 1,
            ' ' if depth <= 0 => cut = Some(at),
            _ => {}
        }
    }
    match cut {
        Some(at) => line.get(..at).unwrap_or(line).trim().to_string(),
        None => line.to_string(),
    }
}

/// `5/10` gives selected 5 of initial 10.
fn read_ratio(value: &str) -> Option<(f64, f64)> {
    let (selected, initial) = value.split_once('/')?;
    let selected: f64 = selected.trim().parse().ok()?;
    let initial: f64 = initial.trim().parse().ok()?;
    (selected.is_finite() && initial.is_finite()).then_some((selected, initial))
}

/// The lines under `Indexes:`. Index kinds stand alone on a line with their
/// facts below; `Ranges: 5` at the same level belongs to the read step and
/// is returned separately.
fn read_index_block(lines: &[Detail]) -> (Vec<IndexInfo>, Vec<(String, String)>) {
    let mut indexes: Vec<IndexInfo> = Vec::new();
    let mut rest = Vec::new();
    let Some(level) = lines.iter().map(|d| d.indent).min() else {
        return (indexes, rest);
    };
    // The indent of an open `Keys:` list, whose items are bare lines.
    let mut keys_at: Option<usize> = None;
    for line in lines {
        let text = line.text.trim();
        if line.indent == level {
            keys_at = None;
            match split_key(text) {
                Some((key, value)) => rest.push((key.to_string(), value.to_string())),
                None => indexes.push(IndexInfo {
                    kind: text.to_string(),
                    ..Default::default()
                }),
            }
            continue;
        }
        let Some(index) = indexes.last_mut() else {
            continue;
        };
        if keys_at.is_some_and(|at| line.indent > at) {
            index.keys.push(text.to_string());
            continue;
        }
        keys_at = None;
        let Some((key, value)) = split_key(text) else {
            continue;
        };
        match key {
            "Keys" => {
                if value.is_empty() {
                    keys_at = Some(line.indent);
                } else {
                    index.keys = split_top_level(value);
                }
            }
            "Condition" => index.condition = Some(value.to_string()),
            "Parts" => index.parts = read_ratio(value),
            "Granules" => index.granules = read_ratio(value),
            "Name" => index.name = Some(value.to_string()),
            "Description" => index.description = Some(value.to_string()),
            "Search Algorithm" => index.search_algorithm = Some(value.to_string()),
            _ => {}
        }
    }
    (indexes, rest)
}

/// Where the pipeline starts: the index of its first `(Step)` line, when
/// the document opens with one and a processor follows at the same indent.
fn pipeline_start(lines: &[String]) -> Option<usize> {
    let mut content = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty());
    let (at, first) = content.next()?;
    let (_, second) = content.next()?;
    step_label(first.trim())?;
    let indent = |line: &str| line.chars().take_while(|c| *c == ' ').count();
    let processor = second.trim();
    (indent(first) == indent(second)
        && processor.starts_with(|c: char| c.is_ascii_uppercase())
        && !processor.starts_with("Header"))
    .then_some(at)
}

/// `(ReadFromMergeTree)` gives `ReadFromMergeTree`.
fn step_label(text: &str) -> Option<&str> {
    let name = text.strip_prefix('(')?.strip_suffix(')')?;
    (name.starts_with(|c: char| c.is_ascii_uppercase())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

fn pipeline_from_lines(lines: &[String]) -> PlanResult<ExecutionPlan> {
    let Some(start) = pipeline_start(lines) else {
        return Err(PlanError::InvalidStructure(
            "ClickHouse EXPLAIN PIPELINE: expected a (Step) line followed by its processors".into(),
        ));
    };
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges = Vec::new();
    // Open processors, outermost first: indent and node id.
    let mut stack: Vec<(usize, i32)> = Vec::new();
    let mut label: Option<String> = None;
    // The indent of the latest `Header` line; deeper lines continue it.
    let mut header_at: Option<usize> = None;
    for line in lines.iter().skip(start) {
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let indent = line.chars().take_while(|c| *c == ' ').count();
        if let Some(name) = step_label(text) {
            label = Some(name.to_string());
            header_at = None;
            continue;
        }
        if header_at.is_some_and(|at| indent >= at + 8) {
            if let Some(header) = nodes
                .last_mut()
                .and_then(|node| node.properties.iter_mut().rfind(|p| p.name == "Header"))
            {
                header.value.push('\n');
                header.value.push_str(text);
            }
            continue;
        }
        header_at = None;
        if let Some(rest) = text.strip_prefix("Header")
            && let Some((streams, value)) = rest.split_once(':')
            && (streams.trim().is_empty() || streams.trim().starts_with('\u{d7}'))
            && let Some(node) = nodes.last_mut()
        {
            node.properties.push(PlanProperty {
                name: "Header".to_string(),
                value: value.trim().to_string(),
            });
            header_at = Some(indent);
            continue;
        }
        while stack.last().is_some_and(|(at, _)| *at >= indent) {
            stack.pop();
        }
        if stack.len() >= MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "ClickHouse EXPLAIN PIPELINE: the pipeline nests too deeply".into(),
            ));
        }
        let id = nodes.len() as i32 + 1;
        if let Some((_, parent)) = stack.last() {
            edges.push(PlanEdge {
                from_node: *parent,
                to_node: id,
            });
        }
        nodes.push(processor_node(id, text, label.as_deref()));
        stack.push((indent, id));
    }
    let mut plan = ExecutionPlan {
        statement_type: "EXPLAIN PIPELINE".to_string(),
        nodes,
        edges,
        dialect: Some(PlanDialect::ClickHouse),
        source_format: Some("clickhouse-pipeline".to_string()),
        ..Default::default()
    };
    super::common::finish_plan(&mut plan);
    Ok(plan)
}

/// `MergeTreeSelect(pool: ReadPool, algorithm: Thread) × 4 0 → 1`: the
/// processor, how many parallel copies run, and its input and output ports.
fn processor_node(id: i32, text: &str, step: Option<&str>) -> PlanNode {
    let mut words: Vec<&str> = text.split(' ').filter(|w| !w.is_empty()).collect();
    let is_number = |word: &&str| !word.is_empty() && word.chars().all(|c| c.is_ascii_digit());
    let mut properties = Vec::new();
    let mut ports = None;
    if let [.., inputs, "\u{2192}", outputs] = words.as_slice()
        && is_number(inputs)
        && is_number(outputs)
    {
        ports = Some((inputs.to_string(), outputs.to_string()));
        words.truncate(words.len() - 3);
    }
    let mut streams = None;
    if let [.., "\u{d7}", count] = words.as_slice()
        && is_number(count)
    {
        streams = count.parse::<u32>().ok();
        words.truncate(words.len() - 2);
    }
    let physical_op = words.join(" ");
    let logical_op = physical_op
        .split('(')
        .next()
        .unwrap_or("")
        .trim()
        .to_string();
    let mut push = |name: &str, value: String| {
        properties.push(PlanProperty {
            name: name.to_string(),
            value,
        })
    };
    if let Some(step) = step {
        push("Step", step.to_string());
    }
    if let Some(streams) = streams {
        push("Streams", streams.to_string());
    }
    if let Some((inputs, outputs)) = ports {
        push("Input ports", inputs);
        push("Output ports", outputs);
    }
    PlanNode {
        id,
        logical_op: if logical_op.is_empty() {
            physical_op.clone()
        } else {
            logical_op
        },
        physical_op,
        parallel: streams.is_some_and(|n| n > 1),
        workers_planned: streams,
        properties,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::warning::PlanWarning;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!("../../../tests/golden/plans/clickhouse/", $name))
        };
    }

    const JOIN_269: &str = fixture!("clickhouse269-join-plan.txt");
    const JOIN_INDEXES_269: &str = fixture!("clickhouse269-join-plan-indexes-actions.txt");
    const RANGE_INDEXES_269: &str = fixture!("clickhouse269-range-plan-indexes.txt");
    const RANGE_HEADER_269: &str = fixture!("clickhouse269-range-plan-header.txt");
    const RANGE_PRETTY_269: &str = fixture!("clickhouse269-range-plan-pretty.txt");
    const RANGE_VERTICAL_269: &str = fixture!("clickhouse269-range-plan-vertical.txt");
    const JOIN_PIPELINE_269: &str = fixture!("clickhouse269-join-pipeline.txt");
    const JOIN_248: &str = fixture!("clickhouse248-join-plan.txt");
    const JOIN_ACTIONS_248: &str = fixture!("clickhouse248-join-plan-actions.txt");
    const JOIN_HEADER_248: &str = fixture!("clickhouse248-join-plan-header.txt");
    const RANGE_INDEXES_248: &str = fixture!("clickhouse248-range-plan-indexes-actions.txt");
    const RANGE_PRETTY_248: &str = fixture!("clickhouse248-range-plan-pretty.txt");
    const RANGE_VERTICAL_248: &str = fixture!("clickhouse248-range-plan-vertical.txt");
    const JOIN_PIPELINE_248: &str = fixture!("clickhouse248-join-pipeline.txt");
    const RANGE_PIPELINE_HEADER_248: &str = fixture!("clickhouse248-range-pipeline-header.txt");

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.logical_op.as_str()).collect()
    }

    fn edges(plan: &ExecutionPlan) -> Vec<(i32, i32)> {
        plan.edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect()
    }

    fn prop<'a>(node: &'a PlanNode, name: &str) -> Option<&'a str> {
        node.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }

    #[test]
    fn tree_layout_reads_steps_details_and_join_estimates() {
        let plan = parse_clickhouse_explain_text(JOIN_269).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::ClickHouse));
        assert_eq!(plan.source_format.as_deref(), Some("clickhouse-text"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            [
                "Limit",
                "Sorting",
                "Aggregating",
                "Join",
                "ReadFromMergeTree",
                "BuildRuntimeFilter",
                "ReadFromMergeTree",
            ]
        );
        assert_eq!(
            edges(&plan),
            vec![(1, 2), (2, 3), (3, 4), (4, 5), (4, 6), (6, 7)]
        );
        let limit = &plan.nodes[0];
        assert_eq!(limit.physical_op, "Limit preliminary LIMIT");
        assert_eq!(prop(limit, "Limit"), Some("5"));
        assert_eq!(prop(limit, "Offset"), Some("0"));
        assert_eq!(limit.output_columns, ["country", "count()", "sum(total)"]);
        assert_eq!(plan.nodes[1].sort_keys, ["sum(total) DESC"]);
        let aggregating = &plan.nodes[2];
        assert_eq!(aggregating.group_keys, ["country"]);
        assert_eq!(prop(aggregating, "Aggregates"), Some("count(), sum(total)"));
        let join = &plan.nodes[3];
        assert_eq!(join.join_type.as_deref(), Some("inner"));
        assert_eq!(join.join_predicate.as_deref(), Some("id = customer_id"));
        assert_eq!(join.estimated_rows, 36_790.0);
        assert_eq!(prop(join, "Strictness"), Some("all"));
        assert_eq!(prop(join, "Algorithm"), Some("SpillingHashJoin(HashJoin)"));
        assert_eq!(prop(join, "Left"), Some("rows estimated 50.00 thousand"));
        assert!(join.extra.iter().any(|e| e.starts_with("c[50000]")));
        let customers = &plan.nodes[4];
        assert_eq!(customers.relation.as_deref(), Some("customers"));
        assert_eq!(customers.schema.as_deref(), Some("default"));
        assert_eq!(customers.output_columns, ["id", "country"]);
        assert_eq!(prop(customers, "Parts"), Some("1"));
        assert_eq!(customers.estimated_rows, 6.0 * 8192.0);
        // No index analysis in this capture, so no full-scan claim.
        assert!(customers.warnings.is_empty());
        let orders = &plan.nodes[6];
        assert_eq!(orders.physical_op, "ReadFromMergeTree default.orders");
        assert_eq!(
            orders.filter.as_deref(),
            Some("placed_at >= '2023-06-01 00:00:00' AND status = 'paid'")
        );
        assert_eq!(orders.estimated_rows, 179.0 * 8192.0);
        assert_eq!(plan.estimated_rows, 36_790.0);
    }

    #[test]
    fn tree_layout_index_analysis_feeds_estimates_and_scan_warnings() {
        let plan = parse_clickhouse_explain_text(JOIN_INDEXES_269).unwrap();
        assert_eq!(plan.nodes.len(), 7);
        let customers = &plan.nodes[4];
        assert_eq!(
            customers.output_list,
            ["PrimaryKey: true parts 1/1 granules 6/6"]
        );
        assert!(matches!(
            customers.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "customers" && schema == "default"
        ));
        assert_eq!(prop(customers, "Ranges"), Some("1"));
        let orders = &plan.nodes[6];
        assert_eq!(
            orders.output_list,
            [
                "Min-Max: (placed_at in [1685577600, +Inf)) parts 5/10 granules 179/392",
                "Partition: (toYYYYMM(placed_at) in [202306, +Inf)) parts 5/5 granules 179/179",
                "PrimaryKey: (placed_at in [1685577600, +Inf)) parts 5/5 granules 179/179",
            ]
        );
        assert_eq!(
            orders.index_cond.as_deref(),
            Some("(placed_at in [1685577600, +Inf))")
        );
        assert_eq!(
            prop(orders, "Partition index keys"),
            Some("toYYYYMM(placed_at)")
        );
        assert_eq!(
            prop(orders, "PrimaryKey index search"),
            Some("generic exclusion search")
        );
        assert_eq!(orders.estimated_rows, 179.0 * 8192.0);
        assert!(orders.warnings.is_empty());

        let plan = parse_clickhouse_explain_text(RANGE_INDEXES_269).unwrap();
        assert_eq!(ops(&plan), ["Aggregating", "ReadFromMergeTree"]);
        let read = &plan.nodes[1];
        assert_eq!(read.output_list.len(), 4);
        assert_eq!(
            read.output_list[3],
            "Skip: (total in ('800', +Inf)) parts 10/10 granules 10/10"
        );
        assert_eq!(read.index_name.as_deref(), Some("total_idx"));
        assert_eq!(
            prop(read, "Skip index total_idx definition"),
            Some("minmax GRANULARITY 4")
        );
        assert_eq!(
            read.index_cond.as_deref(),
            Some("and((customer_id in (-Inf, 200]), (customer_id in [100, +Inf)))")
        );
        assert_eq!(read.estimated_rows, 10.0 * 8192.0);
        assert_eq!(
            read.filter.as_deref(),
            Some("customer_id <= 200 AND customer_id >= 100 AND total > 800")
        );
    }

    #[test]
    fn tree_layout_header_lines_become_output_columns() {
        let plan = parse_clickhouse_explain_text(RANGE_HEADER_269).unwrap();
        let aggregating = &plan.nodes[0];
        assert_eq!(
            prop(aggregating, "Header"),
            Some("__table1.status LowCardinality(String)\ncount() UInt64")
        );
        assert_eq!(aggregating.group_keys, ["status"]);
        // `Output:` on a step wins over its header.
        assert_eq!(plan.nodes[1].output_columns, ["status"]);
    }

    #[test]
    fn indented_layout_reads_the_same_tree() {
        let plan = parse_clickhouse_explain_text(JOIN_248).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Expression",
                "Limit",
                "Sorting",
                "Expression",
                "Aggregating",
                "Expression",
                "Join",
                "Expression",
                "ReadFromMergeTree",
                "Expression",
                "ReadFromMergeTree",
            ]
        );
        assert_eq!(
            edges(&plan),
            vec![
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (7, 8),
                (8, 9),
                (7, 10),
                (10, 11),
            ]
        );
        assert_eq!(plan.nodes[0].physical_op, "Expression Project names");
        assert_eq!(
            plan.nodes[1].physical_op,
            "Limit preliminary LIMIT (without OFFSET)"
        );
        assert_eq!(
            plan.nodes[3].physical_op,
            "Expression (Before ORDER BY + Projection)"
        );
        assert_eq!(plan.nodes[8].relation.as_deref(), Some("customers"));
        assert_eq!(plan.nodes[10].relation.as_deref(), Some("orders"));
        assert!(plan.nodes.iter().all(|n| n.warnings.is_empty()));
    }

    #[test]
    fn indented_layout_actions_fill_typed_fields() {
        let plan = parse_clickhouse_explain_text(JOIN_ACTIONS_248).unwrap();
        assert_eq!(plan.nodes.len(), 11);
        assert_eq!(prop(&plan.nodes[1], "Limit"), Some("5"));
        assert_eq!(plan.nodes[2].sort_keys, ["sum(__table2.total) DESC"]);
        let aggregating = &plan.nodes[4];
        assert_eq!(aggregating.group_keys, ["__table1.country"]);
        assert_eq!(
            prop(aggregating, "Aggregates"),
            Some("count(), sum(__table2.total)")
        );
        let join = &plan.nodes[6];
        assert_eq!(join.join_type.as_deref(), Some("INNER"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("[(__table1.id) = (__table2.customer_id)]")
        );
        assert_eq!(prop(join, "Algorithm"), Some("HashJoin"));
        let orders = &plan.nodes[10];
        assert_eq!(
            orders.filter.as_deref(),
            Some(
                "and(greaterOrEquals(__table2.placed_at, '2023-06-01 00:00:00'_String), \
                 equals(__table2.status, 'paid'_String))"
            )
        );
        assert_eq!(prop(orders, "Read type"), Some("Default"));
        assert_eq!(orders.estimated_rows, 194.0 * 8192.0);
        assert!(prop(orders, "Actions").unwrap().contains("FUNCTION and("));
        assert!(prop(&plan.nodes[0], "Actions").unwrap().lines().count() > 1);
    }

    #[test]
    fn indented_layout_index_block_is_not_mistaken_for_steps() {
        let plan = parse_clickhouse_explain_text(RANGE_INDEXES_248).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Expression",
                "Aggregating",
                "Expression",
                "Expression",
                "ReadFromMergeTree"
            ]
        );
        let read = &plan.nodes[4];
        assert_eq!(
            read.output_list,
            [
                "MinMax: true parts 10/10 granules 389/389",
                "Partition: true parts 10/10 granules 389/389",
                "PrimaryKey: and((customer_id in (-Inf, 200]), (customer_id in [100, +Inf))) \
                 parts 10/10 granules 10/389",
                "Skip: parts 10/10 granules 10/10",
            ]
        );
        assert_eq!(prop(read, "PrimaryKey index keys"), Some("customer_id"));
        assert_eq!(read.index_name.as_deref(), Some("total_idx"));
        assert_eq!(read.estimated_rows, 10.0 * 8192.0);
        assert!(read.warnings.is_empty());
        assert_eq!(plan.nodes[1].group_keys, ["__table1.status"]);
        assert_eq!(prop(&plan.nodes[1], "Aggregates"), Some("count()"));
    }

    #[test]
    fn indented_layout_headers_become_output_columns() {
        let plan = parse_clickhouse_explain_text(JOIN_HEADER_248).unwrap();
        assert_eq!(plan.nodes.len(), 11);
        assert_eq!(
            plan.nodes[0].output_columns,
            ["country", "count()", "sum(total)"]
        );
        assert_eq!(
            plan.nodes[1].output_columns,
            ["sum(__table2.total)", "count()", "__table1.country"]
        );
    }

    #[test]
    fn client_decoration_is_removed() {
        let bare = parse_clickhouse_explain_text(RANGE_INDEXES_269).unwrap();
        for doc in [RANGE_PRETTY_269, RANGE_VERTICAL_269] {
            let plan = parse_clickhouse_explain_text(doc).unwrap();
            assert_eq!(ops(&plan), ops(&bare));
            assert_eq!(plan.nodes[1].output_list, bare.nodes[1].output_list);
            assert_eq!(plan.nodes[1].filter, bare.nodes[1].filter);
            assert_eq!(plan.nodes[0].group_keys, bare.nodes[0].group_keys);
        }
        for doc in [RANGE_PRETTY_248, RANGE_VERTICAL_248] {
            let plan = parse_clickhouse_explain_text(doc).unwrap();
            assert_eq!(plan.nodes.len(), 5);
            let read = &plan.nodes[4];
            assert_eq!(read.relation.as_deref(), Some("orders"));
            assert_eq!(read.output_list.len(), 4);
            assert_eq!(read.estimated_rows, 10.0 * 8192.0);
        }
    }

    #[test]
    fn pipeline_processors_become_nodes() {
        let plan = parse_clickhouse_explain_pipeline(JOIN_PIPELINE_248).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("clickhouse-pipeline"));
        assert_eq!(plan.statement_type, "EXPLAIN PIPELINE");
        assert_eq!(plan.nodes.len(), 17);
        assert_eq!(plan.nodes[0].logical_op, "ExpressionTransform");
        assert_eq!(prop(&plan.nodes[0], "Step"), Some("Expression"));
        let merging = &plan.nodes[2];
        assert_eq!(merging.logical_op, "MergingSortedTransform");
        assert_eq!(prop(merging, "Step"), Some("Sorting"));
        assert_eq!(prop(merging, "Input ports"), Some("7"));
        assert_eq!(prop(merging, "Output ports"), Some("1"));
        assert!(!merging.parallel);
        let sorting = &plan.nodes[3];
        assert_eq!(sorting.physical_op, "MergeSortingTransform");
        assert_eq!(sorting.workers_planned, Some(7));
        assert!(sorting.parallel);
        // JoiningTransform has both sides of the join under it.
        let joining = plan
            .nodes
            .iter()
            .find(|n| n.logical_op == "JoiningTransform")
            .unwrap();
        let children: Vec<&str> = plan
            .edges
            .iter()
            .filter(|e| e.from_node == joining.id)
            .map(|e| plan.nodes[e.to_node as usize - 1].logical_op.as_str())
            .collect();
        assert_eq!(children, ["ExpressionTransform", "FillingRightJoinSide"]);
        let last = plan.nodes.last().unwrap();
        assert_eq!(
            last.physical_op,
            "MergeTreeSelect(pool: ReadPool, algorithm: Thread)"
        );
        assert_eq!(last.logical_op, "MergeTreeSelect");
        assert_eq!(last.workers_planned, Some(5));
        assert_eq!(prop(last, "Step"), Some("ReadFromMergeTree"));

        let plan = parse_clickhouse_explain_pipeline(JOIN_PIPELINE_269).unwrap();
        assert_eq!(plan.nodes.len(), 23);
        assert_eq!(plan.nodes[10].logical_op, "Resize");
        assert_eq!(plan.nodes[10].workers_planned, Some(2));
        assert_eq!(prop(&plan.nodes[10], "Input ports"), Some("2"));
        // The text parser hands pipeline text to the pipeline parser.
        let same = parse_clickhouse_explain_text(JOIN_PIPELINE_269).unwrap();
        assert_eq!(same.source_format.as_deref(), Some("clickhouse-pipeline"));
    }

    #[test]
    fn pipeline_headers_attach_to_their_processor() {
        let plan = parse_clickhouse_explain_pipeline(RANGE_PIPELINE_HEADER_248).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "ExpressionTransform",
                "Resize",
                "AggregatingTransform",
                "StrictResize",
                "ExpressionTransform",
                "ExpressionTransform",
                "MergeTreeSelect",
            ]
        );
        assert_eq!(
            edges(&plan),
            vec![(1, 2), (2, 3), (3, 4), (4, 5), (5, 6), (6, 7)]
        );
        let header = prop(&plan.nodes[0], "Header").unwrap();
        assert_eq!(header.lines().count(), 2);
        assert!(header.starts_with("status LowCardinality(String)"));
        assert_eq!(prop(&plan.nodes[6], "Header").unwrap().lines().count(), 3);
        assert_eq!(plan.nodes[1].workers_planned, None);
        assert_eq!(prop(&plan.nodes[1], "Input ports"), Some("7"));
    }

    #[test]
    fn detectors_are_strict() {
        let plans = [
            JOIN_269,
            JOIN_INDEXES_269,
            RANGE_INDEXES_269,
            RANGE_HEADER_269,
            RANGE_PRETTY_269,
            RANGE_VERTICAL_269,
            JOIN_248,
            JOIN_ACTIONS_248,
            JOIN_HEADER_248,
            RANGE_INDEXES_248,
            RANGE_PRETTY_248,
            RANGE_VERTICAL_248,
            fixture!("clickhouse248-join-plan-nodesc.txt"),
        ];
        for doc in plans {
            assert!(looks_like_clickhouse_explain_text(doc), "{doc}");
            assert!(!looks_like_clickhouse_explain_pipeline(doc), "{doc}");
        }
        for doc in [
            JOIN_PIPELINE_269,
            JOIN_PIPELINE_248,
            RANGE_PIPELINE_HEADER_248,
        ] {
            assert!(looks_like_clickhouse_explain_pipeline(doc), "{doc}");
            assert!(!looks_like_clickhouse_explain_text(doc), "{doc}");
        }
        let others = [
            "",
            "hello",
            "Limit\n  ->  Seq Scan on orders\n",
            "Limit (actual time=0.011..0.012 rows=5 loops=1)\n  ->  Seq Scan on orders (actual \
             time=0.010..0.010 rows=5 loops=1)\n",
            "Seq Scan on orders  (cost=0.00..1.00 rows=1 width=4)\n",
            "-> Limit: 5 row(s)  (cost=0.35 rows=1)\n    -> Table scan on orders\n",
            "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n",
            "== Physical Plan ==\n*(1) Project [id#0]\n+- *(1) Scan parquet\n",
            "Fragment 0 [SINGLE]\n    Output layout: [count]\n",
            "default\torders\t10\t10240\t10\n",
            "SELECT\n    status,\n    count()\nFROM orders\n",
            "SelectWithUnionQuery (children 1)\n ExpressionList (children 1)\n",
            "QUERY id: 0\n  PROJECTION COLUMNS\n    status LowCardinality(String)\n",
            include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-explain.txt"),
            include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-tree.txt"),
            include_str!("../../../tests/golden/plans/sqlite/sqlite351-q2-rows-box.txt"),
        ];
        for doc in others {
            assert!(!looks_like_clickhouse_explain_text(doc), "{doc}");
            assert!(!looks_like_clickhouse_explain_pipeline(doc), "{doc}");
        }
    }

    #[test]
    fn other_explain_kinds_are_refused_by_name() {
        let cases = [
            ("default\torders\t10\t10240\t10\n", "EXPLAIN ESTIMATE"),
            (
                "default\torders\t5\t199286\t194\ndefault\tcustomers\t1\t50000\t6\n",
                "EXPLAIN ESTIMATE",
            ),
            (
                "SELECT\\n    status,\\n    count()\\nFROM orders\\nGROUP BY status",
                "EXPLAIN SYNTAX",
            ),
            (
                "SelectWithUnionQuery (children 1)\n ExpressionList (children 1)\n  SelectQuery \
                 (children 4)\n",
                "EXPLAIN AST",
            ),
            (
                "QUERY id: 0\n  PROJECTION COLUMNS\n    status LowCardinality(String)\n",
                "EXPLAIN QUERY TREE",
            ),
            (
                "digraph\n{\n  rankdir=\"LR\";\n  { node [shape = rect]\n    subgraph cluster_0 \
                 {\n      label =\"ReadFromMergeTree\";\n",
                "graph = 1",
            ),
        ];
        for (doc, expected) in cases {
            for parse in [
                parse_clickhouse_explain_text,
                parse_clickhouse_explain_pipeline,
            ] {
                let error = parse(doc).unwrap_err().to_string();
                assert!(error.contains(expected), "{expected}: {error}");
            }
        }
        assert!(parse_clickhouse_explain_text("").is_err());
        assert!(parse_clickhouse_explain_text("nothing like a plan").is_err());
        assert!(parse_clickhouse_explain_pipeline(JOIN_248).is_err());
    }

    #[test]
    fn clickhouse_text_survives_mangled_input() {
        for sample in [
            JOIN_INDEXES_269,
            RANGE_PRETTY_269,
            RANGE_VERTICAL_248,
            RANGE_INDEXES_248,
            JOIN_HEADER_248,
        ] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_clickhouse_explain_text(doc);
                let _ = looks_like_clickhouse_explain_text(doc);
            });
        }
        for sample in [JOIN_PIPELINE_269, RANGE_PIPELINE_HEADER_248] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_clickhouse_explain_pipeline(doc);
                let _ = looks_like_clickhouse_explain_pipeline(doc);
            });
        }
    }
}
