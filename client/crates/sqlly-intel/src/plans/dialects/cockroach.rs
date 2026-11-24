//! CockroachDB `EXPLAIN`, `EXPLAIN (VERBOSE)`, `EXPLAIN (TYPES)`,
//! `EXPLAIN (DISTSQL)` and `EXPLAIN ANALYZE` text.
//!
//! The document is one `info` column. Header lines (`distribution: local`,
//! `planning time: 514µs`) come first, then a tree in which every operator is
//! a `•` bullet and its facts are `name: value` lines below it:
//!
//! ```text
//! • hash join
//! │ estimated row count: 10,000
//! │ equality: (customer_id) = (id)
//! │
//! ├── • scan
//! │     table: orders@orders_pkey
//! │     spans: FULL SCAN
//! ```
//!
//! A child bullet sits four columns to the right of its parent. The
//! `cockroach sql` client prints the column bare, double-quoted when a line
//! starts with a space (its TSV and CSV output), inside a psql-style table,
//! or as `info | ...` records; all four are read.

use super::common;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty, PlanSpill};
use crate::plans::warning::{MissingIndex, PlanWarning};

/// Deepest operator nesting accepted before the document is refused.
const MAX_DEPTH: usize = 512;

/// Parse the text of a CockroachDB `EXPLAIN` or `EXPLAIN ANALYZE`.
///
/// Per operator, `execution time` (or `KV time` on scans, which report no
/// execution time) is the operator's own time, so it fills
/// `actual_self_time_ms`; `actual_time_ms` is that plus the same figure for
/// every input below it. CockroachDB does not print loop counts, so `loops`
/// stays empty and `actual_rows` is the total the engine reports.
pub fn parse_cockroach_explain_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines = clean_lines(doc);
    let mut plan = ExecutionPlan {
        source_format: Some("cockroachdb-text".to_string()),
        ..Default::default()
    };
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    // Ancestors of the node being read, as (bullet column, node id).
    let mut stack: Vec<(usize, i32)> = Vec::new();
    let mut root_column = 0usize;
    let mut trailer: Vec<String> = Vec::new();
    let mut in_trailer = false;

    for line in &lines {
        if in_trailer {
            if !line.trim().is_empty() {
                trailer.push(line.clone());
            }
            continue;
        }
        if let Some((column, name)) = bullet(line) {
            let name = name.trim();
            if name.is_empty() {
                continue;
            }
            if nodes.is_empty() {
                root_column = column;
            }
            while stack.last().is_some_and(|(c, _)| *c >= column) {
                stack.pop();
            }
            if stack.len() >= MAX_DEPTH {
                return Err(PlanError::InvalidStructure(format!(
                    "CockroachDB EXPLAIN: operators nest deeper than {MAX_DEPTH} levels"
                )));
            }
            let id = nodes.len() as i32 + 1;
            if let Some((_, parent)) = stack.last() {
                edges.push(PlanEdge {
                    from_node: *parent,
                    to_node: id,
                });
            }
            stack.push((column, id));
            nodes.push(new_node(id, name));
            continue;
        }
        let (lead, text) = strip_tree(line);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Some(node) = nodes.last_mut() else {
            header_line(&mut plan, text);
            continue;
        };
        // Facts of an operator are indented past the root bullet. Text back
        // at the root column is what follows the tree: index
        // recommendations, the DISTSQL diagram URL, a client footer.
        if lead <= root_column {
            in_trailer = true;
            trailer.push(line.clone());
            continue;
        }
        node_line(node, text);
    }

    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(no_operators_message(&lines)));
    }
    for node in &mut nodes {
        finish_node(node);
    }
    trailer_lines(&mut plan, &trailer);

    let executed =
        plan.execution_time_ms.is_some() || nodes.iter().any(|node| node.actual_rows.is_some());
    if executed {
        fill_inclusive_times(&mut nodes, &edges);
    }
    plan.statement_type = if executed {
        "EXPLAIN ANALYZE".to_string()
    } else {
        "EXPLAIN".to_string()
    };
    plan.nodes = nodes;
    plan.edges = edges;
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// True for CockroachDB plan text: at least one `•` operator bullet in tree
/// position plus one of the lines every such plan carries (`distribution:`,
/// `vectorized:`, a row count, `table:`, `spans:`).
pub fn looks_like_cockroach_explain_text(doc: &str) -> bool {
    let lines = clean_lines(doc);
    if !lines.iter().any(|line| bullet(line).is_some()) {
        return false;
    }
    lines.iter().any(|line| {
        let text = strip_tree(line).1.trim();
        [
            "distribution:",
            "vectorized:",
            "estimated row count:",
            "actual row count:",
            "table:",
            "spans:",
        ]
        .iter()
        .any(|marker| text.starts_with(marker))
    })
}

