//! Apache Spark SQL `EXPLAIN` text: the `== Physical Plan ==` section of
//! `EXPLAIN`, `EXPLAIN EXTENDED`, `EXPLAIN COST` and `EXPLAIN FORMATTED`.
//!
//! The simple form puts everything about an operator on its line:
//!
//! ```text
//! == Physical Plan ==
//! *(2) Project [name#32]
//! +- *(2) BroadcastHashJoin [id#31L], [customer_id#14L], LeftSemi, BuildRight, false, false
//!    :- *(2) ColumnarToRow
//!    :  +- FileScan parquet spark_catalog.default.customers[id#31L,name#32] Batched: true, ...
//!    +- BroadcastExchange HashedRelationBroadcastMode(...), [plan_id=530]
//! ```
//!
//! A child is introduced by `+- ` or `:- ` and every level is three columns
//! wide. `*(2)` is the whole-stage code generation stage the operator was
//! compiled into.
//!
//! `EXPLAIN FORMATTED` prints the tree with only a name and a number per
//! operator (`+- * Filter (5)`), then one block per number with the details
//! (`(5) Filter [codegen id : 1]`, `Condition : ...`), then the subqueries
//! the same way. The blocks are joined back onto the tree here.
//!
//! Spark's text plans carry no row estimates and no run-time figures for
//! physical operators. `EXPLAIN COST` prints statistics on the optimized
//! logical plan only; the root's figures are copied onto the root operator
//! and the logical sections are otherwise left out.

use super::common;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// Deepest operator nesting accepted before the document is refused.
const MAX_DEPTH: usize = 512;

const PHYSICAL_HEADER: &str = "== Physical Plan ==";

/// Parse the text of a Spark SQL `EXPLAIN`, in any of its modes except
/// `CODEGEN`.
pub fn parse_spark_explain_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines: Vec<&str> = doc.lines().map(str::trim_end).collect();
    if lines
        .iter()
        .any(|line| line.starts_with("Found ") && line.ends_with("WholeStageCodegen subtrees."))
    {
        return Err(PlanError::InvalidStructure(
            "Spark EXPLAIN CODEGEN prints generated Java code, not a plan the viewer can read. Paste the output of EXPLAIN, EXPLAIN EXTENDED or EXPLAIN FORMATTED.".to_string(),
        ));
    }
    let header = lines.iter().position(|line| line.trim() == PHYSICAL_HEADER);
    let has_sections = lines.iter().any(|line| section_header(line));
    let body: &[&str] = match header {
        Some(index) => {
            let rest = lines.get(index + 1..).unwrap_or_default();
            let end = rest
                .iter()
                .position(|line| section_header(line))
                .unwrap_or(rest.len());
            rest.get(..end).unwrap_or_default()
        }
        None if has_sections => {
            return Err(PlanError::InvalidStructure(
                "Spark EXPLAIN: only logical plan sections found. Paste the output including the `== Physical Plan ==` section.".to_string(),
            ));
        }
        // A tree pasted without its header is still read, as long as it
        // has a child connector to show it is one.
        None if lines.iter().any(|line| tree_prefix(line).1) => &lines,
        None => {
            return Err(PlanError::InvalidStructure(
                "Spark EXPLAIN: no `== Physical Plan ==` section found. Paste the full output of EXPLAIN.".to_string(),
            ));
        }
    };

    let mut builder = Builder::default();
    if body.iter().any(|line| block_header(line).is_some()) {
        builder.read_formatted(body)?;
    } else {
        builder.read_simple(body)?;
    }
    let Builder {
        mut nodes, edges, ..
    } = builder;
    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "Spark EXPLAIN: no operators found under `== Physical Plan ==`. Paste the full output of EXPLAIN.".to_string(),
        ));
    }
    if let Some(statistics) = optimized_plan_statistics(&lines)
        && let Some(root) = nodes.first_mut()
    {
        if let Some(rows) = statistics
            .split_once("rowCount=")
            .and_then(|(_, rest)| common::parse_count(rest.split([',', ')']).next()?))
        {
            root.estimated_rows = rows;
        }
        push_property(root, "Optimized plan statistics", &statistics);
    }
    for node in &mut nodes {
        finish_node(node);
    }
    let mut plan = ExecutionPlan {
        source_format: Some("spark-text".to_string()),
        statement_type: "EXPLAIN".to_string(),
        nodes,
        edges,
        ..Default::default()
    };
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// True for Spark plan text: it has the `== Physical Plan ==` section header
/// with at least one line under it, and is not `EXPLAIN CODEGEN` output.
pub fn looks_like_spark_explain_text(doc: &str) -> bool {
    let mut lines = doc.lines().map(str::trim);
    if !lines.any(|line| line == PHYSICAL_HEADER) {
        return false;
    }
    lines.any(|line| !line.is_empty())
}

/// `== Parsed Logical Plan ==` and its siblings.
fn section_header(line: &str) -> bool {
    let line = line.trim();
    line.len() > 6 && line.starts_with("== ") && line.ends_with(" ==") && line.contains("Plan")
}

