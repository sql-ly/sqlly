//! Trino `EXPLAIN` and `EXPLAIN ANALYZE [VERBOSE]` text.
//!
//! A distributed plan is a list of fragments, each holding one operator tree:
//!
//! ```text
//! Trino version: 484
//! Fragment 0 [SINGLE]
//!     Output layout: [name, sum]
//!     Output[columnNames = [name, t]]
//!     │   Layout: [name:varchar(25), sum:double]
//!     │   Estimates: {rows: 900 (28.13kB), cpu: 0, memory: 0B, network: 0B}
//!     └─ RemoteSource[sourceFragmentIds = [1]]
//!            Layout: [name:varchar(25), sum:double]
//!
//! Fragment 1 [SOURCE]
//!     ...
//! ```
//!
//! An operator is `Name[key = value, ...]`; a child is introduced by `└─` or
//! `├─` and its facts sit four columns right of its name. The fragments are
//! joined into one tree: the root of fragment N hangs under the
//! `RemoteSource` or `RemoteMerge` whose `sourceFragmentIds` names N.
//!
//! `EXPLAIN (TYPE LOGICAL)` on Trino 435 prints one tree without fragments
//! (the exchanges appear as `RemoteExchange` operators); that is read too.
//! The Trino CLI's ALIGNED, VERTICAL, CSV and TSV renderings are all undone
//! here.

use super::common;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// Deepest operator nesting accepted before the document is refused.
const MAX_DEPTH: usize = 512;

/// One `Fragment N [...]` section.
struct Fragment {
    number: String,
    root: Option<i32>,
    /// Header lines seen before the fragment's first operator.
    header: Vec<PlanProperty>,
}

/// Parse the text of a Trino `EXPLAIN` or `EXPLAIN ANALYZE`.
///
/// `EXPLAIN ANALYZE` reports per operator `CPU`, `Scheduled` (wall time the
/// operator was scheduled for) and `Output` rows. `Scheduled` is the
/// operator's own time, so it fills `actual_self_time_ms`, and
/// `actual_time_ms` is that plus the same figure for every input below it,
/// across fragments. Trino prints estimates per operator, not as a
/// cumulative cost, so `estimated_cost` stays 0 and the `Estimates` line is
/// kept whole in `properties`.
pub fn parse_trino_explain_text(doc: &str) -> PlanResult<ExecutionPlan> {
    let lines = clean_lines(doc);
    if let Some(reason) = unsupported_shape(&lines) {
        return Err(PlanError::InvalidStructure(reason.to_string()));
    }
    let mut plan = ExecutionPlan {
        source_format: Some("trino-text".to_string()),
        ..Default::default()
    };
    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    let mut fragments: Vec<Fragment> = Vec::new();
    // Ancestors of the operator being read within its fragment, as
    // (name column, node id).
    let mut stack: Vec<(usize, i32)> = Vec::new();
    // The operator whose facts are being read, as (name column, node index).
    let mut current: Option<(usize, usize)> = None;

    for line in &lines {
        let (column, connector, text) = split_tree(line);
        let text = text.trim_end();
        if text.is_empty() {
            continue;
        }
        if !connector && let Some(number) = fragment_number(text) {
            let value = text.strip_prefix("Fragment ").unwrap_or(text).to_string();
            fragments.push(Fragment {
                number,
                root: None,
                header: vec![PlanProperty {
                    name: "Fragment".to_string(),
                    value,
                }],
            });
            stack.clear();
            current = None;
            continue;
        }
        let open_root = match fragments.last() {
            Some(fragment) => fragment.root.is_none() && operator_head(text, false),
            None => nodes.is_empty() && operator_head(text, true),
        };
        if connector || open_root {
            while stack.last().is_some_and(|(c, _)| *c >= column) {
                stack.pop();
            }
            if stack.len() >= MAX_DEPTH {
                return Err(PlanError::InvalidStructure(format!(
                    "Trino EXPLAIN: operators nest deeper than {MAX_DEPTH} levels"
                )));
            }
            let id = nodes.len() as i32 + 1;
            let mut node = new_node(id, text);
            match stack.last() {
                Some((_, parent)) => edges.push(PlanEdge {
                    from_node: *parent,
                    to_node: id,
                }),
                None => {
                    if let Some(fragment) = fragments.last_mut()
                        && fragment.root.is_none()
                    {
                        fragment.root = Some(id);
                        node.properties.append(&mut fragment.header);
                    }
                }
            }
            stack.push((column, id));
            current = Some((column, nodes.len()));
            nodes.push(node);
            continue;
        }
        match current.and_then(|(op_column, index)| Some((op_column, nodes.get_mut(index)?))) {
            Some((op_column, node)) => {
                // Facts sit four columns right of the operator name; anything
                // further right continues the fact above it.
                if column > op_column + 4 && !node.properties.is_empty() {
                    continuation(node, text);
                } else {
                    node_line(node, text);
                }
            }
            None => match fragments.last_mut() {
                Some(fragment) if fragment.root.is_none() => {
                    let (name, value) = match text.split_once(": ") {
                        Some((name, value)) => (name.trim(), value.trim()),
                        None => (text, ""),
                    };
                    fragment.header.push(PlanProperty {
                        name: format!("Fragment {name}"),
                        value: value.to_string(),
                    });
                }
                _ => header_line(&mut plan, text),
            },
        }
    }

    if nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "Trino EXPLAIN: no operator lines such as `Output[...]` or `TableScan[...]` found. Paste the full output of EXPLAIN or EXPLAIN ANALYZE.".to_string(),
        ));
    }
    link_fragments(&nodes, &fragments, &mut edges);
    for node in &mut nodes {
        finish_node(node);
    }
    let executed = nodes.iter().any(|node| node.actual_rows.is_some());
    if executed {
        fill_inclusive_times(&mut nodes, &edges);
    }
    plan.statement_type = if executed || plan.execution_time_ms.is_some() {
        "EXPLAIN ANALYZE".to_string()
    } else {
        "EXPLAIN".to_string()
    };
    plan.nodes = nodes;
    plan.edges = edges;
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// True for Trino plan text: a `Layout: [` fact line (every operator has
/// one) together with a `Fragment N [` header, the `Trino version:` line or
/// an `Estimates: {rows:` line.
pub fn looks_like_trino_explain_text(doc: &str) -> bool {
    let lines = clean_lines(doc);
    let mut layout = false;
    let mut marker = false;
    for line in &lines {
        let (_, connector, text) = split_tree(line);
        if text.starts_with("Layout: [") {
            layout = true;
        } else if text.starts_with("Estimates: {rows:")
            || text.starts_with("Trino version:")
            || (!connector && fragment_number(text).is_some())
        {
            marker = true;
        }
    }
    layout && marker
}