/// Undo the client's decoration, one entry per plan line: TSV / CSV quoting,
/// the `info` header, the table rule and `(N rows)` footer, `info | ` record
/// prefixes and the `#` comment lines of `--format=raw`.
fn clean_lines(doc: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in doc.lines() {
        let mut line = raw.trim_end().to_string();
        let trimmed = line.trim();
        if trimmed.starts_with("-[ RECORD") || trimmed.starts_with('#') {
            continue;
        }
        if trimmed == "info" || trimmed == "\"info\"" {
            continue;
        }
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '-' || c == '+') {
            continue;
        }
        if is_row_count_footer(trimmed) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("info |") {
            line = rest.strip_prefix(' ').unwrap_or(rest).to_string();
        }
        if line.len() >= 2 && line.starts_with('"') && line.ends_with('"') {
            line = line[1..line.len() - 1].replace("\"\"", "\"");
        }
        lines.push(line);
    }
    lines
}

/// `(25 rows)` / `(1 row)`.
fn is_row_count_footer(text: &str) -> bool {
    let Some(inner) = text.strip_prefix('(').and_then(|t| t.strip_suffix(')')) else {
        return false;
    };
    let Some(count) = inner
        .strip_suffix(" rows")
        .or_else(|| inner.strip_suffix(" row"))
    else {
        return false;
    };
    !count.is_empty() && count.chars().all(|c| c.is_ascii_digit())
}

fn is_tree_char(c: char) -> bool {
    matches!(c, ' ' | '\t' | '│' | '├' | '└' | '─')
}

/// Split a line into the number of tree-drawing characters it starts with
/// and the text after them.
fn strip_tree(line: &str) -> (usize, &str) {
    let mut count = 0usize;
    for (index, c) in line.char_indices() {
        if !is_tree_char(c) {
            return (count, &line[index..]);
        }
        count += 1;
    }
    (count, "")
}

/// The bullet column and operator name when `line` opens an operator.
fn bullet(line: &str) -> Option<(usize, &str)> {
    let (column, text) = strip_tree(line);
    text.strip_prefix('•').map(|name| (column, name))
}

fn split_name_value(text: &str) -> Option<(&str, &str)> {
    if let Some((name, value)) = text.split_once(": ") {
        let name = name.trim();
        return (!name.is_empty()).then_some((name, value.trim()));
    }
    let name = text.strip_suffix(':')?.trim();
    (!name.is_empty()).then_some((name, ""))
}

fn header_line(plan: &mut ExecutionPlan, text: &str) {
    if let Some((name, value)) = split_name_value(text) {
        match name {
            "planning time" => plan.planning_time_ms = parse_duration_ms(value),
            "execution time" => plan.execution_time_ms = parse_duration_ms(value),
            "sql cpu time" => plan.cpu_time_ms = parse_duration_ms(value),
            _ => {}
        }
    }
    plan.notes.push(text.to_string());
}

fn new_node(id: i32, name: &str) -> PlanNode {
    let mut node = PlanNode {
        id,
        logical_op: name.to_string(),
        physical_op: name.to_string(),
        ..Default::default()
    };
    let (base, qualifiers) = split_qualifiers(name);
    if base.ends_with("join") && base != "index join" {
        // Plain EXPLAIN leaves the type off inner joins; VERBOSE and ANALYZE
        // (VERBOSE) print `(inner)`.
        let join_type = qualifiers
            .iter()
            .find_map(|q| join_type_name(q))
            .unwrap_or("Inner");
        node.join_type = Some(join_type.to_string());
    }
    node
}

/// `lookup join (anti) (streamer)` gives `("lookup join", ["anti",
/// "streamer"])`.
fn split_qualifiers(name: &str) -> (&str, Vec<&str>) {
    let Some(open) = name.find(" (") else {
        return (name.trim(), Vec::new());
    };
    let qualifiers = name[open..]
        .split('(')
        .filter_map(|part| part.split(')').next())
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    (name[..open].trim(), qualifiers)
}

fn join_type_name(qualifier: &str) -> Option<&'static str> {
    Some(match qualifier {
        "inner" => "Inner",
        "left outer" => "Left",
        "right outer" => "Right",
        "full outer" => "Full",
        "semi" => "Semi",
        "anti" => "Anti",
        "right semi" => "Right Semi",
        "right anti" => "Right Anti",
        "cross" => "Cross",
        _ => return None,
    })
}