/// The `Statistics(...)` of the first operator of the optimized logical plan
/// that `EXPLAIN COST` prints, without the wrapper.
fn optimized_plan_statistics(lines: &[&str]) -> Option<String> {
    let index = lines
        .iter()
        .position(|line| line.trim() == "== Optimized Logical Plan ==")?;
    let first = lines
        .get(index + 1..)?
        .iter()
        .find(|line| !line.is_empty())?;
    let (_, rest) = first.rsplit_once(", Statistics(")?;
    Some(rest.strip_suffix(')')?.to_string())
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
    /// Ancestors of the operator being read, as (depth, node id).
    stack: Vec<(usize, i32)>,
}

impl Builder {
    /// Add an operator at `depth` under the nearest shallower one, or under
    /// `host` when it starts a tree.
    fn add(&mut self, depth: usize, mut node: PlanNode, host: Option<i32>) -> PlanResult<usize> {
        while self.stack.last().is_some_and(|(d, _)| *d >= depth) {
            self.stack.pop();
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(PlanError::InvalidStructure(format!(
                "Spark EXPLAIN: operators nest deeper than {MAX_DEPTH} levels"
            )));
        }
        let id = self.nodes.len() as i32 + 1;
        node.id = id;
        let parent = self.stack.last().map(|(_, parent)| *parent).or(host);
        if let Some(parent) = parent {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        self.stack.push((depth, id));
        self.nodes.push(node);
        Ok(self.nodes.len() - 1)
    }

    /// `EXPLAIN`, `EXPLAIN EXTENDED`, `EXPLAIN COST`: one line per operator.
    fn read_simple(&mut self, body: &[&str]) -> PlanResult<()> {
        for line in body {
            if line.trim().is_empty() {
                continue;
            }
            let (depth, connector, text) = tree_prefix(line);
            if !connector && !self.nodes.is_empty() {
                // A second line without a connector is not part of the tree
                // (a client timing line, a prompt).
                break;
            }
            let (stage, text) = codegen_prefix(text);
            let mut node = operator(text);
            if let Some(stage) = stage {
                push_property(&mut node, "Codegen stage", stage);
            }
            self.add(depth, node, None)?;
        }
        Ok(())
    }

    /// `EXPLAIN FORMATTED`: numbered trees, then numbered detail blocks.
    fn read_formatted(&mut self, body: &[&str]) -> PlanResult<()> {
        // Operator number to node index.
        let mut numbered: Vec<(String, usize)> = Vec::new();
        let mut in_tree = true;
        let mut block: Option<usize> = None;
        // The operator the next tree is a subquery of, with its expression.
        let mut host: Option<(i32, String)> = None;
        for line in body {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                block = None;
                if !self.stack.is_empty() {
                    in_tree = false;
                }
                continue;
            }
            if trimmed.starts_with("=====") {
                continue;
            }
            if let Some(rest) = trimmed.strip_prefix("Subquery:") {
                // `Subquery:1 Hosting operator id = 6 Hosting Expression = ...`
                let number = rest
                    .split_once("Hosting operator id = ")
                    .and_then(|(_, tail)| tail.split(' ').next());
                let expression = rest
                    .split_once("Hosting Expression = ")
                    .map(|(_, tail)| tail.trim())
                    .unwrap_or_default();
                host = number
                    .and_then(|number| numbered.iter().find(|(n, _)| n == number))
                    .and_then(|(_, index)| self.nodes.get(*index))
                    .map(|node| (node.id, expression.to_string()));
                self.stack.clear();
                in_tree = true;
                block = None;
                continue;
            }
            if let Some((number, head)) = block_header(trimmed) {
                in_tree = false;
                block = numbered
                    .iter()
                    .find(|(n, _)| n == number)
                    .map(|(_, index)| *index);
                if let Some(node) = block.and_then(|index| self.nodes.get_mut(index))
                    && let Some((_, tail)) = head.split_once(" [codegen id : ")
                {
                    push_property(node, "Codegen stage", tail.trim_end_matches(']').trim());
                }
                continue;
            }
            if in_tree {
                let (depth, _, text) = tree_prefix(line);
                let text = text.strip_prefix("* ").unwrap_or(text);
                let (label, number) = match text.rsplit_once(" (") {
                    Some((label, tail))
                        if tail.strip_suffix(')').is_some_and(|n| {
                            !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                        }) =>
                    {
                        (label, tail.trim_end_matches(')'))
                    }
                    _ => (text, ""),
                };
                let mut node = operator(label);
                if !number.is_empty() {
                    push_property(&mut node, "Operator id", number);
                }
                let starts_tree = self.stack.is_empty();
                let subquery_of = if starts_tree { host.take() } else { None };
                if let Some((_, expression)) = &subquery_of {
                    // `Subquery subquery#224, [id=#1188]`
                    let name = expression
                        .strip_prefix("Subquery ")
                        .unwrap_or(expression)
                        .split(',')
                        .next()
                        .unwrap_or_default();
                    node.subplan_name = Some(name.trim().to_string());
                    push_property(&mut node, "Hosting Expression", expression);
                }
                let index = self.add(depth, node, subquery_of.map(|(id, _)| id))?;
                if !number.is_empty() {
                    numbered.push((number.to_string(), index));
                }
            } else if let Some(node) = block.and_then(|index| self.nodes.get_mut(index)) {
                detail_line(node, trimmed);
            }
        }
        Ok(())
    }
}