/// Undo the Trino CLI's renderings, one entry per plan line: the quoted
/// single cell of CSV, the `\n` escapes of TSV, the header, rule and
/// `(1 row)` footer of ALIGNED, and the `Query Plan | ` gutter of VERTICAL.
fn clean_lines(doc: &str) -> Vec<String> {
    let mut text = doc.trim().to_string();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text = text[1..text.len() - 1].replace("\"\"", "\"");
    }
    if text.contains("\\n") && !text.contains("\n    ") {
        text = text.replace("\\n", "\n");
    }
    let mut lines = Vec::new();
    let mut gutter: Option<usize> = None;
    for raw in text.lines() {
        let mut line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.starts_with("-[ RECORD") || trimmed == "Query Plan" {
            continue;
        }
        if !trimmed.is_empty() && trimmed.chars().all(|c| c == '-' || c == '+') {
            continue;
        }
        if is_row_count_footer(trimmed) {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Query Plan |") {
            gutter = Some(line.len() - rest.len());
            line = rest;
        } else if let Some(width) = gutter
            && let (Some(head), Some(rest)) = (line.get(..width), line.get(width..))
            && head.trim() == "|"
        {
            line = rest;
        }
        if gutter.is_some() {
            line = line.strip_prefix(' ').unwrap_or(line);
        }
        lines.push(line.to_string());
    }
    lines
}

/// `(1 row)` / `(25 rows)`.
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

/// The other things `EXPLAIN` can print, which are not operator trees.
fn unsupported_shape(lines: &[String]) -> Option<&'static str> {
    let mut content = lines
        .iter()
        .map(|line| line.trim())
        .filter(|l| !l.is_empty());
    let first = content.next()?;
    let second = content.next().unwrap_or("");
    if first.starts_with("digraph ") {
        return Some(
            "Trino EXPLAIN (FORMAT GRAPHVIZ) output is a drawing, not a plan the viewer can read. Paste the output of EXPLAIN or EXPLAIN ANALYZE in the default text format.",
        );
    }
    if first == "Valid" && (second == "true" || second == "false") {
        return Some(
            "Trino EXPLAIN (TYPE VALIDATE) only says whether the statement is valid. Paste the output of EXPLAIN or EXPLAIN ANALYZE.",
        );
    }
    if first.starts_with('{') {
        if lines
            .iter()
            .any(|line| line.contains("\"inputTableColumnInfos\""))
        {
            return Some(
                "Trino EXPLAIN (TYPE IO) lists the tables a statement reads and writes, not its plan. Paste the output of EXPLAIN or EXPLAIN ANALYZE.",
            );
        }
        return Some(
            "Trino EXPLAIN (FORMAT JSON) is not supported. Paste the output of EXPLAIN or EXPLAIN ANALYZE in the default text format.",
        );
    }
    None
}

fn is_tree_char(c: char) -> bool {
    matches!(c, ' ' | '\t' | '│' | '├' | '└' | '─')
}

/// Split a line into the column its text starts at, whether a `└─` / `├─`
/// connector leads into it, and the text.
fn split_tree(line: &str) -> (usize, bool, &str) {
    let mut column = 0usize;
    let mut connector = false;
    for (index, c) in line.char_indices() {
        if !is_tree_char(c) {
            return (column, connector, &line[index..]);
        }
        connector |= c == '─';
        column += 1;
    }
    (column, false, "")
}

/// The number in `Fragment 3 [HASH]`.
fn fragment_number(text: &str) -> Option<String> {
    let rest = text.strip_prefix("Fragment ")?;
    let (number, rest) = rest.split_once(' ')?;
    (!number.is_empty() && number.chars().all(|c| c.is_ascii_digit()) && rest.starts_with('['))
        .then(|| number.to_string())
}

/// True when `text` can open an operator tree: `Name[...]`, or a bare `Name`
/// unless `need_descriptor` is set.
fn operator_head(text: &str, need_descriptor: bool) -> bool {
    if !text.starts_with(|c: char| c.is_ascii_uppercase()) {
        return false;
    }
    let name_len = text
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(text.len());
    match text.get(name_len..) {
        Some("") => !need_descriptor,
        Some(rest) => rest.starts_with('['),
        None => false,
    }
}