/// File one line below an operator bullet. Lines without a `name: value`
/// shape are flags such as `right cols are key` or `auto commit`.
fn node_line(node: &mut PlanNode, text: &str) {
    let Some((name, value)) = split_name_value(text) else {
        node.extra.push(text.to_string());
        return;
    };
    match name {
        "estimated row count" => {
            // `20,000 (100% of the table; stats collected 2 seconds ago)`
            let count = value.split(" (").next().unwrap_or(value);
            if let Some(rows) = common::parse_count(count) {
                node.estimated_rows = rows;
            }
            if count.len() != value.len() || common::parse_count(count).is_none() {
                push_property(node, name, value);
            }
        }
        "actual row count" => {
            if let Some(rows) = common::parse_count(value) {
                node.actual_rows = Some(rows);
                node.actual_rows_total = Some(rows);
            } else {
                push_property(node, name, value);
            }
        }
        "execution time" => match parse_duration_ms(value) {
            Some(ms) => node.actual_self_time_ms = Some(ms),
            None => push_property(node, name, value),
        },
        "KV time" => {
            // Scans, index joins and lookup joins report the time spent
            // waiting on the key-value layer instead of an execution time.
            if node.actual_self_time_ms.is_none() {
                node.actual_self_time_ms = parse_duration_ms(value);
            }
            push_property(node, name, value);
        }
        "sql cpu time" => match parse_duration_ms(value) {
            Some(ms) => node.actual_cpu_ms = Some(ms),
            None => push_property(node, name, value),
        },
        "KV rows decoded" => {
            node.rows_read = common::parse_count(value);
            push_property(node, name, value);
        }
        "estimated max memory allocated" => match common::parse_size_kb(value) {
            Some(kb) => node.peak_memory_kb = Some(kb),
            None => push_property(node, name, value),
        },
        "estimated max sql temp disk usage" => {
            if let Some(kb) = common::parse_size_kb(value).filter(|kb| *kb > 0.0) {
                node.spill = Some(PlanSpill {
                    written_kb: Some(kb),
                    ..Default::default()
                });
                node.warnings.push(PlanWarning::SpillToTempDb);
            }
            push_property(node, name, value);
        }
        "table" => {
            // `orders@orders_pkey`; mutations print the bare table name.
            let (table, index) = match value.split_once('@') {
                Some((table, index)) => (table, Some(index)),
                None => (value, None),
            };
            node.relation = Some(table.to_string());
            node.index_name = index.map(str::to_string);
        }
        "spans" => {
            if value == "FULL SCAN" || value.starts_with("FULL SCAN ") {
                push_property(node, name, value);
            } else {
                node.index_cond = Some(value.to_string());
            }
        }
        "filter" => node.filter = Some(value.to_string()),
        "equality" => node.join_predicate = Some(value.to_string()),
        "pred" => {
            // The part of a join condition that is not an equality.
            if node.join_type.is_some() && node.join_predicate.is_none() {
                node.join_predicate = Some(value.to_string());
            } else {
                push_property(node, name, value);
            }
        }
        "order" => node.sort_keys = split_top_level(value),
        "group by" => node.group_keys = split_top_level(value),
        "columns" => {
            let inner = value
                .strip_prefix('(')
                .and_then(|v| v.strip_suffix(')'))
                .unwrap_or(value);
            node.output_columns = split_top_level(inner);
        }
        "id" if node.physical_op == "subquery" => node.subplan_name = Some(value.to_string()),
        "label" => {
            // `buffer 1 (big)`: the buffer a WITH query was materialized
            // into, with the query's name in parentheses.
            if let Some(cte) = value
                .rsplit_once(" (")
                .and_then(|(_, tail)| tail.strip_suffix(')'))
            {
                node.cte_name = Some(cte.to_string());
            }
            push_property(node, name, value);
        }
        _ => push_property(node, name, value),
    }
}

fn push_property(node: &mut PlanNode, name: &str, value: &str) {
    node.properties.push(PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    });
}

/// Derive the search strings and warnings once all of a node's lines are in.
fn finish_node(node: &mut PlanNode) {
    let full_scan = node
        .property("spans")
        .is_some_and(|spans| spans.starts_with("FULL SCAN"));
    if let Some(relation) = node.relation.clone() {
        node.output_list.push(relation.clone());
        if let Some(index) = &node.index_name {
            node.output_list.push(format!("Index: {index}"));
        }
        if full_scan {
            node.warnings.push(PlanWarning::TableScan {
                table: relation.clone(),
                schema: String::new(),
            });
        }
        if split_qualifiers(&node.physical_op).0 == "index join" {
            node.warnings.push(PlanWarning::KeyLookup {
                table: relation,
                index: node.index_name.clone().unwrap_or_default(),
            });
        }
    }
    if let Some(cond) = &node.index_cond {
        node.output_list.push(format!("Index Cond: {cond}"));
    }
    if let Some(filter) = &node.filter {
        node.output_list.push(format!("Filter: {filter}"));
    }
    if let Some(predicate) = &node.join_predicate {
        node.output_list.push(format!("Condition: {predicate}"));
    }
}