/// Count the three-column tree levels a line starts with. Returns the depth,
/// whether a `+- ` / `:- ` connector leads into the text, and the text.
fn tree_prefix(line: &str) -> (usize, bool, &str) {
    let bytes = line.as_bytes();
    let mut at = 0usize;
    let mut depth = 0usize;
    let mut connector = false;
    loop {
        match bytes.get(at..at + 3) {
            Some(b"+- ") | Some(b":- ") => {
                depth += 1;
                at += 3;
                connector = true;
                break;
            }
            Some(b":  ") | Some(b"   ") => {
                depth += 1;
                at += 3;
            }
            _ => break,
        }
    }
    // Only ASCII was consumed, so `at` is a character boundary.
    (
        depth,
        connector,
        line.get(at..).unwrap_or_default().trim_start(),
    )
}

/// Split `*(2) Project [..]` into the stage number and the operator text.
fn codegen_prefix(text: &str) -> (Option<&str>, &str) {
    if let Some(rest) = text.strip_prefix("*(")
        && let Some((stage, rest)) = rest.split_once(") ")
        && !stage.is_empty()
        && stage.chars().all(|c| c.is_ascii_digit())
    {
        return (Some(stage), rest);
    }
    (None, text)
}

/// `(5) Filter [codegen id : 1]` gives `("5", "Filter [codegen id : 1]")`.
fn block_header(line: &str) -> Option<(&str, &str)> {
    let rest = line.strip_prefix('(')?;
    let (number, head) = rest.split_once(") ")?;
    (!number.is_empty()
        && number.chars().all(|c| c.is_ascii_digit())
        && head.starts_with(|c: char| c.is_ascii_alphabetic()))
    .then_some((number, head))
}

fn push_property(node: &mut PlanNode, name: &str, value: &str) {
    node.properties.push(PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    });
}

/// Build a node from the text of one operator: a line of the simple form, or
/// the label of a line of the `FORMATTED` tree.
fn operator(text: &str) -> PlanNode {
    let text = text.trim();
    let name_end = text.find([' ', '(', '[']).unwrap_or(text.len());
    let mut name = text.get(..name_end).unwrap_or(text).to_string();
    let mut rest = text.get(name_end..).unwrap_or_default();
    let mut node = PlanNode::default();

    match name.as_str() {
        "FileScan" | "Scan" => {
            // `FileScan parquet spark_catalog.default.orders[id#5L,total#7] Batched: true, ...`
            let mut words = rest.trim_start().splitn(2, ' ');
            if let Some(format) = words.next().filter(|word| !word.is_empty()) {
                name = format!("{name} {format}");
                rest = words.next().unwrap_or_default();
                let table_end = rest.find([' ', '[']).unwrap_or(rest.len());
                set_table(&mut node, rest.get(..table_end).unwrap_or_default());
                rest = rest.get(table_end..).unwrap_or_default();
                if let Some(inner) = rest.strip_prefix('[')
                    && let Some((columns, tail)) = inner.split_once(']')
                {
                    node.output_columns = split_top_level(columns);
                    rest = tail;
                }
                for (key, value) in scan_options(rest) {
                    push_property(&mut node, &key, &value);
                }
                rest = "";
            }
        }
        "Execute" => {
            let mut words = rest.trim_start().splitn(2, ' ');
            if let Some(command) = words.next().filter(|word| !word.is_empty()) {
                name = format!("{name} {command}");
                rest = words.next().unwrap_or_default();
            }
        }
        _ => {}
    }
    node.logical_op = name.clone();
    node.physical_op = name.clone();

    if let Some(inner) = rest.strip_prefix('(')
        && !rest.starts_with("( ")
        && name_end < text.len()
        && text.as_bytes().get(name_end) == Some(&b'(')
    {
        // `HashAggregate(keys=[..], functions=[..], output=[..])`
        let inner = inner.strip_suffix(')').unwrap_or(inner);
        for argument in split_top_level(inner) {
            let Some((key, value)) = argument.split_once('=') else {
                push_property(&mut node, &argument, "");
                continue;
            };
            match key {
                "keys" => node.group_keys = split_top_level(strip_brackets(value)),
                "orderBy" => node.sort_keys = split_top_level(strip_brackets(value)),
                "output" => node.output_columns = split_top_level(strip_brackets(value)),
                _ => push_property(&mut node, key, value),
            }
        }
        return node;
    }

    let arguments = rest.trim();
    if arguments.is_empty() {
        return node;
    }
    let parts = split_top_level(arguments);
    let first_list = parts
        .first()
        .filter(|part| part.starts_with('['))
        .map(|part| split_top_level(strip_brackets(part)));
    match name.as_str() {
        "Filter" => node.filter = Some(strip_parens(arguments).to_string()),
        "Project" => match first_list {
            Some(columns) => node.output_columns = columns,
            None => push_property(&mut node, "Arguments", arguments),
        },
        "Sort" => {
            node.sort_keys = first_list.unwrap_or_default();
            push_property(&mut node, "Arguments", arguments);
        }
        "Subquery" => {
            node.subplan_name = parts.first().cloned();
            push_property(&mut node, "Arguments", arguments);
        }
        _ if name.ends_with("Join") => {
            join_arguments(&mut node, &parts, arguments);
            push_property(&mut node, "Arguments", arguments);
        }
        _ => push_property(&mut node, "Arguments", arguments),
    }
    node
}