fn header_line(plan: &mut ExecutionPlan, text: &str) {
    if text.starts_with("Trino version:")
        || text.starts_with("WARNING: EXPLAIN")
        || text.starts_with("Below is the output for EXPLAIN")
    {
        plan.notes.push(text.to_string());
    } else if text.starts_with("Queued: ") {
        // `Queued: 144.54us, Analysis: 4.16ms, Planning: 29.42ms, Execution: 289.31ms`
        for part in text.split(", ") {
            match part.split_once(": ") {
                Some(("Planning", value)) => plan.planning_time_ms = parse_duration_ms(value),
                Some(("Execution", value)) => plan.execution_time_ms = parse_duration_ms(value),
                _ => {}
            }
        }
        plan.notes.push(text.to_string());
    }
}

/// Build a node from `Name[key = value, ...]`.
fn new_node(id: i32, text: &str) -> PlanNode {
    let (name, descriptor) = match text.split_once('[') {
        Some((name, rest)) => (name.trim(), rest.strip_suffix(']').unwrap_or(rest)),
        None => (text.trim(), ""),
    };
    let mut node = PlanNode {
        id,
        logical_op: name.to_string(),
        physical_op: name.to_string(),
        ..Default::default()
    };
    if let Some(kind) = name.strip_suffix("Join") {
        node.join_type = Some(if kind.is_empty() { "Inner" } else { kind }.to_string());
    }
    for argument in split_top_level(descriptor) {
        let Some((key, value)) = argument.split_once(" = ") else {
            push_property(&mut node, &argument, "");
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "table" => {
                set_table(&mut node, value);
                push_property(&mut node, key, value);
            }
            "filterPredicate" => node.filter = Some(strip_parens(value).to_string()),
            "criteria" => node.join_predicate = Some(strip_parens(value).to_string()),
            "filter" if node.join_type.is_some() => {
                node.filter = Some(strip_parens(value).to_string());
            }
            "keys" => node.group_keys = split_top_level(strip_brackets(value)),
            "orderBy" => node.sort_keys = split_top_level(strip_brackets(value)),
            _ => push_property(&mut node, key, value),
        }
    }
    node
}

/// `tpch:tiny:orders` is catalog, schema, table. Connectors print their own
/// table handles, so anything that is not three or two colon-separated names
/// is kept whole as the relation.
fn set_table(node: &mut PlanNode, value: &str) {
    let parts: Vec<&str> = value.split(':').collect();
    let plain = |part: &&str| !part.is_empty() && !part.contains([' ', '{', '[', '(', '=']);
    match parts.as_slice() {
        [catalog, schema, table] if parts.iter().all(plain) => {
            node.database = Some(catalog.to_string());
            node.schema = Some(schema.to_string());
            node.relation = Some(table.to_string());
        }
        [catalog, table] if parts.iter().all(plain) => {
            node.database = Some(catalog.to_string());
            node.relation = Some(table.to_string());
        }
        _ => node.relation = Some(value.to_string()),
    }
}

fn strip_parens(value: &str) -> &str {
    value
        .strip_prefix('(')
        .and_then(|v| v.strip_suffix(')'))
        .filter(|inner| balanced(inner))
        .unwrap_or(value)
}

fn strip_brackets(value: &str) -> &str {
    value
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(value)
}

/// False when a closing parenthesis comes before its opening one, as in the
/// inside of `(a = b) AND (c = d)`.
fn balanced(text: &str) -> bool {
    let mut depth = 0i32;
    for c in text.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

fn push_property(node: &mut PlanNode, name: &str, value: &str) {
    node.properties.push(PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    });
}

/// File one fact line below an operator.
fn node_line(node: &mut PlanNode, text: &str) {
    if let Some((name, value)) = text.split_once(" := ") {
        // `revenue := sum`: the expression behind an output symbol.
        push_property(node, name.trim(), value.trim());
        return;
    }
    let Some((name, value)) = split_name_value(text) else {
        match text.split_once(" = ") {
            Some((name, value)) => push_property(node, name.trim(), value.trim()),
            None => push_property(node, text, ""),
        }
        return;
    };
    match name {
        "Layout" => {
            node.output_columns = split_top_level(strip_brackets(value));
        }
        "Estimates" => {
            estimates(node, value);
            push_property(node, name, value);
        }
        "CPU" | "Input" => {
            for (key, part) in split_measures(text) {
                measure(node, &key, &part);
                push_property(node, &key, &part);
            }
        }
        _ if name.ends_with("Input avg.") => {
            for (key, part) in split_measures(text) {
                push_property(node, &key, &part);
            }
        }
        _ => push_property(node, name, value),
    }
}

fn split_name_value(text: &str) -> Option<(&str, &str)> {
    if let Some((name, value)) = text.split_once(": ") {
        let name = name.trim();
        // `tpch:orderstatus` and `a := b` are not `name: value`.
        return (!name.is_empty() && !name.contains(['=', '[', '{', '\'']))
            .then_some((name, value.trim()));
    }
    let name = text.strip_suffix(':')?.trim();
    (!name.is_empty() && !name.contains(['=', '[', '{', '\''])).then_some((name, ""))
}