/// `actual_time_ms` for every node: its own time plus the inclusive time of
/// its inputs. Children always follow their parent in document order, so one
/// reverse pass sees every input before the node that reads it.
fn fill_inclusive_times(nodes: &mut [PlanNode], edges: &[PlanEdge]) {
    let mut inclusive: Vec<Option<f64>> = vec![None; nodes.len()];
    let mut inputs: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for edge in edges {
        let (from, to) = (edge.from_node as usize, edge.to_node as usize);
        if from >= 1 && to >= 1 && from <= nodes.len() && to <= nodes.len() {
            inputs[from - 1].push(to - 1);
        }
    }
    for index in (0..nodes.len()).rev() {
        let mut total = nodes[index].actual_self_time_ms;
        for input in &inputs[index] {
            if let Some(ms) = inclusive.get(*input).copied().flatten() {
                total = Some(total.unwrap_or(0.0) + ms);
            }
        }
        inclusive[index] = total;
        nodes[index].actual_time_ms = total;
    }
}

/// What follows the tree: `index recommendations: N` with numbered entries,
/// and the `Diagram:` URL of `EXPLAIN (DISTSQL)`.
fn trailer_lines(plan: &mut ExecutionPlan, lines: &[String]) {
    let mut current: Option<String> = None;
    for line in lines {
        let text = line.trim();
        if text.starts_with("Diagram: ") {
            plan.notes.push(
                "EXPLAIN (DISTSQL) diagram link left out; the plan above it was read.".to_string(),
            );
            continue;
        }
        if text.starts_with("index recommendations:") {
            continue;
        }
        let numbered = text
            .split_once(". ")
            .filter(|(number, _)| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()));
        if let Some((number, rest)) = numbered {
            if let Some(done) = current.take() {
                recommendation(plan, done);
            }
            current = Some(format!("index recommendation {number}: {rest}"));
        } else if let Some(open) = current.as_mut() {
            open.push_str("; ");
            open.push_str(text);
        } else {
            plan.notes.push(text.to_string());
        }
    }
    if let Some(done) = current.take() {
        recommendation(plan, done);
    }
}

fn recommendation(plan: &mut ExecutionPlan, note: String) {
    if note.contains("type: index creation")
        && let Some(index) = missing_index(&note)
    {
        plan.warnings.push(PlanWarning::MissingIndex(index));
    }
    plan.notes.push(note);
}

/// Read `CREATE INDEX ON db.schema.table (a, b) STORING (c);`. The key
/// columns go in `equality_columns`: the recommendation does not say which
/// of them serve an equality and which a range.
fn missing_index(note: &str) -> Option<MissingIndex> {
    let rest = note.split_once("CREATE INDEX ON ")?.1;
    let (name, rest) = rest.split_once(" (")?;
    let (keys, rest) = rest.split_once(')')?;
    let included = rest
        .trim_start()
        .strip_prefix("STORING (")
        .and_then(|tail| tail.split_once(')'))
        .map(|(columns, _)| split_top_level(columns))
        .unwrap_or_default();
    let mut parts: Vec<&str> = name.trim().split('.').collect();
    let table = parts.pop()?.to_string();
    let schema = parts.pop().unwrap_or_default().to_string();
    let database = parts.pop().unwrap_or_default().to_string();
    if table.is_empty() {
        return None;
    }
    Some(MissingIndex {
        database,
        schema,
        table,
        equality_columns: split_top_level(keys),
        inequality_columns: Vec::new(),
        included_columns: included,
        impact: 0.0,
    })
}

fn no_operators_message(lines: &[String]) -> String {
    let text = lines.join("\n");
    if text.contains("Schema change plan for") {
        return "CockroachDB EXPLAIN (DDL) describes schema change stages, not a query plan. Paste the output of EXPLAIN or EXPLAIN ANALYZE.".to_string();
    }
    if text.contains("*colexec") || text.contains("*colfetcher") {
        return "CockroachDB EXPLAIN (VEC) lists vectorized operators without estimates. Paste the output of EXPLAIN or EXPLAIN ANALYZE.".to_string();
    }
    if text.contains("├── ") || text.contains("└── ") {
        return "CockroachDB EXPLAIN (OPT) output is the optimizer's expression tree and is not supported. Paste the output of EXPLAIN, EXPLAIN (VERBOSE) or EXPLAIN ANALYZE.".to_string();
    }
    "CockroachDB EXPLAIN: no `•` operator lines found. Paste the full output of EXPLAIN or EXPLAIN ANALYZE.".to_string()
}