/// `[id#31L], [customer_id#14L], LeftSemi, BuildRight, false, false` in the
/// simple form, `Inner BuildRight` in a `FORMATTED` tree label.
fn join_arguments(node: &mut PlanNode, parts: &[String], arguments: &str) {
    let keys: Vec<&String> = parts.iter().filter(|part| part.starts_with('[')).collect();
    if let [left, right, ..] = keys.as_slice() {
        node.join_predicate = Some(key_equality(left, right));
    }
    node.join_type = parts
        .iter()
        .map(String::as_str)
        .chain(arguments.split(' '))
        .find_map(join_type_name)
        .map(str::to_string);
    // A condition beyond the keys is the one parenthesised argument.
    if let Some(condition) = parts.iter().find(|part| part.starts_with('(')) {
        node.filter = Some(strip_parens(condition).to_string());
    }
}

fn key_equality(left: &str, right: &str) -> String {
    let (left_keys, right_keys) = (
        split_top_level(strip_brackets(left)),
        split_top_level(strip_brackets(right)),
    );
    if left_keys.len() == right_keys.len() && !left_keys.is_empty() {
        left_keys
            .iter()
            .zip(&right_keys)
            .map(|(l, r)| format!("{l} = {r}"))
            .collect::<Vec<_>>()
            .join(" AND ")
    } else {
        format!("{left} = {right}")
    }
}

fn join_type_name(word: &str) -> Option<&'static str> {
    Some(match word {
        "Inner" => "Inner",
        "Cross" => "Cross",
        "LeftOuter" => "Left",
        "RightOuter" => "Right",
        "FullOuter" => "Full",
        "LeftSemi" => "Semi",
        "LeftAnti" => "Anti",
        _ if word.starts_with("ExistenceJoin") => "Existence",
        _ => return None,
    })
}

/// `spark_catalog.default.orders` is catalog, database, table.
fn set_table(node: &mut PlanNode, name: &str) {
    if name.is_empty() {
        return;
    }
    let parts: Vec<&str> = name.split('.').collect();
    match parts.as_slice() {
        [catalog, database, table] => {
            node.database = Some(catalog.to_string());
            node.schema = Some(database.to_string());
            node.relation = Some(table.to_string());
        }
        [database, table] => {
            node.schema = Some(database.to_string());
            node.relation = Some(table.to_string());
        }
        _ => node.relation = Some(name.to_string()),
    }
}

/// Split the tail of a `FileScan` line, `Batched: true, DataFilters: [..],
/// Format: Parquet, ...`, into its `name: value` pairs. A new pair starts at
/// a top-level `, ` that is followed by a word and `: `.
fn scan_options(text: &str) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> = Vec::new();
    for part in split_top_level(text) {
        let pair = part
            .split_once(": ")
            .filter(|(key, _)| !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric()));
        match pair {
            Some((key, value)) => options.push((key.to_string(), value.trim().to_string())),
            None => match (options.last_mut(), part.strip_suffix(':')) {
                (_, Some(key)) if key.chars().all(|c| c.is_ascii_alphanumeric()) => {
                    options.push((key.to_string(), String::new()));
                }
                (Some((_, value)), _) => {
                    value.push_str(", ");
                    value.push_str(&part);
                }
                (None, _) => options.push(("Arguments".to_string(), part)),
            },
        }
    }
    options
}

/// File one line of a `FORMATTED` detail block.
fn detail_line(node: &mut PlanNode, text: &str) {
    let Some((key, value)) = text.split_once(':') else {
        push_property(node, text, "");
        return;
    };
    // `Left keys [1]`, `Output [2]`, `Condition `: drop the count.
    let key = key.trim();
    let key = match key.rsplit_once(" [") {
        Some((name, count))
            if count
                .strip_suffix(']')
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit())) =>
        {
            name
        }
        _ => key,
    };
    let value = value.trim();
    let list = || split_top_level(strip_brackets(value));
    match key {
        "Output" | "Results" => node.output_columns = list(),
        "Condition" => node.filter = Some(strip_parens(value).to_string()),
        "Keys" => node.group_keys = list(),
        "Join type" => {
            node.join_type = Some(join_type_name(value).unwrap_or(value).to_string());
        }
        "Left keys" => push_property(node, key, value),
        "Right keys" => {
            if let Some(left) = node.property("Left keys") {
                node.join_predicate = Some(key_equality(left, value));
            }
            push_property(node, key, value);
        }
        "Join condition" => {
            if value != "None" {
                node.filter = Some(strip_parens(value).to_string());
            }
        }
        "Arguments" if node.physical_op == "Sort" => {
            if let Some(first) = split_top_level(value).first() {
                node.sort_keys = split_top_level(strip_brackets(first));
            }
            push_property(node, key, value);
        }
        _ => push_property(node, key, value),
    }
}