/// Split `CPU: 1.00ms (0.60%), Scheduled: 1.00ms (0.37%), Output: 25 rows
/// (752B)` into its `name: value` parts.
fn split_measures(text: &str) -> Vec<(String, String)> {
    let mut measures: Vec<(String, String)> = Vec::new();
    for part in split_top_level(text) {
        match part.split_once(": ") {
            Some((name, value)) if !name.contains(['(', '{', '[']) || name.ends_with("avg.") => {
                measures.push((name.trim().to_string(), value.trim().to_string()));
            }
            _ => match measures.last_mut() {
                Some((_, value)) => {
                    value.push_str(", ");
                    value.push_str(&part);
                }
                None => measures.push((part, String::new())),
            },
        }
    }
    measures
}

/// Typed fields from one `EXPLAIN ANALYZE` measure.
fn measure(node: &mut PlanNode, name: &str, value: &str) {
    let first = value.split(' ').next().unwrap_or(value);
    match name {
        "CPU" => node.actual_cpu_ms = parse_duration_ms(first),
        "Scheduled" => node.actual_self_time_ms = parse_duration_ms(first),
        "Output" if value.contains(" row") => {
            if let Some(rows) = common::parse_count(first) {
                node.actual_rows = Some(rows);
                node.actual_rows_total = Some(rows);
            }
        }
        "Input" if value.contains(" row") => node.rows_read = common::parse_count(first),
        _ => {}
    }
}

/// `{rows: 1500 (26.37kB), cpu: 33.74k, memory: 0B, network: 0B}/{rows: 300
/// (5.27kB), ...}`. A combined operator such as `ScanFilterProject` prints
/// one group per step, scan first: the last group is what the operator
/// returns and the first what it reads. Unknown values print as `?`.
fn estimates(node: &mut PlanNode, value: &str) {
    let groups: Vec<&str> = value.split("}/{").collect();
    let field = |group: &str, key: &str| -> Option<f64> {
        let rest = group.split_once(key)?.1;
        let token = rest.split([' ', ',', '}']).next().unwrap_or_default();
        common::parse_count(token)
    };
    if let Some(last) = groups.last() {
        if let Some(rows) = field(last, "rows: ") {
            node.estimated_rows = rows;
        }
        if let Some(cpu) = field(last, "cpu: ") {
            node.estimated_cpu = cpu;
        }
    }
    if groups.len() > 1
        && let Some(first) = groups.first()
    {
        node.estimated_rows_read = field(first, "rows: ");
    }
}

/// A line indented past the facts: the value of the fact above it
/// (`tpch:orderstatus` then `:: [[F], [O], [P]]`), a `Dynamic filters:`
/// entry, or one `'name' = value` entry of a VERBOSE `metrics:` block.
fn continuation(node: &mut PlanNode, text: &str) {
    let parent = node
        .properties
        .iter()
        .rev()
        .find(|property| !property.name.starts_with('\''))
        .map(|property| property.name.clone())
        .unwrap_or_default();
    if let Some(rest) = text.strip_prefix('\'')
        && let Some((metric, value)) = rest.split_once("' = ")
        && parent.ends_with("metrics")
    {
        let side = parent.strip_suffix("metrics").unwrap_or_default().trim();
        let name = if side.is_empty() {
            format!("'{metric}'")
        } else {
            format!("'{side} {metric}'")
        };
        push_property(node, &name, value.trim());
        return;
    }
    if let Some(last) = node.properties.last_mut() {
        if !last.value.is_empty() {
            last.value.push('\n');
        }
        last.value.push_str(text);
    }
}

/// Hang each fragment's root under the `RemoteSource` / `RemoteMerge` that
/// reads it. A link is only made forward in the document and only to a
/// fragment that has no reader yet, so the result is always a tree.
fn link_fragments(nodes: &[PlanNode], fragments: &[Fragment], edges: &mut Vec<PlanEdge>) {
    let mut linked: Vec<i32> = Vec::new();
    for node in nodes {
        let Some(sources) = node.property("sourceFragmentIds") else {
            continue;
        };
        for number in split_top_level(strip_brackets(sources)) {
            let root = fragments
                .iter()
                .find(|fragment| fragment.number == number)
                .and_then(|fragment| fragment.root);
            if let Some(root) = root
                && root > node.id
                && !linked.contains(&root)
            {
                linked.push(root);
                edges.push(PlanEdge {
                    from_node: node.id,
                    to_node: root,
                });
            }
        }
    }
}