/// Read a duration the way CockroachDB prints them (`514µs`, `7ms`, `1.2s`,
/// `1m30s`, `2h`) into milliseconds.
fn parse_duration_ms(text: &str) -> Option<f64> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == ','))
            .unwrap_or(rest.len());
        let number: String = rest[..digits].chars().filter(|c| *c != ',').collect();
        let value: f64 = number.parse().ok()?;
        rest = &rest[digits..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let scale = match rest[..unit_len].trim() {
            "ns" => 1e-6,
            "µs" | "μs" | "us" => 1e-3,
            "ms" => 1.0,
            "s" => 1e3,
            "m" => 6e4,
            "h" => 3.6e6,
            _ => return None,
        };
        total += value * scale;
        rest = &rest[unit_len..];
    }
    total.is_finite().then_some(total)
}

/// Split on commas that are not inside parentheses, brackets or quotes.
fn split_top_level(text: &str) -> Vec<String> {
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
                ',' if depth <= 0 => {
                    parts.push(current.trim().to_string());
                    current.clear();
                }
                _ => current.push(c),
            },
        }
    }
    parts.push(current.trim().to_string());
    parts.retain(|part| !part.is_empty());
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q2_EXPLAIN_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-explain-tsv.txt");
    const Q2_EXPLAIN_TABLE: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-explain-table.txt");
    const Q2_EXPLAIN_RECORDS: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-explain-records.txt");
    const Q2_EXPLAIN_RAW: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-explain-raw.txt");
    const Q2_ANALYZE_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-analyze-tsv.txt");
    const Q2_DISTSQL_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q2-distsql-tsv.txt");
    const Q3_EXPLAIN_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q3-explain-tsv.txt");
    const Q4_VERBOSE_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q4-verbose-tsv.txt");
    const Q5_EXPLAIN_TABLE: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q5-explain-table.txt");
    const Q6_ANALYZE_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q6-analyze-tsv.txt");
    const Q7_ANALYZE_VERBOSE_TSV: &str =
        include_str!("../../../tests/golden/plans/cockroachdb/crdb243-q7-analyze-verbose-tsv.txt");
    /// `EXPLAIN (OPT)` from CockroachDB v24.3.37, as the client prints it.
    const Q2_OPT_TSV: &str = "info\nsort\n\" └── group-by (hash)\"\n\"      ├── inner-join (hash)\"\n\"      │    ├── scan orders [as=o]\"\n\"      │    ├── scan customers@customers_region_idx [as=c]\"\n\"      │    │    └── constraint: /3/1\"\n\"      │    └── filters\"\n\"      │         └── customer_id = c.id\"\n\"      └── aggregations\"\n\"           ├── count-rows\"\n\"           └── sum\"\n";

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
    fn estimated_plan_reads_tree_and_estimates() {
        let plan = parse_cockroach_explain_text(Q2_EXPLAIN_TSV).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("cockroachdb-text"));
        assert!(plan.dialect.is_none());
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(plan.notes, vec!["distribution: local", "vectorized: true"]);
        assert_eq!(
            ops(&plan),
            vec!["sort", "group (hash)", "hash join", "scan", "scan"]
        );
        assert_eq!(children(&plan, 1), vec![2]);
        assert_eq!(children(&plan, 2), vec![3]);
        assert_eq!(children(&plan, 3), vec![4, 5]);
        assert_eq!(plan.estimated_rows, 2.0);

        assert_eq!(plan.nodes[0].sort_keys, vec!["-sum"]);
        assert_eq!(plan.nodes[1].group_keys, vec!["region"]);
        let join = &plan.nodes[2];
        assert_eq!(join.estimated_rows, 10_000.0);
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.join_predicate.as_deref(), Some("(customer_id) = (id)"));
        assert_eq!(join.extra, vec!["right cols are key"]);

        let orders = &plan.nodes[3];
        assert_eq!(orders.estimated_rows, 20_000.0);
        assert_eq!(orders.relation.as_deref(), Some("orders"));
        assert_eq!(orders.index_name.as_deref(), Some("orders_pkey"));
        assert!(orders.index_cond.is_none());
        assert!(matches!(
            orders.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "orders"
        ));
        assert!(
            orders
                .property("estimated row count")
                .unwrap()
                .contains("100% of the table")
        );

        let customers = &plan.nodes[4];
        assert_eq!(customers.estimated_rows, 500.0);
        assert_eq!(
            customers.index_name.as_deref(),
            Some("customers_region_idx")
        );
        assert_eq!(
            customers.index_cond.as_deref(),
            Some("[/'east' - /'east'] [/'west' - /'west']")
        );
        assert!(customers.warnings.is_empty());
        assert!(
            customers
                .output_list
                .contains(&"Index: customers_region_idx".to_string())
        );
    }

    #[test]
    fn every_client_format_gives_the_same_plan() {
        let reference = parse_cockroach_explain_text(Q2_EXPLAIN_TSV).unwrap();
        for doc in [Q2_EXPLAIN_TABLE, Q2_EXPLAIN_RECORDS, Q2_EXPLAIN_RAW] {
            let plan = parse_cockroach_explain_text(doc).unwrap();
            assert_eq!(ops(&plan), ops(&reference));
            assert_eq!(plan.edges.len(), reference.edges.len());
            assert_eq!(children(&plan, 3), vec![4, 5]);
            assert_eq!(plan.nodes[3].estimated_rows, 20_000.0);
            assert_eq!(plan.nodes[4].relation.as_deref(), Some("customers"));
            assert_eq!(plan.notes, reference.notes);
        }
    }

    #[test]
    fn analyze_reads_actuals_and_times() {
        let plan = parse_cockroach_explain_text(Q2_ANALYZE_TSV).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        assert_eq!(plan.planning_time_ms, Some(0.514));
        assert_eq!(plan.execution_time_ms, Some(7.0));
        assert_eq!(plan.cpu_time_ms, Some(3.0));
        assert!(
            plan.notes
                .contains(&"rows decoded from KV: 20,500 (1.1 MiB, 2 gRPC calls)".to_string())
        );
        assert!(plan.notes.contains(&"distribution: full".to_string()));

        let sort = &plan.nodes[0];
        assert_eq!(sort.actual_rows, Some(2.0));
        assert_eq!(sort.actual_rows_total, Some(2.0));
        assert_eq!(sort.actual_self_time_ms, Some(0.007));
        assert_eq!(sort.actual_cpu_ms, Some(0.007));
        assert_eq!(sort.peak_memory_kb, Some(10.0));
        assert!(sort.spill.is_none());
        assert_eq!(sort.property("sql nodes"), Some("n1"));
        assert!(sort.loops.is_none());

        let group = &plan.nodes[1];
        assert_eq!(group.actual_self_time_ms, Some(0.497));
        assert_eq!(group.peak_memory_kb, Some(1024.0));

        let join = &plan.nodes[2];
        assert_eq!(join.actual_rows, Some(10_000.0));
        assert_eq!(join.estimated_rows, 10_000.0);

        let orders = &plan.nodes[3];
        assert_eq!(orders.actual_rows, Some(20_000.0));
        assert_eq!(orders.rows_read, Some(20_000.0));
        assert_eq!(orders.actual_self_time_ms, Some(5.0));
        assert_eq!(orders.actual_time_ms, Some(5.0));
        assert_eq!(orders.actual_cpu_ms, Some(2.0));
        assert_eq!(orders.property("KV bytes read"), Some("1.1 MiB"));

        let customers = &plan.nodes[4];
        assert_eq!(customers.actual_rows, Some(500.0));
        assert_eq!(customers.actual_self_time_ms, Some(0.307));

        // Inclusive time is the node's own time plus its inputs'.
        let expected_join = 0.349 + 5.0 + 0.307;
        assert!((join.actual_time_ms.unwrap() - expected_join).abs() < 1e-9);
        let expected_root = 0.007 + 0.497 + expected_join;
        assert!((sort.actual_time_ms.unwrap() - expected_root).abs() < 1e-9);
    }

    #[test]
    fn verbose_reads_columns_join_types_and_recommendations() {
        let plan = parse_cockroach_explain_text(Q4_VERBOSE_TSV).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                "union all",
                "project",
                "lookup join (anti)",
                "hash join (semi)",
                "scan",
                "filter",
                "scan",
                "project",
                "filter",
                "scan",
            ]
        );
        assert_eq!(children(&plan, 1), vec![2, 8]);
        assert_eq!(children(&plan, 4), vec![5, 6]);
        assert_eq!(plan.nodes[0].estimated_rows, 1043.0);
        assert_eq!(plan.nodes[0].output_columns, vec!["name"]);

        let anti = &plan.nodes[2];
        assert_eq!(anti.join_type.as_deref(), Some("Anti"));
        assert_eq!(anti.relation.as_deref(), Some("order_items"));
        assert_eq!(anti.join_predicate.as_deref(), Some("(id) = (order_id)"));
        assert_eq!(anti.property("pred"), Some("qty = 5"));
        assert_eq!(anti.output_columns, vec!["id", "name"]);

        let semi = &plan.nodes[3];
        assert_eq!(semi.join_type.as_deref(), Some("Semi"));
        assert_eq!(semi.estimated_rows, 80.0);

        let filter = &plan.nodes[5];
        assert_eq!(filter.filter.as_deref(), Some("total > 499"));
        assert!(
            filter
                .output_list
                .contains(&"Filter: total > 499".to_string())
        );

        assert_eq!(plan.warnings.len(), 3);
        let PlanWarning::MissingIndex(second) = &plan.warnings[1] else {
            panic!("expected a missing index warning");
        };
        assert_eq!(second.database, "shop");
        assert_eq!(second.schema, "public");
        assert_eq!(second.table, "order_items");
        assert_eq!(second.equality_columns, vec!["price"]);
        assert_eq!(second.included_columns, vec!["sku"]);
        assert!(plan.notes.iter().any(|note| note
            == "index recommendation 1: type: index creation; SQL command: CREATE INDEX ON shop.public.order_items (qty);"));
    }

    #[test]
    fn index_join_and_replacement_recommendation() {
        let plan = parse_cockroach_explain_text(Q3_EXPLAIN_TSV).unwrap();
        assert_eq!(
            ops(&plan),
            vec!["limit", "lookup join", "sort", "index join", "scan"]
        );
        assert_eq!(plan.nodes[0].property("count"), Some("10"));
        assert_eq!(plan.nodes[1].estimated_rows, 59.0);
        assert_eq!(plan.nodes[1].join_type.as_deref(), Some("Inner"));
        let index_join = &plan.nodes[3];
        assert!(index_join.join_type.is_none());
        assert!(matches!(
            index_join.warnings.as_slice(),
            [PlanWarning::KeyLookup { table, index }] if table == "orders" && index == "orders_pkey"
        ));
        let scan = &plan.nodes[4];
        assert_eq!(scan.index_cond.as_deref(), Some("[/42 - /42]"));
        assert_eq!(scan.estimated_rows, 20.0);
        // An index replacement is a note, not a missing index.
        assert!(plan.warnings.is_empty());
        assert!(
            plan.notes
                .iter()
                .any(|note| note.starts_with("index recommendation 1: type: index replacement"))
        );
    }

    #[test]
    fn single_operator_table_output() {
        let plan = parse_cockroach_explain_text(Q5_EXPLAIN_TABLE).unwrap();
        assert_eq!(ops(&plan), vec!["scan"]);
        assert!(plan.edges.is_empty());
        let scan = &plan.nodes[0];
        assert_eq!(scan.estimated_rows, 1.0);
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.index_cond.as_deref(), Some("[/77 - /77]"));
        assert!(
            scan.property("estimated row count")
                .unwrap()
                .contains("<0.01% of the table")
        );
    }

    #[test]
    fn mutation_with_subquery() {
        let plan = parse_cockroach_explain_text(Q6_ANALYZE_TSV).unwrap();
        assert_eq!(
            ops(&plan),
            vec![
                "root",
                "update",
                "render",
                "filter",
                "index join",
                "scan",
                "subquery",
                "group (scalar)",
                "scan",
            ]
        );
        assert_eq!(children(&plan, 1), vec![2, 7]);
        assert_eq!(plan.execution_time_ms, Some(6.0));
        let update = &plan.nodes[1];
        assert_eq!(update.relation.as_deref(), Some("orders"));
        assert!(update.index_name.is_none());
        assert_eq!(update.actual_rows, Some(1.0));
        assert_eq!(update.property("set"), Some("status"));
        assert_eq!(update.extra, vec!["auto commit"]);
        let filter = &plan.nodes[3];
        assert_eq!(filter.filter.as_deref(), Some("total > @S1"));
        assert_eq!(filter.actual_rows, Some(0.0));
        assert_eq!(filter.estimated_rows, 6.0);
        let subquery = &plan.nodes[6];
        assert_eq!(subquery.subplan_name.as_deref(), Some("@S1"));
        assert_eq!(
            subquery.property("original sql"),
            Some("(SELECT avg(total) FROM orders)")
        );
        assert_eq!(subquery.property("exec mode"), Some("one row"));
        assert_eq!(plan.nodes[8].actual_rows, Some(20_000.0));
    }

    #[test]
    fn analyze_verbose_window_and_buffer() {
        let plan = parse_cockroach_explain_text(Q7_ANALYZE_VERBOSE_TSV).unwrap();
        assert_eq!(plan.nodes.len(), 11);
        assert_eq!(plan.planning_time_ms, Some(0.43));
        let window = &plan.nodes[2];
        assert_eq!(window.physical_op, "window");
        assert_eq!(window.actual_rows, Some(500.0));
        assert_eq!(window.estimated_rows, 333.0);
        assert_eq!(window.property("vectorized batch count"), Some("1"));
        assert!(
            window
                .property("window 0")
                .unwrap()
                .starts_with("row_number() OVER")
        );
        let join = &plan.nodes[3];
        assert_eq!(join.physical_op, "hash join (inner)");
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.extra, vec!["left cols are key", "right cols are key"]);
        let scan_buffer = &plan.nodes[6];
        assert_eq!(scan_buffer.physical_op, "scan buffer");
        assert_eq!(scan_buffer.cte_name.as_deref(), Some("big"));
        let scan = &plan.nodes[4];
        assert_eq!(
            scan.property("MVCC step count (ext/int)"),
            Some("1,000/1,000")
        );
        assert_eq!(scan.output_columns, vec!["id", "name"]);
        let group = &plan.nodes[9];
        assert_eq!(group.physical_op, "group (hash)");
        assert_eq!(group.group_keys, vec!["customer_id"]);
        assert_eq!(group.property("aggregate 0"), Some("sum(total)"));
        assert_eq!(group.actual_self_time_ms, Some(1.0));
    }

    #[test]
    fn distsql_plan_is_read_and_the_url_dropped() {
        let plan = parse_cockroach_explain_text(Q2_DISTSQL_TSV).unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert!(plan.notes.iter().all(|note| !note.contains("https://")));
        assert!(plan.notes.iter().any(|note| note.contains("diagram")));
    }

    #[test]
    fn other_explain_modes_are_refused_with_a_reason() {
        let error = parse_cockroach_explain_text(Q2_OPT_TSV).unwrap_err();
        assert!(error.to_string().contains("EXPLAIN (OPT)"));
        assert!(!looks_like_cockroach_explain_text(Q2_OPT_TSV));
        assert!(parse_cockroach_explain_text("").is_err());
        assert!(parse_cockroach_explain_text("info\ndistribution: local\n").is_err());
    }

    #[test]
    fn detector_accepts_own_formats_only() {
        for doc in [
            Q2_EXPLAIN_TSV,
            Q2_EXPLAIN_TABLE,
            Q2_EXPLAIN_RECORDS,
            Q2_EXPLAIN_RAW,
            Q2_ANALYZE_TSV,
            Q2_DISTSQL_TSV,
            Q3_EXPLAIN_TSV,
            Q4_VERBOSE_TSV,
            Q5_EXPLAIN_TABLE,
            Q6_ANALYZE_TSV,
            Q7_ANALYZE_VERBOSE_TSV,
        ] {
            assert!(looks_like_cockroach_explain_text(doc));
        }
        for doc in [
            "Seq Scan on t  (cost=0.00..1.50 rows=50 width=4)\n  ->  Index Scan using i on u  (cost=0.1..2.0 rows=1 width=4)",
            "-> Limit: 10 row(s)  (cost=1.2 rows=10)\n    -> Table scan on t  (cost=1.2 rows=100)",
            "• a bullet point in some notes\n• another one",
            "Fragment 0 [SINGLE]\n    Output[columnNames = [a]]\n    │   Layout: [a:bigint]\n    └─ TableScan[table = tpch:tiny:nation]",
            "== Physical Plan ==\n*(1) Project [id#1L]\n+- *(1) Range (0, 10, step=1, splits=7)",
            "",
        ] {
            assert!(!looks_like_cockroach_explain_text(doc));
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("514µs"), Some(0.514));
        assert_eq!(parse_duration_ms("7ms"), Some(7.0));
        assert_eq!(parse_duration_ms("1.5s"), Some(1500.0));
        assert_eq!(parse_duration_ms("1m30s"), Some(90_000.0));
        assert_eq!(parse_duration_ms("0µs"), Some(0.0));
        assert_eq!(parse_duration_ms("soon"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn never_panics_on_mangled_input() {
        for sample in [
            Q2_EXPLAIN_TSV,
            Q2_EXPLAIN_TABLE,
            Q2_EXPLAIN_RECORDS,
            Q2_EXPLAIN_RAW,
            Q4_VERBOSE_TSV,
            Q6_ANALYZE_TSV,
            Q7_ANALYZE_VERBOSE_TSV,
            Q2_OPT_TSV,
        ] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_cockroach_explain_text(doc);
                let _ = looks_like_cockroach_explain_text(doc);
            });
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut doc = String::from("distribution: local\n");
        for depth in 0..600 {
            doc.push_str(&" ".repeat(depth * 4));
            doc.push_str("• render\n");
        }
        assert!(parse_cockroach_explain_text(&doc).is_err());
    }
}