/// Derive the search strings and warnings once all of a node's lines are in.
fn finish_node(node: &mut PlanNode) {
    if let Some(relation) = node.relation.clone() {
        node.output_list.push(relation.clone());
        // A file scan with nothing pushed down and no partition filter reads
        // the whole table.
        let empty = |name: &str| node.property(name).is_none_or(|value| value == "[]");
        if empty("PushedFilters") && empty("PartitionFilters") && empty("DataFilters") {
            node.warnings.push(PlanWarning::TableScan {
                table: relation,
                schema: node.schema.clone().unwrap_or_default(),
            });
        }
    }
    if let Some(filter) = &node.filter {
        node.output_list.push(format!("Filter: {filter}"));
    }
    if let Some(predicate) = &node.join_predicate {
        node.output_list.push(format!("Condition: {predicate}"));
    }
}

fn strip_brackets(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value)
}

/// Drop one pair of parentheses that wraps the whole expression.
fn strip_parens(value: &str) -> &str {
    let value = value.trim();
    let Some(inner) = value.strip_prefix('(').and_then(|v| v.strip_suffix(')')) else {
        return value;
    };
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return value;
                }
            }
            _ => {}
        }
    }
    if depth == 0 { inner } else { value }
}

/// Split on commas that are not inside parentheses, brackets or braces.
/// `struct<a:int,b:int>` stays whole because a split also needs the space
/// Spark prints after a separating comma, or the end of a bracketed list.
fn split_top_level(text: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut depth = 0i32;
    let mut angle = 0i32;
    let mut current = String::new();
    for c in text.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth -= 1,
            '<' if current.ends_with("struct") || angle > 0 => angle += 1,
            '>' if angle > 0 => angle -= 1,
            ',' if depth <= 0 && angle <= 0 => {
                parts.push(current.trim().to_string());
                current.clear();
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    parts.push(current.trim().to_string());
    parts.retain(|part| !part.is_empty());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q1_SIMPLE: &str = include_str!("../../../tests/golden/plans/spark/spark43-q1-simple.txt");
    const Q2_SIMPLE: &str = include_str!("../../../tests/golden/plans/spark/spark43-q2-simple.txt");
    const Q2_EXTENDED: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q2-extended.txt");
    const Q2_CBO_COST: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q2-cbo-cost.txt");
    const Q3_SIMPLE: &str = include_str!("../../../tests/golden/plans/spark/spark43-q3-simple.txt");
    const Q4_NOAQE_SIMPLE: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q4-noaqe-simple.txt");
    const Q4_NOAQE_FORMATTED: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q4-noaqe-formatted.txt");
    const Q5_SIMPLE: &str = include_str!("../../../tests/golden/plans/spark/spark43-q5-simple.txt");
    const Q5_NOAQE_SIMPLE: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q5-noaqe-simple.txt");
    const Q5_FORMATTED: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q5-formatted.txt");
    const Q6_SIMPLE: &str = include_str!("../../../tests/golden/plans/spark/spark43-q6-simple.txt");
    const Q6_FORMATTED: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-q6-formatted.txt");
    const INSERT_SIMPLE: &str =
        include_str!("../../../tests/golden/plans/spark/spark43-insert-simple.txt");

    /// First lines of `EXPLAIN CODEGEN` from Spark 4.3.0.
    const CODEGEN: &str = "Found 4 WholeStageCodegen subtrees.\n== Subtree 1 / 4 (maxMethodCodeSize:378; maxConstantPoolSize:157(0.24% used); numInnerClasses:0) ==\n*(1) Filter (region#33 IN (east,west) AND isnotnull(id#31L))\n+- *(1) ColumnarToRow\n\nGenerated code:\n/* 001 */ public Object generate(Object[] references) {\n";

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes
            .iter()
            .map(|node| node.physical_op.as_str())
            .collect()
    }

    fn children(plan: &ExecutionPlan, id: i32) -> Vec<i32> {
        common::child_ids(&plan.edges, id)
    }

    #[test]
    fn simple_plan_reads_tree_and_operator_arguments() {
        let plan = parse_spark_explain_text(Q2_SIMPLE).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("spark-text"));
        assert!(plan.dialect.is_none());
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            vec![
                "AdaptiveSparkPlan",
                "Sort",
                "Exchange",
                "HashAggregate",
                "Exchange",
                "HashAggregate",
                "Project",
                "BroadcastHashJoin",
                "BroadcastExchange",
                "Filter",
                "FileScan parquet",
                "Filter",
                "FileScan parquet",
            ]
        );
        assert_eq!(children(&plan, 8), vec![9, 12]);
        assert_eq!(children(&plan, 9), vec![10]);
        assert_eq!(children(&plan, 12), vec![13]);
        assert_eq!(plan.edges.len(), 12);

        assert_eq!(
            plan.nodes[0].property("Arguments"),
            Some("isFinalPlan=false")
        );
        assert_eq!(plan.nodes[1].sort_keys, vec!["revenue#32 DESC NULLS LAST"]);
        assert_eq!(
            plan.nodes[2].property("Arguments"),
            Some(
                "rangepartitioning(revenue#32 DESC NULLS LAST, 200), ENSURE_REQUIREMENTS, [plan_id=194]"
            )
        );
        let aggregate = &plan.nodes[3];
        assert_eq!(aggregate.group_keys, vec!["region#40"]);
        assert_eq!(
            aggregate.property("functions"),
            Some("[count(1), sum(total#7)]")
        );
        assert_eq!(plan.nodes[6].output_columns, vec!["region#40", "total#7"]);

        let join = &plan.nodes[7];
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("id#38L = customer_id#6L")
        );
        assert!(join.property("Arguments").unwrap().contains("BuildLeft"));

        let filter = &plan.nodes[9];
        assert_eq!(
            filter.filter.as_deref(),
            Some("region#40 IN (east,west) AND isnotnull(id#38L)")
        );
        assert_eq!(
            plan.nodes[11].filter.as_deref(),
            Some("isnotnull(customer_id#6L)")
        );

        let scan = &plan.nodes[10];
        assert_eq!(scan.database.as_deref(), Some("spark_catalog"));
        assert_eq!(scan.schema.as_deref(), Some("default"));
        assert_eq!(scan.relation.as_deref(), Some("customers"));
        assert_eq!(scan.output_columns, vec!["id#38L", "region#40"]);
        assert_eq!(scan.property("Batched"), Some("true"));
        assert_eq!(scan.property("Format"), Some("Parquet"));
        assert_eq!(
            scan.property("DataFilters"),
            Some("[region#40 IN (east,west), isnotnull(id#38L)]")
        );
        assert_eq!(
            scan.property("Location"),
            Some("InMemoryFileIndex(1 paths)[file:/tmp/spark-warehouse/customers]")
        );
        assert_eq!(scan.property("PartitionFilters"), Some("[]"));
        assert_eq!(
            scan.property("PushedFilters"),
            Some("[In(region, [east,west]), IsNotNull(id)]")
        );
        assert_eq!(
            scan.property("ReadSchema"),
            Some("struct<id:bigint,region:string>")
        );
        assert!(scan.warnings.is_empty());
    }

    #[test]
    fn codegen_stages_and_unfiltered_scan() {
        let plan = parse_spark_explain_text(Q4_NOAQE_SIMPLE).unwrap();
        assert_eq!(plan.nodes.len(), 14);
        assert_eq!(plan.nodes[0].physical_op, "Union");
        assert_eq!(plan.nodes[0].property("Codegen stage"), Some("2"));
        assert_eq!(children(&plan, 1), vec![2, 11]);
        let join = &plan.nodes[2];
        assert_eq!(join.join_type.as_deref(), Some("Semi"));
        assert_eq!(children(&plan, 3), vec![4, 6]);
        let customers = &plan.nodes[4];
        assert_eq!(customers.relation.as_deref(), Some("customers"));
        assert!(customers.property("Codegen stage").is_none());
        assert!(matches!(
            customers.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "customers" && schema == "default"
        ));
        assert_eq!(plan.nodes[6].property("Codegen stage"), Some("1"));
        assert_eq!(
            plan.nodes[7].filter.as_deref(),
            Some("isnotnull(total#15) AND (total#15 > 499.00)")
        );
    }

    #[test]
    fn single_stage_plan_and_partition_filters() {
        let plan = parse_spark_explain_text(Q1_SIMPLE).unwrap();
        assert_eq!(
            ops(&plan),
            vec!["Filter", "ColumnarToRow", "FileScan parquet"]
        );
        assert_eq!(
            plan.nodes[0].filter.as_deref(),
            Some(
                "((isnotnull(status#8) AND isnotnull(total#7)) AND (status#8 = paid)) AND (total#7 > 400.00)"
            )
        );
        let plan = parse_spark_explain_text(Q3_SIMPLE).unwrap();
        let top = &plan.nodes[1];
        assert_eq!(top.physical_op, "TakeOrderedAndProject");
        assert_eq!(top.property("limit"), Some("10"));
        assert_eq!(top.sort_keys, vec!["total#7 DESC NULLS LAST"]);
        assert_eq!(
            top.output_columns,
            vec!["id#5L", "total#7", "sku#115", "qty#117L"]
        );
        let items = plan.nodes.last().unwrap();
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(
            items.property("PartitionFilters"),
            Some("[isnotnull(qty#117L), (qty#117L = 3)]")
        );
    }

    #[test]
    fn inline_subquery_hangs_under_its_filter() {
        for (doc, name, count) in [
            (Q5_SIMPLE, "subquery#176", 21),
            (Q5_NOAQE_SIMPLE, "scalar-subquery#100", 22),
        ] {
            let plan = parse_spark_explain_text(doc).unwrap();
            assert_eq!(plan.nodes.len(), count);
            let filter = plan
                .nodes
                .iter()
                .find(|node| {
                    node.filter
                        .as_deref()
                        .is_some_and(|f| f.contains("Subquery"))
                })
                .unwrap();
            let inputs = children(&plan, filter.id);
            assert_eq!(inputs.len(), 2);
            let subquery = &plan.nodes[inputs[0] as usize - 1];
            assert_eq!(subquery.physical_op, "Subquery");
            assert_eq!(subquery.subplan_name.as_deref(), Some(name));
            let aggregate = &plan.nodes[inputs[1] as usize - 1];
            assert_eq!(aggregate.physical_op, "HashAggregate");
            assert_eq!(aggregate.group_keys.len(), 1);
        }
        let plan = parse_spark_explain_text(Q5_SIMPLE).unwrap();
        assert_eq!(plan.nodes[1].physical_op, "Window");
        assert!(
            plan.nodes[1]
                .property("Arguments")
                .unwrap()
                .starts_with("[row_number() windowspecdefinition(")
        );
    }

    #[test]
    fn extended_and_cost_use_the_physical_section() {
        let plan = parse_spark_explain_text(Q2_EXTENDED).unwrap();
        assert_eq!(plan.nodes.len(), 13);
        assert_eq!(plan.nodes[0].physical_op, "AdaptiveSparkPlan");
        // Nothing from the logical sections is in the tree.
        assert!(plan.nodes.iter().all(|node| {
            !["Aggregate", "Join", "Relation", "SubqueryAlias"].contains(&node.physical_op.as_str())
        }));
        assert_eq!(
            plan.nodes[3].output_columns,
            vec!["region#40", "n#50L", "revenue#51"]
        );
        assert_eq!(plan.estimated_rows, 0.0);

        let plan = parse_spark_explain_text(Q2_CBO_COST).unwrap();
        assert_eq!(plan.nodes.len(), 14);
        assert_eq!(plan.nodes[0].physical_op, "Sort");
        assert_eq!(plan.nodes[0].property("Codegen stage"), Some("4"));
        assert_eq!(plan.nodes[0].estimated_rows, 2.0);
        assert_eq!(plan.estimated_rows, 2.0);
        assert_eq!(
            plan.nodes[0].property("Optimized plan statistics"),
            Some("sizeInBytes=98.0 B, rowCount=2")
        );
        assert_eq!(plan.nodes[1].estimated_rows, 0.0);
    }

    #[test]
    fn formatted_joins_detail_blocks_onto_the_tree() {
        let plan = parse_spark_explain_text(Q6_FORMATTED).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                "AdaptiveSparkPlan",
                "HashAggregate",
                "Exchange",
                "HashAggregate",
                "Project",
                "SortMergeJoin",
                "Sort",
                "Exchange",
                "Filter",
                "Scan parquet",
                "Sort",
                "Exchange",
                "Project",
                "Filter",
                "Scan parquet",
            ]
        );
        assert_eq!(children(&plan, 6), vec![7, 11]);
        let root = &plan.nodes[0];
        assert_eq!(root.property("Operator id"), Some("15"));
        assert_eq!(root.property("Arguments"), Some("isFinalPlan=false"));
        assert_eq!(
            root.output_columns,
            vec!["status#8", "sku#115", "max(price)#318"]
        );
        let aggregate = &plan.nodes[1];
        assert_eq!(aggregate.group_keys, vec!["status#8", "sku#115"]);
        assert_eq!(aggregate.property("Functions"), Some("[max(price#116)]"));
        assert_eq!(
            aggregate.property("Input"),
            Some("[status#8, sku#115, max#320]")
        );
        let join = &plan.nodes[5];
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("id#5L = order_id#114L")
        );
        assert!(join.filter.is_none());
        let sort = &plan.nodes[6];
        assert_eq!(sort.sort_keys, vec!["id#5L ASC NULLS FIRST"]);
        assert_eq!(plan.nodes[8].filter.as_deref(), Some("isnotnull(id#5L)"));
        let scan = &plan.nodes[9];
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.property("Operator id"), Some("1"));
        assert_eq!(scan.output_columns, vec!["id#5L", "status#8"]);
        assert_eq!(scan.property("PushedFilters"), Some("[IsNotNull(id)]"));
        assert_eq!(
            scan.property("Location"),
            Some("InMemoryFileIndex [file:/tmp/spark-warehouse/orders]")
        );
        assert_eq!(
            scan.property("ReadSchema"),
            Some("struct<id:bigint,status:string>")
        );
        assert!(scan.warnings.is_empty());

        // The same query in the simple form gives the same operators.
        let simple = parse_spark_explain_text(Q6_SIMPLE).unwrap();
        assert_eq!(simple.nodes.len(), plan.nodes.len());
        assert_eq!(children(&simple, 6), vec![7, 11]);
        assert_eq!(
            simple.nodes[5].join_predicate.as_deref(),
            Some("id#5L = order_id#114L")
        );
    }

    #[test]
    fn formatted_subquery_section_hangs_under_its_host() {
        let plan = parse_spark_explain_text(Q5_FORMATTED).unwrap();
        assert_eq!(plan.nodes.len(), 20);
        assert_eq!(plan.edges.len(), 19);
        let filter = plan
            .nodes
            .iter()
            .find(|node| node.property("Operator id") == Some("6"))
            .unwrap();
        assert_eq!(filter.physical_op, "Filter");
        assert!(
            filter
                .filter
                .as_deref()
                .unwrap()
                .contains("Subquery subquery#224")
        );
        let inputs = children(&plan, filter.id);
        assert_eq!(inputs.len(), 2);
        let subquery = &plan.nodes[inputs[1] as usize - 1];
        assert_eq!(subquery.physical_op, "AdaptiveSparkPlan");
        assert_eq!(subquery.property("Operator id"), Some("20"));
        assert_eq!(subquery.subplan_name.as_deref(), Some("subquery#224"));
        assert_eq!(subquery.output_columns, vec!["avg(total)#233"]);
        let scan = plan.nodes.last().unwrap();
        assert_eq!(scan.property("Operator id"), Some("16"));
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        // No pushed filters: this scan reads the whole table.
        assert_eq!(scan.warnings.len(), 1);
        let scalar = plan
            .nodes
            .iter()
            .find(|node| node.property("Operator id") == Some("19"))
            .unwrap();
        assert!(scalar.group_keys.is_empty());
        assert_eq!(
            scalar.property("Functions"),
            Some("[avg(UnscaledValue(total#236))]")
        );
        let join = plan
            .nodes
            .iter()
            .find(|node| node.physical_op == "BroadcastHashJoin")
            .unwrap();
        assert_eq!(join.property("Arguments"), Some("Inner BuildRight"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("customer_id#6L = id#38L")
        );
    }

    #[test]
    fn formatted_codegen_ids() {
        let plan = parse_spark_explain_text(Q4_NOAQE_FORMATTED).unwrap();
        assert_eq!(plan.nodes.len(), 14);
        assert_eq!(plan.nodes[0].physical_op, "Union");
        assert_eq!(plan.nodes[0].property("Codegen stage"), Some("2"));
        assert_eq!(children(&plan, 1), vec![2, 11]);
        let join = &plan.nodes[2];
        assert_eq!(join.join_type.as_deref(), Some("Semi"));
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("id#31L = customer_id#14L")
        );
        let filter = plan
            .nodes
            .iter()
            .find(|node| node.property("Operator id") == Some("5"))
            .unwrap();
        assert_eq!(filter.property("Codegen stage"), Some("1"));
        assert_eq!(
            filter.filter.as_deref(),
            Some("isnotnull(total#15) AND (total#15 > 499.00)")
        );
        assert!(plan.nodes[12].property("Codegen stage").is_none());
    }

    #[test]
    fn command_plan() {
        let plan = parse_spark_explain_text(INSERT_SIMPLE).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                "Execute InsertIntoHadoopFsRelationCommand",
                "WriteFiles",
                "Project",
                "Range",
            ]
        );
        assert!(
            plan.nodes[0]
                .property("Arguments")
                .unwrap()
                .starts_with("file:/tmp/spark-warehouse/customers, false, Parquet")
        );
        assert_eq!(
            plan.nodes[3].property("Arguments"),
            Some("(0, 5, step=1, splits=7)")
        );
        assert_eq!(plan.nodes[3].property("Codegen stage"), Some("1"));
    }

    #[test]
    fn other_outputs_are_refused_with_a_reason() {
        let error = parse_spark_explain_text(CODEGEN).unwrap_err();
        assert!(error.to_string().contains("CODEGEN"));
        assert!(!looks_like_spark_explain_text(CODEGEN));
        let logical_only = "== Optimized Logical Plan ==\nProject [id#1L]\n+- Range (0, 10, step=1, splits=Some(7))\n";
        let error = parse_spark_explain_text(logical_only).unwrap_err();
        assert!(error.to_string().contains("Physical Plan"));
        assert!(parse_spark_explain_text("").is_err());
        assert!(parse_spark_explain_text("== Physical Plan ==\n").is_err());
        assert!(parse_spark_explain_text("SELECT 1;\nSELECT 2;\n").is_err());
        let bare = "Project [a#1]\n+- Range (0, 10, step=1, splits=7)\n";
        assert_eq!(parse_spark_explain_text(bare).unwrap().nodes.len(), 2);
    }

    #[test]
    fn detector_accepts_own_formats_only() {
        for doc in [
            Q1_SIMPLE,
            Q2_SIMPLE,
            Q2_EXTENDED,
            Q2_CBO_COST,
            Q3_SIMPLE,
            Q4_NOAQE_SIMPLE,
            Q4_NOAQE_FORMATTED,
            Q5_SIMPLE,
            Q5_NOAQE_SIMPLE,
            Q5_FORMATTED,
            Q6_SIMPLE,
            Q6_FORMATTED,
            INSERT_SIMPLE,
        ] {
            assert!(looks_like_spark_explain_text(doc));
        }
        for doc in [
            "Seq Scan on t  (cost=0.00..1.50 rows=50 width=4)\n  ->  Index Scan using i on u  (cost=0.1..2.0 rows=1 width=4)",
            "-> Limit: 10 row(s)  (cost=1.2 rows=10)\n    -> Table scan on t  (cost=1.2 rows=100)",
            "distribution: local\nvectorized: true\n\n• scan\n  estimated row count: 1\n  table: orders@orders_pkey\n  spans: [/77 - /77]",
            "Trino version: 484\nFragment 0 [SINGLE]\n    Output[columnNames = [a]]\n    │   Layout: [a:bigint]",
            "== Physical Plan ==",
            "",
        ] {
            assert!(!looks_like_spark_explain_text(doc));
        }
    }

    #[test]
    fn never_panics_on_mangled_input() {
        for sample in [
            Q2_SIMPLE,
            Q2_EXTENDED,
            Q2_CBO_COST,
            Q4_NOAQE_FORMATTED,
            Q5_NOAQE_SIMPLE,
            Q5_FORMATTED,
            INSERT_SIMPLE,
            CODEGEN,
        ] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_spark_explain_text(doc);
                let _ = looks_like_spark_explain_text(doc);
            });
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut doc = String::from("== Physical Plan ==\nProject [a#1]\n");
        for depth in 0..600 {
            doc.push_str(&"   ".repeat(depth));
            doc.push_str("+- Project [a#1]\n");
        }
        assert!(parse_spark_explain_text(&doc).is_err());
    }
}