/// Derive the search strings and warnings once all of a node's lines are in.
fn finish_node(node: &mut PlanNode) {
    if let Some(relation) = node.relation.clone() {
        node.output_list.push(relation.clone());
        // Only a bare `TableScan` is reported: `ScanFilter` pushes its
        // predicate to the connector, which may or may not read everything.
        if node.physical_op == "TableScan" {
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
    if node.filter.is_some()
        && let (Some(read), Some(returned)) = (node.rows_read, node.actual_rows)
        && read >= returned
    {
        node.rows_removed_by_filter = Some(read - returned);
    }
}

/// `actual_time_ms` for every node: its own scheduled time plus the
/// inclusive time of its inputs. Every edge points forward in the document,
/// so one reverse pass sees each input before the node that reads it.
fn fill_inclusive_times(nodes: &mut [PlanNode], edges: &[PlanEdge]) {
    let mut inclusive: Vec<Option<f64>> = vec![None; nodes.len()];
    let mut inputs: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for edge in edges {
        let (from, to) = (edge.from_node as usize, edge.to_node as usize);
        if from >= 1 && to > from && to <= nodes.len() {
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

/// Read a Trino duration (`144.54us`, `4.16ms`, `1.18s`, `2.50m`, `0.00ns`)
/// into milliseconds.
fn parse_duration_ms(text: &str) -> Option<f64> {
    let text = text.trim();
    let digits = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let value: f64 = text.get(..digits)?.parse().ok()?;
    let scale = match text.get(digits..)?.trim() {
        "ns" => 1e-6,
        "us" | "µs" | "μs" => 1e-3,
        "ms" => 1.0,
        "s" => 1e3,
        "m" => 6e4,
        "h" => 3.6e6,
        "d" => 8.64e7,
        _ => return None,
    };
    let ms = value * scale;
    ms.is_finite().then_some(ms)
}

/// Split on commas that are not inside parentheses, brackets, braces or
/// quotes.
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

    const Q1_TSV: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q1-explain-tsv.txt");
    const Q1_LOGICAL: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q1-logical-aligned.txt");
    const Q2_ALIGNED: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q2-explain-aligned.txt");
    const Q2_CSV: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q2-explain-csv.txt");
    const Q2_VERTICAL: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q2-explain-vertical.txt");
    const Q2_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q2-analyze-aligned.txt");
    const Q3_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q3-analyze-aligned.txt");
    const Q4_VERBOSE: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q4-analyze-verbose-aligned.txt");
    const Q5_ALIGNED: &str =
        include_str!("../../../tests/golden/plans/trino/trino484-q5-explain-aligned.txt");
    const T435_LOGICAL: &str =
        include_str!("../../../tests/golden/plans/trino/trino435-q2-logical-aligned.txt");
    const T435_LOGICAL_CSV: &str =
        include_str!("../../../tests/golden/plans/trino/trino435-q1-logical-csv.txt");
    const T435_ANALYZE: &str =
        include_str!("../../../tests/golden/plans/trino/trino435-q2-analyze-aligned.txt");

    /// First lines of the other `EXPLAIN` outputs of Trino 484.
    const IO_JSON: &str = "              Query Plan\n---------------------------------------\n {\n   \"inputTableColumnInfos\" : [ {\n     \"table\" : {\n       \"catalog\" : \"tpch\",\n";
    const PLAN_JSON: &str = "                Query Plan\n-----------------------\n {\n   \"0\" : {\n     \"id\" : \"25\",\n     \"name\" : \"Output\",\n";
    const VALIDATE: &str = " Valid\n-------\n true\n(1 row)\n";
    const GRAPHVIZ: &str =
        " digraph distributed_plan {\n subgraph cluster_0 {\n label = \"SINGLE\"\n";

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
    fn distributed_plan_joins_fragments_into_one_tree() {
        let plan = parse_trino_explain_text(Q5_ALIGNED).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("trino-text"));
        assert!(plan.dialect.is_none());
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(plan.notes, vec!["Trino version: 484"]);
        assert_eq!(
            ops(&plan),
            vec![
                "Output",
                "Window",
                "LocalExchange",
                "RemoteSource",
                "InnerJoin",
                "ScanFilter",
                "LocalExchange",
                "RemoteSource",
                "Filter",
                "Aggregate",
                "LocalExchange",
                "RemoteSource",
                "Aggregate",
                "TableScan",
            ]
        );
        // Every node but the root has exactly one parent.
        assert_eq!(plan.edges.len(), plan.nodes.len() - 1);
        assert_eq!(children(&plan, 4), vec![5]);
        assert_eq!(children(&plan, 5), vec![6, 7]);
        assert_eq!(children(&plan, 8), vec![9]);
        assert_eq!(children(&plan, 12), vec![13]);
        assert_eq!(children(&plan, 13), vec![14]);

        let output = &plan.nodes[0];
        assert_eq!(output.property("Fragment"), Some("0 [SINGLE]"));
        assert_eq!(
            output.property("Fragment Output layout"),
            Some("[name, sum, row_number_3]")
        );
        assert_eq!(output.property("columnNames"), Some("[name, t, rn]"));
        assert_eq!(output.property("t"), Some("sum"));
        assert_eq!(
            output.output_columns,
            vec!["name:varchar(25)", "sum:double", "row_number_3:bigint"]
        );
        // `rows: ? (?)` is an unknown estimate.
        assert_eq!(output.estimated_rows, 0.0);

        let window = &plan.nodes[1];
        assert_eq!(window.sort_keys, vec!["sum DESC NULLS LAST"]);
        assert_eq!(
            window.property("row_number_3"),
            Some("row_number() RANGE UNBOUNDED_PRECEDING CURRENT_ROW")
        );

        let join = &plan.nodes[4];
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.join_predicate.as_deref(), Some("custkey_1 = custkey"));
        assert_eq!(join.estimated_rows, 900.0);
        assert_eq!(join.estimated_cpu, 90_820.0);
        assert_eq!(join.property("distribution"), Some("REPLICATED"));
        assert_eq!(join.property("Fragment"), Some("1 [SOURCE]"));
        assert_eq!(
            join.property("dynamicFilterAssignments"),
            Some("{custkey -> #df_383}")
        );

        let customer = &plan.nodes[5];
        assert_eq!(customer.database.as_deref(), Some("tpch"));
        assert_eq!(customer.schema.as_deref(), Some("tiny"));
        assert_eq!(customer.relation.as_deref(), Some("customer"));
        assert_eq!(customer.estimated_rows, 1500.0);
        assert_eq!(customer.estimated_rows_read, Some(1500.0));
        assert!(customer.warnings.is_empty());

        let filter = &plan.nodes[8];
        assert_eq!(filter.filter.as_deref(), Some("double '500000.0' < sum"));
        assert_eq!(
            filter.property("Fragment Output partitioning"),
            Some("BROADCAST []")
        );

        let aggregate = &plan.nodes[9];
        assert_eq!(aggregate.group_keys, vec!["custkey"]);
        assert_eq!(aggregate.property("type"), Some("FINAL"));
        assert_eq!(aggregate.estimated_rows, 1000.0);

        let scan = &plan.nodes[13];
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.estimated_rows, 15_000.0);
        assert_eq!(
            scan.property("tpch:orderstatus"),
            Some(":: [[F], [O], [P]]")
        );
        assert!(matches!(
            scan.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "orders" && schema == "tiny"
        ));
    }

    #[test]
    fn every_client_format_gives_the_same_plan() {
        let reference = parse_trino_explain_text(Q2_ALIGNED).unwrap();
        assert_eq!(reference.nodes.len(), 19);
        for doc in [Q2_CSV, Q2_VERTICAL] {
            let plan = parse_trino_explain_text(doc).unwrap();
            assert_eq!(ops(&plan), ops(&reference));
            assert_eq!(plan.notes, reference.notes);
            for (node, expected) in plan.nodes.iter().zip(&reference.nodes) {
                assert_eq!(children(&plan, node.id), children(&reference, expected.id));
                assert_eq!(node.estimated_rows, expected.estimated_rows);
                assert_eq!(node.output_columns, expected.output_columns);
                assert_eq!(node.properties.len(), expected.properties.len());
            }
        }
        let tsv = parse_trino_explain_text(Q1_TSV).unwrap();
        assert_eq!(ops(&tsv), vec!["Output", "ScanFilter"]);
        assert_eq!(tsv.nodes[1].relation.as_deref(), Some("orders"));
    }

    #[test]
    fn analyze_reads_actuals() {
        let plan = parse_trino_explain_text(Q2_ANALYZE).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        assert_eq!(plan.planning_time_ms, Some(29.42));
        assert_eq!(plan.execution_time_ms, Some(289.31));
        assert_eq!(plan.notes.len(), 2);
        assert!(plan.notes[1].starts_with("Queued: 144.54us"));

        // ANALYZE has no Output operator: the first fragment starts at the
        // merge that reads fragment 2.
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "RemoteMerge");
        assert_eq!(root.property("Fragment"), Some("1 [SINGLE]"));
        assert!(
            root.property("Fragment CPU")
                .unwrap()
                .starts_with("497.71us, Scheduled: 553.50us")
        );
        assert_eq!(
            root.property("Fragment Peak Memory"),
            Some("968B, Tasks count: 1; per task: max: 968B")
        );
        assert_eq!(root.actual_rows, Some(25.0));
        assert_eq!(root.actual_self_time_ms, Some(0.0));
        assert_eq!(root.property("Blocked"), Some("120.00ms (3.11%)"));
        assert_eq!(root.property("Input avg."), Some("25.00 rows"));
        assert_eq!(root.property("Input std.dev."), Some("0.00%"));
        assert_eq!(children(&plan, 1), vec![2]);

        let merge = &plan.nodes[1];
        assert_eq!(merge.physical_op, "LocalMerge");
        assert_eq!(merge.estimated_rows, 25.0);
        assert_eq!(merge.actual_rows_total, Some(25.0));
        assert_eq!(merge.actual_cpu_ms, Some(1.0));
        assert_eq!(merge.actual_self_time_ms, Some(1.0));
        assert_eq!(merge.property("Scheduled"), Some("1.00ms (0.37%)"));
        assert_eq!(merge.property("Output"), Some("25 rows (752B)"));

        let sort = &plan.nodes[2];
        assert_eq!(sort.physical_op, "PartialSort");
        assert_eq!(sort.actual_self_time_ms, Some(2.0));

        let aggregate = &plan.nodes[4];
        assert_eq!(aggregate.property("type"), Some("FINAL"));
        assert_eq!(aggregate.group_keys, vec!["name_5"]);
        assert_eq!(aggregate.actual_self_time_ms, Some(5.0));
        assert_eq!(aggregate.property("count"), Some("count(count_7)"));

        let join = plan
            .nodes
            .iter()
            .find(|node| node.physical_op == "InnerJoin")
            .unwrap();
        assert_eq!(join.actual_rows, Some(939.0));
        assert_eq!(join.estimated_rows, 4500.0);
        assert_eq!(
            join.property("Left (probe) Input avg."),
            Some("956.25 rows")
        );
        assert_eq!(
            join.property("Right (build) Input avg."),
            Some("42.13 rows")
        );

        let customer = plan
            .nodes
            .iter()
            .find(|node| node.relation.as_deref() == Some("customer"))
            .unwrap();
        assert_eq!(customer.physical_op, "ScanFilterProject");
        assert_eq!(
            customer.filter.as_deref(),
            Some("mktsegment = varchar(10) 'BUILDING'")
        );
        assert_eq!(customer.rows_read, Some(1500.0));
        assert_eq!(customer.property("Filtered"), Some("77.53%"));
        assert_eq!(customer.property("Splits"), Some("4"));
        assert_eq!(
            customer.property("Dynamic filters"),
            Some(
                "- df_590, [ SortedRangeSet[type=bigint, ranges=25, {[0], ..., [24]}] ], collection time=95.88ms"
            )
        );
        let returned = customer.actual_rows.unwrap();
        assert_eq!(customer.rows_removed_by_filter, Some(1500.0 - returned));
        assert_eq!(customer.estimated_rows_read, Some(1500.0));
        assert_eq!(customer.estimated_rows, 300.0);

        // Inclusive time is the node's own time plus its inputs'.
        let below: f64 = children(&plan, 3)
            .iter()
            .filter_map(|id| plan.nodes[*id as usize - 1].actual_time_ms)
            .sum();
        assert!((sort.actual_time_ms.unwrap() - (2.0 + below)).abs() < 1e-9);
        assert!(root.actual_time_ms.unwrap() >= merge.actual_time_ms.unwrap());
    }

    #[test]
    fn analyze_with_no_rows_and_unknown_filter_share() {
        let plan = parse_trino_explain_text(Q3_ANALYZE).unwrap();
        assert_eq!(plan.nodes[0].physical_op, "TopN");
        assert_eq!(plan.nodes[0].property("count"), Some("10"));
        assert_eq!(plan.nodes[0].sort_keys, vec!["totalprice DESC NULLS LAST"]);
        let lineitem = plan
            .nodes
            .iter()
            .find(|node| node.relation.as_deref() == Some("lineitem"))
            .unwrap();
        assert_eq!(lineitem.rows_read, Some(0.0));
        assert_eq!(lineitem.property("Filtered"), Some("?%"));
        let orders = plan
            .nodes
            .iter()
            .find(|node| node.relation.as_deref() == Some("orders"))
            .unwrap();
        assert_eq!(orders.filter.as_deref(), Some("custkey = bigint '42'"));
        assert_eq!(orders.rows_read, Some(15_000.0));
        assert_eq!(orders.property("Filtered"), Some("100.00%"));
        let join = plan
            .nodes
            .iter()
            .find(|node| node.physical_op == "InnerJoin")
            .unwrap();
        assert_eq!(join.property("distribution"), Some("PARTITIONED"));
    }

    #[test]
    fn verbose_metrics_become_properties() {
        let plan = parse_trino_explain_text(Q4_VERBOSE).unwrap();
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "RemoteSource");
        assert_eq!(root.property("sourceFragmentIds"), Some("[2, 5]"));
        assert_eq!(children(&plan, 1).len(), 2);
        assert_eq!(root.actual_rows, Some(26.0));
        assert_eq!(root.property("Input avg."), Some("3.25 rows"));
        assert!(
            root.property("'Input rows distribution'")
                .unwrap()
                .starts_with("{count=8, p01=0.00")
        );
        assert!(
            root.property("Fragment Task output distribution")
                .unwrap()
                .starts_with("{count=1.00, p01=598B")
        );
        let join = &plan.nodes[1];
        assert_eq!(join.physical_op, "InnerJoin");
        assert_eq!(join.join_predicate.as_deref(), Some("custkey = custkey_2"));
        assert_eq!(join.estimated_rows, 1000.0);
        assert_eq!(join.actual_rows, Some(15.0));
        assert!(
            join.property("Reorder joins cost")
                .unwrap()
                .starts_with("{rows: 1000 (22.46kB)")
        );
        assert!(
            join.property("'Left (probe) Input rows distribution'")
                .unwrap()
                .starts_with("{count=4, p01=375.00")
        );
        assert!(
            join.property("'Right (build) Input rows distribution'")
                .unwrap()
                .starts_with("{count=1, p01=15.00")
        );
        let customer = &plan.nodes[2];
        assert_eq!(customer.relation.as_deref(), Some("customer"));
        assert_eq!(customer.actual_rows, Some(1500.0));
        assert_eq!(customer.actual_cpu_ms, Some(13.0));
        assert_eq!(customer.actual_self_time_ms, Some(20.0));
        assert_eq!(
            customer.property("'Dynamic Filter CPU time'"),
            Some("{duration=0.00ns}")
        );
    }

    #[test]
    fn deprecated_logical_type_prints_the_distributed_plan() {
        let plan = parse_trino_explain_text(Q1_LOGICAL).unwrap();
        assert_eq!(ops(&plan), vec!["Output", "ScanFilter"]);
        // The first warning is the CLI's own, the next two lines are in the
        // plan cell.
        assert_eq!(plan.notes.len(), 4);
        assert!(plan.notes[1].starts_with("WARNING: EXPLAIN TYPE LOGICAL is no longer supported"));
        assert_eq!(plan.notes[3], "Trino version: 484");
        assert_eq!(
            plan.nodes[0].property("Fragment"),
            Some("0 [tpch:orders:15000]")
        );
    }

    #[test]
    fn trino_435_logical_plan_has_no_fragments() {
        let plan = parse_trino_explain_text(T435_LOGICAL).unwrap();
        assert_eq!(plan.notes, vec!["Trino version: 435"]);
        assert_eq!(plan.nodes.len(), 18);
        assert_eq!(plan.edges.len(), 17);
        assert_eq!(plan.nodes[0].physical_op, "Output");
        assert!(plan.nodes[0].property("Fragment").is_none());
        assert_eq!(plan.estimated_rows, 25.0);
        assert_eq!(plan.nodes[1].physical_op, "RemoteMerge");
        assert_eq!(plan.nodes[4].physical_op, "RemoteExchange");
        assert_eq!(plan.nodes[4].property("type"), Some("REPARTITION"));
        let join = &plan.nodes[9];
        assert_eq!(join.physical_op, "InnerJoin");
        assert_eq!(
            join.join_predicate.as_deref(),
            Some("\"custkey_1\" = \"custkey\"")
        );
        assert_eq!(children(&plan, 10), vec![11, 12]);
        let orders = &plan.nodes[10];
        assert_eq!(orders.relation.as_deref(), Some("orders"));
        assert_eq!(orders.estimated_rows, 15_000.0);
        let customer = &plan.nodes[14];
        assert_eq!(customer.physical_op, "ScanFilterProject");
        assert_eq!(
            customer.filter.as_deref(),
            Some("\"mktsegment\" = CAST('BUILDING' AS varchar(10))")
        );
        assert_eq!(customer.estimated_rows_read, Some(1500.0));
        assert_eq!(customer.estimated_rows, 300.0);
        assert_eq!(plan.nodes[17].physical_op, "TableScan");
        assert_eq!(plan.nodes[17].relation.as_deref(), Some("nation"));

        let csv = parse_trino_explain_text(T435_LOGICAL_CSV).unwrap();
        assert_eq!(ops(&csv), vec!["Output", "ScanFilter"]);
        assert_eq!(
            csv.nodes[1].filter.as_deref(),
            Some("\"regionkey\" = BIGINT '1'")
        );
        assert_eq!(csv.nodes[1].estimated_rows, 5.0);
        assert_eq!(csv.nodes[1].estimated_rows_read, Some(25.0));
    }

    #[test]
    fn trino_435_analyze() {
        let plan = parse_trino_explain_text(T435_ANALYZE).unwrap();
        assert_eq!(plan.planning_time_ms, Some(65.5));
        assert_eq!(plan.execution_time_ms, Some(1180.0));
        assert_eq!(plan.nodes[0].physical_op, "LocalMerge");
        assert_eq!(plan.nodes[0].property("Fragment"), Some("1 [ROUND_ROBIN]"));
        assert_eq!(plan.nodes[0].actual_rows, Some(25.0));
        assert_eq!(plan.nodes[1].actual_self_time_ms, Some(8.0));
        assert_eq!(plan.nodes[3].physical_op, "Aggregate");
        assert_eq!(plan.nodes[3].actual_self_time_ms, Some(12.0));
        assert_eq!(children(&plan, 3), vec![4]);
    }

    #[test]
    fn other_explain_outputs_are_refused_with_a_reason() {
        for (doc, needle) in [
            (IO_JSON, "TYPE IO"),
            (PLAN_JSON, "FORMAT JSON"),
            (VALIDATE, "TYPE VALIDATE"),
            (GRAPHVIZ, "GRAPHVIZ"),
        ] {
            let error = parse_trino_explain_text(doc).unwrap_err();
            assert!(error.to_string().contains(needle), "{error}");
            assert!(!looks_like_trino_explain_text(doc));
        }
        assert!(parse_trino_explain_text("").is_err());
        assert!(parse_trino_explain_text("Trino version: 484\n").is_err());
    }

    #[test]
    fn detector_accepts_own_formats_only() {
        for doc in [
            Q1_TSV,
            Q1_LOGICAL,
            Q2_ALIGNED,
            Q2_CSV,
            Q2_VERTICAL,
            Q2_ANALYZE,
            Q3_ANALYZE,
            Q4_VERBOSE,
            Q5_ALIGNED,
            T435_LOGICAL,
            T435_LOGICAL_CSV,
            T435_ANALYZE,
        ] {
            assert!(looks_like_trino_explain_text(doc));
        }
        for doc in [
            "Seq Scan on t  (cost=0.00..1.50 rows=50 width=4)\n  ->  Index Scan using i on u  (cost=0.1..2.0 rows=1 width=4)",
            "-> Limit: 10 row(s)  (cost=1.2 rows=10)\n    -> Table scan on t  (cost=1.2 rows=100)",
            "distribution: local\nvectorized: true\n\n• scan\n  estimated row count: 1\n  table: orders@orders_pkey\n  spans: [/77 - /77]",
            "== Physical Plan ==\n*(1) Project [id#1L]\n+- *(1) Range (0, 10, step=1, splits=7)",
            "Fragment 1 of the report\nLayout: [see below]x",
            "",
        ] {
            assert!(!looks_like_trino_explain_text(doc));
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_ms("144.54us"), Some(0.14454));
        assert_eq!(parse_duration_ms("4.16ms"), Some(4.16));
        assert_eq!(parse_duration_ms("1.18s"), Some(1180.0));
        assert_eq!(parse_duration_ms("0.00ns"), Some(0.0));
        assert_eq!(parse_duration_ms("2.00m"), Some(120_000.0));
        assert_eq!(parse_duration_ms("?"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn never_panics_on_mangled_input() {
        for sample in [
            Q1_TSV,
            Q1_LOGICAL,
            Q2_CSV,
            Q2_VERTICAL,
            Q2_ANALYZE,
            Q4_VERBOSE,
            Q5_ALIGNED,
            T435_LOGICAL,
            IO_JSON,
            VALIDATE,
        ] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_trino_explain_text(doc);
                let _ = looks_like_trino_explain_text(doc);
            });
        }
    }

    #[test]
    fn deep_nesting_is_refused() {
        let mut doc = String::from("Trino version: 484\nOutput[columnNames = [a]]\n");
        for depth in 0..600 {
            doc.push_str(&" ".repeat(depth * 3));
            doc.push_str("└─ Project[]\n");
        }
        assert!(parse_trino_explain_text(&doc).is_err());
    }
}
