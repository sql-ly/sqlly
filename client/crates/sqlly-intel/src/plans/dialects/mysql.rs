//! MySQL and MariaDB `EXPLAIN FORMAT=JSON` (version 1: the `query_block`
//! document), MariaDB `ANALYZE FORMAT=JSON`, and MySQL `EXPLAIN FORMAT=TREE`
//! / `EXPLAIN ANALYZE` text.

use super::common;
use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanBuffers, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;
use serde_json::{Map, Value};

/// Deepest nesting either parser follows.
const MAX_DEPTH: usize = 512;
/// Nodes read from one document.
const MAX_NODES: usize = 50_000;

/// What MySQL prints as the whole plan for a statement the iterator executor
/// does not run (single-table `UPDATE` / `DELETE`, for example).
const NOT_EXECUTABLE: &str = "<not executable by iterator executor>";

fn property(name: &str, value: &str) -> PlanProperty {
    PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    }
}

// ---------------------------------------------------------------------------
// MySQL / MariaDB: EXPLAIN FORMAT=JSON (version 1)
// ---------------------------------------------------------------------------

/// Parse MySQL / MariaDB `EXPLAIN FORMAT=JSON` output (the `query_block`
/// document; on MySQL 8.3 and later that is `explain_json_format_version =
/// 1`, the default) and MariaDB `ANALYZE FORMAT=JSON`, which is the same
/// shape with `r_*` runtime fields.
///
/// The root is a `Query Block` node. Each `table` object becomes a node
/// (`Table: name` / `Access: type`); `nested_loop` lists add no node, so the
/// tables of a join are siblings in join order. The blocks that describe
/// work around the tables (`ordering_operation`, `grouping_operation`,
/// `windowing`, `union_result`, `materialized_from_subquery`, MariaDB
/// `filesort`, `temporary_table`, `block-nl-join`, `subquery_cache`, nested
/// `query_block`s, ...) become nodes above the tables they contain.
///
/// Row numbers on a table are the rows read per scan, before the table's
/// condition: `estimated_rows` is `rows_examined_per_scan` (MariaDB `rows`)
/// and `filtered_percent` says what share is expected to pass. MariaDB
/// `r_rows` is the average rows read per scan and `r_loops` the number of
/// scans; this parser puts the product in `actual_rows` (and in
/// `actual_rows_total` and `rows_read`), with the scan count in `loops`.
///
/// A version 2 document (`operation` / `query_plan` at the top) is handed to
/// [`super::mysql_v2::parse_mysql_explain_json_v2`].
pub fn parse_mysql_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("EXPLAIN FORMAT=JSON: {e}")))?;
    let block = match value.get("query_block") {
        Some(Value::Object(block)) => block,
        _ => {
            if value.get("operation").is_some() || value.get("query_plan").is_some() {
                return super::mysql_v2::parse_mysql_explain_json_v2(doc);
            }
            return Err(PlanError::InvalidStructure(
                "EXPLAIN FORMAT=JSON: missing query_block".into(),
            ));
        }
    };

    let mut walker = JsonWalker::default();
    let root_id = walker.block_node("query_block", "Query Block", block, 0);
    walker.children(block, root_id, 1)?;
    let JsonWalker { nodes, edges } = walker;

    let mut marks = KeyMarks::default();
    marks.scan(&value, 0);
    let dialect = if marks.runtime || (marks.mariadb && !marks.mysql) {
        PlanDialect::MariaDb
    } else {
        PlanDialect::MySql
    };

    let estimated_rows = nodes
        .iter()
        .skip(1)
        .map(|n| n.estimated_rows)
        .fold(0.0, f64::max);
    let mut plan = ExecutionPlan {
        statement_type: if marks.runtime { "ANALYZE" } else { "EXPLAIN" }.to_string(),
        estimated_rows,
        dialect: Some(dialect),
        source_format: Some(
            match dialect {
                PlanDialect::MariaDb => "mariadb-json",
                _ => "mysql-json",
            }
            .to_string(),
        ),
        // MariaDB ANALYZE: time spent optimizing, and the root block's wall
        // time.
        planning_time_ms: value
            .get("query_optimization")
            .and_then(|o| o.get("r_total_time_ms"))
            .and_then(finite),
        execution_time_ms: nodes.first().and_then(|root| root.actual_time_ms),
        has_actuals: marks.runtime,
        nodes,
        edges,
        ..Default::default()
    };
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// MySQL cost fields are strings ("0.35"); MariaDB sometimes emits numbers.
fn number_or_string(value: &Value) -> f64 {
    finite(value).unwrap_or(0.0)
}

/// A finite number from a JSON number or a numeric string.
fn finite(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|n: &f64| n.is_finite())
}

fn number(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(finite)
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn is_true(map: &Map<String, Value>, key: &str) -> bool {
    matches!(map.get(key), Some(Value::Bool(true)))
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn string_list(map: &Map<String, Value>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(scalar_text).collect(),
        _ => Vec::new(),
    }
}

/// Which engine's keys a document uses.
#[derive(Default)]
struct KeyMarks {
    /// Any `r_*` key: MariaDB ANALYZE.
    runtime: bool,
    /// `cost_info`, `rows_examined_per_scan`, `rows_produced_per_join`.
    mysql: bool,
    /// `query_optimization`, or a numeric `rows` / `cost`.
    mariadb: bool,
}

impl KeyMarks {
    fn scan(&mut self, value: &Value, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    match key.as_str() {
                        "cost_info" | "rows_examined_per_scan" | "rows_produced_per_join" => {
                            self.mysql = true
                        }
                        "query_optimization" => self.mariadb = true,
                        "rows" | "cost" if child.is_number() => self.mariadb = true,
                        _ if key.starts_with("r_") => self.runtime = true,
                        _ => {}
                    }
                    self.scan(child, depth + 1);
                }
            }
            Value::Array(items) => {
                for item in items {
                    self.scan(item, depth + 1);
                }
            }
            _ => {}
        }
    }
}

/// Display name of the JSON blocks that become plan nodes.
fn block_name(key: &str) -> Option<&'static str> {
    Some(match key {
        "query_block" => "Query Block",
        "ordering_operation" => "Ordering operation",
        "grouping_operation" => "Grouping operation",
        "duplicates_removal" | "duplicate_removal" => "Duplicates removal",
        "windowing" => "Windowing",
        "buffer_result" => "Buffer result",
        "union_result" => "Union result",
        "intersect_result" => "Intersect result",
        "except_result" => "Except result",
        "recursive_union" => "Recursive union",
        "materialized_from_subquery" | "materialized" | "materialization" => {
            "Materialized subquery"
        }
        "filesort" => "Filesort",
        "temporary_table" => "Temporary table",
        "read_sorted_file" => "Read sorted file",
        "block-nl-join" => "Block nested loop join",
        "subquery_cache" => "Subquery cache",
        "expression_cache" => "Expression cache",
        "window_functions_computation" => "Window functions computation",
        _ => return None,
    })
}

/// A materialization printed next to the `table` that reads it (MariaDB)
/// belongs under that table.
fn is_materialization(key: &str) -> bool {
    matches!(
        key,
        "materialized_from_subquery" | "materialized" | "materialization"
    )
}

/// True when `value` holds a table or one of the node blocks anywhere inside
/// it. Everything else (`cost_info`, `r_engine_stats`, `rowid_filter`, the
/// `windows` list) is detail of the node it sits on.
fn is_structural(value: &Value, depth: usize) -> bool {
    if depth > MAX_DEPTH {
        return false;
    }
    match value {
        Value::Object(map) => map.iter().any(|(key, child)| {
            (child.is_object() && (key == "table" || block_name(key).is_some()))
                || is_structural(child, depth + 1)
        }),
        Value::Array(items) => items.iter().any(|item| is_structural(item, depth + 1)),
        _ => false,
    }
}

/// Add `key: value` to `out`, flattening nested detail objects into
/// `parent.child` names.
fn push_detail(out: &mut Vec<PlanProperty>, name: &str, value: &Value, depth: usize) {
    match value {
        Value::Null => {}
        Value::Object(map) if depth < 4 => {
            for (key, child) in map {
                push_detail(out, &format!("{name}.{key}"), child, depth + 1);
            }
        }
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => {
            let joined = items
                .iter()
                .filter_map(scalar_text)
                .collect::<Vec<_>>()
                .join(", ");
            out.push(property(name, &joined));
        }
        Value::Array(_) | Value::Object(_) => out.push(property(name, &value.to_string())),
        scalar => {
            if let Some(text) = scalar_text(scalar) {
                out.push(property(name, &text));
            }
        }
    }
}

/// Every non-structural entry of `map` except the `consumed` keys.
fn detail_properties(map: &Map<String, Value>, consumed: &[&str]) -> Vec<PlanProperty> {
    let mut out = Vec::new();
    for (key, value) in map {
        if consumed.contains(&key.as_str())
            || (key == "table" && value.is_object())
            || (block_name(key).is_some() && value.is_object())
            || is_structural(value, 0)
        {
            continue;
        }
        push_detail(&mut out, key, value, 0);
    }
    out
}

/// Split on commas that are not inside parentheses or quotes: the items of a
/// sort key such as `` sum(a * b) desc, c.`name` ``.
fn split_top_level(text: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in text.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' => depth += 1,
                ')' => depth = (depth - 1).max(0),
                ',' if depth == 0 => {
                    items.push(std::mem::take(&mut current));
                    continue;
                }
                _ => {}
            },
        }
        current.push(c);
    }
    items.push(current);
    items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// MariaDB `r_buffer_size`: `"544"` is bytes, `"3KiB"` carries its unit.
fn buffer_size_kb(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().map(|bytes| bytes / 1024.0),
        Value::String(s) => {
            let s = s.trim();
            if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
                s.parse::<f64>().ok().map(|bytes| bytes / 1024.0)
            } else {
                common::parse_size_kb(s)
            }
        }
        _ => None,
    }
    .filter(|kb| kb.is_finite())
}

#[derive(Default)]
struct JsonWalker {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

impl JsonWalker {
    fn push(&mut self, node: PlanNode, parent: i32) -> i32 {
        let id = node.id;
        self.nodes.push(node);
        if parent > 0 {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        id
    }

    /// Walk the entries of `map` that hold tables or node blocks, adding
    /// them under `parent`. Subquery lists go last so a block's main input
    /// comes first.
    fn children(&mut self, map: &Map<String, Value>, parent: i32, depth: usize) -> PlanResult<()> {
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN FORMAT=JSON: the plan is nested deeper than 512 levels".into(),
            ));
        }
        if self.nodes.len() > MAX_NODES {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN FORMAT=JSON: the plan has more than 50000 nodes".into(),
            ));
        }
        let mut table_id = None;
        if let Some(Value::Object(table)) = map.get("table") {
            let id = self.table_node(table, parent);
            self.children(table, id, depth + 1)?;
            table_id = Some(id);
        }
        let is_subquery_list = |key: &str| key.contains("subquer") && !is_materialization(key);
        let ordered = map
            .iter()
            .filter(|(key, _)| !is_subquery_list(key))
            .chain(map.iter().filter(|(key, _)| is_subquery_list(key)));
        for (key, value) in ordered {
            if key == "table" && value.is_object() {
                continue;
            }
            match (block_name(key), value) {
                (Some(name), Value::Object(block)) => {
                    let under = match table_id {
                        Some(table) if is_materialization(key) => table,
                        _ => parent,
                    };
                    let id = self.block_node(key, name, block, under);
                    self.children(block, id, depth + 1)?;
                }
                _ if is_structural(value, 0) => self.transparent(key, value, parent, depth + 1)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// A list or wrapper object that adds no node of its own (`nested_loop`,
    /// the subquery lists and their `{dependent, cacheable, query_block}`
    /// items). What such an item says about itself is kept on the first node
    /// it produces.
    fn transparent(
        &mut self,
        key: &str,
        value: &Value,
        parent: i32,
        depth: usize,
    ) -> PlanResult<()> {
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN FORMAT=JSON: the plan is nested deeper than 512 levels".into(),
            ));
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.transparent(key, item, parent, depth + 1)?;
                }
            }
            Value::Object(map) => {
                let before = self.nodes.len();
                self.children(map, parent, depth + 1)?;
                if let Some(first) = self.nodes.get_mut(before) {
                    if key.contains("subquer") {
                        first.properties.push(property("subquery_list", key));
                    }
                    first.properties.extend(detail_properties(map, &[]));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// A node for one of the [`block_name`] objects.
    fn block_node(
        &mut self,
        key: &str,
        name: &str,
        block: &Map<String, Value>,
        parent: i32,
    ) -> i32 {
        let id = self.nodes.len() as i32 + 1;
        let mut node = PlanNode {
            id,
            logical_op: name.to_string(),
            physical_op: name.to_string(),
            ..Default::default()
        };
        let is_query_block = key == "query_block";
        if is_query_block {
            node.subplan_name = block
                .get("select_id")
                .and_then(scalar_text)
                .map(|select_id| format!("Select #{select_id}"));
            node.estimated_cost = block
                .get("cost_info")
                .and_then(|c| c.get("query_cost"))
                .or_else(|| block.get("cost"))
                .map(number_or_string)
                .unwrap_or(0.0);
        }
        if let Some(table) = text(block, "table_name") {
            node.output_list.push(table.to_string());
        }
        // MariaDB ANALYZE runtime fields.
        let r_loops = number(block, "r_loops");
        node.loops = r_loops;
        if let Some(total_ms) = number(block, "r_total_time_ms") {
            if is_query_block || key == "subquery_cache" {
                // Wall time of the whole block.
                node.actual_time_ms = Some(total_ms);
            } else {
                // A filesort reports the time of the sort alone.
                node.actual_self_time_ms = Some(total_ms);
            }
            if is_query_block {
                node.output_list.push(format!("Actual time: {total_ms} ms"));
            }
        }
        if let Some(rows) = number(block, "r_output_rows").or_else(|| number(block, "r_rows")) {
            let total = rows * r_loops.unwrap_or(1.0);
            node.actual_rows = Some(total);
            node.actual_rows_total = Some(total);
        }
        node.peak_memory_kb = block.get("r_buffer_size").and_then(buffer_size_kb);
        if is_true(block, "r_used_priority_queue") {
            node.sort_method = Some("priority queue".to_string());
        }
        if let Some(sort_key) = text(block, "sort_key") {
            node.sort_keys = split_top_level(sort_key);
        }
        if is_true(block, "using_temporary_table") {
            node.extra.push("Using temporary".to_string());
        }
        if is_true(block, "using_filesort") {
            node.extra.push("Using filesort".to_string());
        }
        if let Some(condition) = text(block, "attached_condition") {
            if key == "block-nl-join" {
                node.join_predicate = Some(condition.to_string());
            } else {
                node.filter = Some(condition.to_string());
            }
            node.output_list.push(format!("Condition: {condition}"));
        }
        node.properties = detail_properties(block, &["sort_key"]);
        self.push(node, parent)
    }

    fn table_node(&mut self, table: &Map<String, Value>, parent: i32) -> i32 {
        let id = self.nodes.len() as i32 + 1;
        let name = text(table, "table_name").unwrap_or("?");
        let access = text(table, "access_type").unwrap_or("");
        let internal = name.starts_with('<');
        let derived = table
            .keys()
            .any(|key| is_materialization(key) && table.get(key).is_some_and(Value::is_object));

        let mut node = PlanNode {
            id,
            logical_op: format!("Access: {}", if access.is_empty() { "?" } else { access }),
            physical_op: format!("Table: {name}"),
            ..Default::default()
        };
        // `ALL` is the full table scan. Reading back a materialized result
        // (`<union1,2>`, a derived table) is not a table the user can index.
        if access == "ALL" && !internal && !derived {
            node.warnings.push(PlanWarning::TableScan {
                table: name.to_string(),
                schema: String::new(),
            });
        }
        if derived {
            node.alias = Some(name.to_string());
        } else if !internal && name != "?" {
            node.relation = Some(name.to_string());
        }
        node.output_list.push(name.to_string());
        if let Some(key) = text(table, "key") {
            node.output_list.push(format!("Key: {key}"));
            node.index_name = Some(key.to_string());
        }
        if let Some(condition) = text(table, "attached_condition") {
            node.output_list.push(format!("Condition: {condition}"));
            node.filter = Some(condition.to_string());
        }
        node.index_cond = text(table, "index_condition").map(str::to_string);

        // Estimates. MySQL names the rows read per scan
        // `rows_examined_per_scan`; MariaDB just `rows`.
        let rows = number(table, "rows_examined_per_scan").or_else(|| number(table, "rows"));
        node.estimated_rows = rows.unwrap_or(0.0);
        node.estimated_rows_read = rows;
        node.filtered_percent = number(table, "filtered");
        let cost_info = table.get("cost_info");
        node.estimated_cost = cost_info
            .and_then(|c| c.get("prefix_cost"))
            .or_else(|| table.get("cost"))
            .map(number_or_string)
            .unwrap_or(0.0);
        node.estimated_io = cost_info
            .and_then(|c| c.get("read_cost"))
            .map(number_or_string)
            .unwrap_or(0.0);
        // MariaDB 11: how many times the optimizer expects to scan the table.
        node.estimated_executions = number(table, "loops");
        node.output_columns = string_list(table, "used_columns");
        node.partitions = string_list(table, "partitions");

        // MariaDB ANALYZE runtime fields. `r_rows` is the average rows read
        // per scan and `r_loops` the number of scans, so rows read in total
        // is the product. `r_rows` is null for a table that was never read.
        let r_loops = number(table, "r_loops");
        node.loops = r_loops;
        node.actual_rebinds = r_loops.map(|l| l as i32);
        match number(table, "r_rows") {
            Some(per_scan) => {
                let total = per_scan * r_loops.unwrap_or(1.0);
                node.actual_rows = Some(total);
                node.actual_rows_total = Some(total);
                node.rows_read = Some(total);
                if let Some(r_filtered) = number(table, "r_filtered") {
                    node.rows_removed_by_filter =
                        Some((total * (1.0 - r_filtered / 100.0)).max(0.0));
                }
            }
            None if matches!(table.get("r_rows"), Some(Value::Null)) => {
                node.never_executed = true;
            }
            None => {}
        }
        // Newer MariaDB splits per-table time into r_table_time_ms +
        // r_other_time_ms; older versions report r_total_time_ms.
        if let Some(time_ms) = number(table, "r_total_time_ms").or(number(table, "r_table_time_ms"))
        {
            node.output_list.push(format!("Actual time: {time_ms} ms"));
        }
        let own_time = number(table, "r_total_time_ms").or_else(|| {
            number(table, "r_table_time_ms")
                .map(|table_ms| table_ms + number(table, "r_other_time_ms").unwrap_or(0.0))
        });
        node.actual_time_ms = own_time;
        node.actual_self_time_ms = own_time;
        if let Some(filtered) = number(table, "r_filtered") {
            node.output_list
                .push(format!("Actual filtered: {filtered}%"));
        }
        if let Some(stats) = table.get("r_engine_stats").and_then(Value::as_object) {
            let logical = number(stats, "pages_accessed");
            let physical = number(stats, "pages_read_count");
            if logical.is_some() || physical.is_some() {
                node.buffers = Some(PlanBuffers {
                    logical_reads: logical,
                    physical_reads: physical,
                    ..Default::default()
                });
            }
        }

        // Flags the tabular form prints in `Extra`.
        if is_true(table, "using_index") {
            node.extra.push("Using index".to_string());
        }
        if is_true(table, "using_index_for_group_by") {
            node.extra.push("Using index for group-by".to_string());
        }
        if is_true(table, "backward_index_scan") {
            node.extra.push("Backward index scan".to_string());
        }
        if is_true(table, "using_MRR") {
            node.extra.push("Using MRR".to_string());
        }
        if is_true(table, "not_exists") {
            node.extra.push("Not exists".to_string());
        }
        if let Some(buffer) = text(table, "using_join_buffer") {
            node.extra.push(format!("Using join buffer ({buffer})"));
        }
        if table.get("rowid_filter").is_some_and(Value::is_object) {
            node.extra.push("Using rowid filter".to_string());
        }

        node.properties = detail_properties(
            table,
            &[
                "table_name",
                "access_type",
                "key",
                "attached_condition",
                "index_condition",
                "used_columns",
                "partitions",
                "rows_examined_per_scan",
                "rows",
                "filtered",
                "r_rows",
                "r_loops",
            ],
        );
        self.push(node, parent)
    }
}

// ---------------------------------------------------------------------------
// MySQL 8+: EXPLAIN FORMAT=TREE and EXPLAIN ANALYZE text
// ---------------------------------------------------------------------------

/// Parse MySQL `EXPLAIN FORMAT=TREE` and `EXPLAIN ANALYZE` text into the
/// shared model. Lines look like
///
/// ```text
/// -> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.055..0.058 rows=2 loops=1)
///     -> Table scan on p  (cost=0.7 rows=2) (actual time=0.033..0.034 rows=2 loops=1)
/// ```
///
/// with indentation giving tree depth. A second top-level line (`-> Select
/// #2 (subquery in projection; dependent)`) is attached under the first.
/// The client decorations around the text are tolerated: the `EXPLAIN`
/// column header, the box (`| -> ...` on the first line, ` |` on the last),
/// the `\G` form (`EXPLAIN: -> ...`) and batch mode, which prints the line
/// breaks as a literal `\n`.
///
/// Each node keeps the operator text as its name and gets the parts in typed
/// fields: table, index, lookup condition or ranges, filter, sort keys, join
/// type and hash join condition.
///
/// The `rows` in the actual section is the average per loop; the total,
/// rows x loops, goes into [`PlanNode::actual_rows`] and
/// [`PlanNode::actual_rows_total`], with the loop count in
/// [`PlanNode::loops`] (and [`PlanNode::actual_rebinds`]). `time=a..b` is
/// milliseconds to the first and to the last row of one loop:
/// `actual_startup_ms` is `a` and `actual_time_ms` is `b` x loops. The node's
/// output list keeps `b` as `Actual time: b ms`. Operators without an actual
/// section (plain `FORMAT=TREE`) keep `actual_rows` as `None`, and `(never
/// executed)` sets `never_executed`.
pub fn parse_mysql_explain_analyze_tree(doc: &str) -> PlanResult<ExecutionPlan> {
    // Batch mode (`mysql -B`, a copied cell) escapes the line breaks.
    let unescaped;
    let doc = if doc.contains("\\n")
        && doc
            .lines()
            .filter(|line| line.trim_start().starts_with("-> "))
            .count()
            <= 1
    {
        unescaped = doc.replace("\\n", "\n");
        unescaped.as_str()
    } else {
        doc
    };

    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    // Ancestors of the node being added, as (indent, node id).
    let mut stack: Vec<(usize, i32)> = Vec::new();
    for line in doc.lines() {
        // Blank lines and stray non-operator lines (headers, borders, a
        // footer) are skipped.
        let Some((indent, rest)) = tree_operator_line(line) else {
            continue;
        };
        if nodes.len() >= MAX_NODES {
            break;
        }
        let id = nodes.len() as i32 + 1;
        let node = tree_node(id, rest).ok_or_else(|| {
            PlanError::InvalidStructure(format!(
                "EXPLAIN ANALYZE: operator line without a name `{}`",
                line.trim()
            ))
        })?;
        nodes.push(node);
        // The first line is the root; a later line at its depth (a subquery
        // of the select list) hangs under it.
        while stack.len() > 1 && stack.last().is_some_and(|(i, _)| *i >= indent) {
            stack.pop();
        }
        if stack.len() > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "EXPLAIN ANALYZE: the plan is nested deeper than 512 levels".into(),
            ));
        }
        if let Some((_, parent)) = stack.last() {
            edges.push(PlanEdge {
                from_node: *parent,
                to_node: id,
            });
        }
        if stack.is_empty() || stack.last().is_some_and(|(i, _)| *i < indent) {
            stack.push((indent, id));
        }
    }
    let Some(root) = nodes.first() else {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN ANALYZE: no `-> operator` lines found".into(),
        ));
    };
    if nodes.len() == 1 && root.physical_op == NOT_EXECUTABLE {
        return Err(PlanError::Other(
            "MySQL printed no plan tree for this statement: FORMAT=TREE and EXPLAIN ANALYZE only describe statements the iterator executor runs. Use EXPLAIN FORMAT=TRADITIONAL, or EXPLAIN FORMAT=JSON with explain_json_format_version = 1.".into(),
        ));
    }
    let analyzed = nodes
        .iter()
        .any(|n| n.actual_rows.is_some() || n.never_executed);
    let mut plan = ExecutionPlan {
        statement_type: "EXPLAIN ANALYZE".to_string(),
        total_subtree_cost: root.estimated_cost,
        estimated_rows: root.estimated_rows,
        dialect: Some(PlanDialect::MySql),
        source_format: Some("mysql-tree".to_string()),
        has_actuals: analyzed,
        // The root's time to its last row is the statement's run time.
        execution_time_ms: root.actual_time_ms,
        nodes,
        edges,
        ..Default::default()
    };
    if !analyzed {
        plan.statement_type = "EXPLAIN".to_string();
    }
    common::finish_plan(&mut plan);
    Ok(plan)
}

/// The indentation and text after `-> ` of an operator line, with the
/// client's `EXPLAIN: ` (`\G`) or `| ` (box) prefix and a trailing ` |`
/// removed. `None` for every other line.
fn tree_operator_line(line: &str) -> Option<(usize, &str)> {
    let line = line.trim_end();
    let stripped = line.trim_start();
    let mut indent = line.chars().count() - stripped.chars().count();
    let mut body = stripped;
    for prefix in ["EXPLAIN:", "|"] {
        if let Some(after) = body.strip_prefix(prefix) {
            body = after.trim_start();
            indent = 0;
        }
    }
    let rest = body.strip_prefix("-> ")?;
    let rest = rest.strip_suffix('|').unwrap_or(rest).trim();
    Some((indent, rest))
}

/// The value of `key` (`rows=`, `cost=`, `time=`, `loops=`) in one
/// parenthesised annotation.
fn tree_field<'a>(section: &'a str, key: &str) -> Option<&'a str> {
    let mut from = 0;
    while let Some(found) = section.get(from..)?.find(key) {
        let at = from + found;
        let after = at + key.len();
        let starts_word = section
            .get(..at)
            .and_then(|before| before.chars().next_back())
            .is_none_or(|c| c == ' ' || c == '(');
        if starts_word {
            let rest = section.get(after..)?;
            let end = rest.find([' ', ')']).unwrap_or(rest.len());
            return rest.get(..end);
        }
        from = after;
    }
    None
}

/// `a..b` or a single number, as (first, last).
fn tree_range(value: &str) -> (Option<f64>, Option<f64>) {
    let read = |text: &str| text.parse::<f64>().ok().filter(|n| n.is_finite());
    match value.split_once("..") {
        Some((first, last)) => (read(first), read(last)),
        None => (None, read(value)),
    }
}

/// Extract `key<value>` from a TREE annotation section and parse the value,
/// taking the LAST number of an `a..b` range (for `time=0.041..0.045`, the
/// time to the last row; a `cost=a..b` range reads the same way).
fn tree_field_last(section: &str, key: &str) -> Option<f64> {
    tree_range(tree_field(section, key)?).1
}

/// The text inside the parentheses that open at the start of `text`, and
/// what follows the closing one.
fn leading_parens(text: &str) -> Option<(&str, &str)> {
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for (at, c) in text.char_indices() {
        if at == 0 && c != '(' {
            return None;
        }
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' | '`' => quote = Some(c),
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((text.get(1..at)?, text.get(at + 1..)?));
                    }
                }
                _ => {}
            },
        }
    }
    None
}

/// `(a = b)` without its outer parentheses when they wrap the whole text.
fn unwrap_parens(text: &str) -> &str {
    let text = text.trim();
    match leading_parens(text) {
        Some((inner, rest)) if rest.trim().is_empty() => inner.trim(),
        _ => text,
    }
}

/// One operator line (the text after `-> `) as a node. `None` when the line
/// has annotations but no operator text.
fn tree_node(id: i32, rest: &str) -> Option<PlanNode> {
    // Split into operator text, `(cost=... rows=...)` and `(actual time=...
    // rows=... loops=...)`. Either annotation may be absent.
    let cost_at = rest.find("(cost=");
    let actual_at = rest.rfind("(actual ");
    let never_at = rest.rfind("(never executed)");
    let name_end = [cost_at, actual_at, never_at]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let name = rest.get(..name_end)?.trim();
    if name.is_empty() {
        return None;
    }
    let cost_section = cost_at.and_then(|at| {
        let end = [actual_at, never_at]
            .into_iter()
            .flatten()
            .filter(|end| *end > at)
            .min()
            .unwrap_or(rest.len());
        rest.get(at..end)
    });
    let actual_section = actual_at.and_then(|at| rest.get(at..));

    let mut node = PlanNode {
        id,
        logical_op: name.to_string(),
        physical_op: name.to_string(),
        ..Default::default()
    };
    if let Some(section) = cost_section {
        let (first, last) = tree_field(section, "cost=")
            .map(tree_range)
            .unwrap_or((None, None));
        node.estimated_cost = last.unwrap_or(0.0);
        node.estimated_startup_cost = first;
        node.estimated_rows = tree_field_last(section, "rows=").unwrap_or(0.0);
    }
    if let Some(section) = actual_section {
        let loops = tree_field_last(section, "loops=");
        let times = loops.unwrap_or(1.0);
        node.loops = loops;
        node.actual_rebinds = loops.map(|l| l as i32);
        if let Some(per_loop) = tree_field_last(section, "rows=") {
            node.actual_rows = Some(per_loop * times);
            node.actual_rows_total = Some(per_loop * times);
        }
        let (first, last) = tree_field(section, "time=")
            .map(tree_range)
            .unwrap_or((None, None));
        node.actual_startup_ms = first;
        if let Some(last_row_ms) = last {
            node.actual_time_ms = Some(last_row_ms * times);
            node.output_list
                .push(format!("Actual time: {last_row_ms} ms"));
        }
    }
    node.never_executed = never_at.is_some();
    tree_operator_details(&mut node, name);
    Some(node)
}

/// Fill the typed fields the operator text spells out.
fn tree_operator_details(node: &mut PlanNode, name: &str) {
    let lower = name.to_ascii_lowercase();
    if let Some(condition) = name.strip_prefix("Filter:") {
        node.filter = Some(unwrap_parens(condition).to_string());
        return;
    }
    if name.starts_with("Select #") {
        node.subplan_name = Some(name.to_string());
        return;
    }
    if let Some(at) = name.find("CTE ")
        && lower.starts_with("materialize")
    {
        node.cte_name = name
            .get(at + 4..)
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_string);
        return;
    }
    if lower.starts_with("sort") {
        // `Sort: a, b DESC`, `Sort row IDs: a`, `Sort: a, limit input to 20
        // row(s) per chunk`.
        if let Some((_, keys)) = name.split_once(':') {
            for item in split_top_level(keys) {
                if item.starts_with("limit input to") {
                    node.properties.push(property("limit", &item));
                } else {
                    node.sort_keys.push(item);
                }
            }
        }
        return;
    }
    if lower.starts_with("limit") {
        if let Some((_, limit)) = name.split_once(':') {
            node.properties.push(property("limit", limit.trim()));
        }
        return;
    }
    if lower.contains("aggregate") {
        if let Some((_, functions)) = name.split_once(':') {
            node.properties
                .push(property("functions", functions.trim()));
        }
        return;
    }
    if lower.contains("join") {
        node.join_type = Some(
            if lower.contains("antijoin") {
                "Anti"
            } else if lower.contains("semijoin") {
                "Semi"
            } else if lower.contains("full outer") {
                "Full"
            } else if lower.contains("left") {
                "Left"
            } else if lower.contains("right") {
                "Right"
            } else {
                "Inner"
            }
            .to_string(),
        );
        // `Inner hash join (c.`name` = p.`name`)`: the condition follows.
        if let Some(at) = lower.find("join") {
            let after = name.get(at + 4..).unwrap_or("").trim();
            if let Some((condition, _)) = leading_parens(after) {
                node.join_predicate = Some(condition.trim().to_string());
            }
        }
        return;
    }

    // Table access: `Table scan on T`, `Index lookup on T using I (cond)`,
    // `Covering index range scan on T using I over (ranges) (reverse)`, ...
    let Some(on_at) = name.find(" on ") else {
        return;
    };
    let head = lower.get(..on_at).unwrap_or("");
    if !(head.ends_with("scan") || head.ends_with("lookup") || head.ends_with("search")) {
        return;
    }
    let mut after = name.get(on_at + 4..).unwrap_or("").trim();
    let table_end = after.find(' ').unwrap_or(after.len());
    let table = after.get(..table_end).unwrap_or("");
    after = after.get(table_end..).unwrap_or("").trim();
    if table.is_empty() {
        return;
    }
    node.output_list.push(table.to_string());
    let internal = table.starts_with('<');
    if !internal {
        node.relation = Some(table.to_string());
    }
    // "Table scan on <t>" is the full table scan. Reading back the server's
    // own `<temporary>` table (GROUP BY, DISTINCT, UNION) is not a table the
    // user can index.
    if head == "table scan" && !internal {
        node.warnings.push(PlanWarning::TableScan {
            table: table.to_string(),
            schema: String::new(),
        });
    }
    if head.starts_with("covering") {
        node.extra.push("Using index".to_string());
    }
    if let Some(rest) = after.strip_prefix("using ") {
        let index_end = rest.find(' ').unwrap_or(rest.len());
        if let Some(index) = rest.get(..index_end).filter(|index| !index.is_empty()) {
            node.index_name = Some(index.to_string());
            node.output_list.push(format!("Key: {index}"));
        }
        after = rest.get(index_end..).unwrap_or("").trim();
    }
    if let Some(stripped) = after.strip_suffix("(reverse)") {
        node.extra.push("Backward index scan".to_string());
        after = stripped.trim();
    }
    if let Some(ranges) = after.strip_prefix("over ") {
        node.index_cond = Some(ranges.trim().to_string());
    } else if let Some((condition, tail)) = leading_parens(after) {
        node.index_cond = Some(condition.trim().to_string());
        let tail = tail.trim().trim_start_matches(',').trim();
        if !tail.is_empty() {
            node.properties.push(property("detail", tail));
        }
    } else if !after.is_empty() {
        node.properties.push(property("detail", after));
    }
}

/// Real captured MySQL/MariaDB documents shared between this module's tests
/// and the dispatch-routing tests in the parent module.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Real MySQL 8.4 output for `EXPLAIN FORMAT=JSON SELECT * FROM
    /// plan_demo WHERE name = 'x'`.
    pub(crate) const MYSQL_FULL_SCAN: &str = r#"{
  "query_block": {
    "select_id": 1,
    "cost_info": {
      "query_cost": "0.35"
    },
    "table": {
      "table_name": "plan_demo",
      "access_type": "ALL",
      "rows_examined_per_scan": 1,
      "rows_produced_per_join": 1,
      "filtered": "100.00",
      "cost_info": {
        "read_cost": "0.25",
        "eval_cost": "0.10",
        "prefix_cost": "0.35",
        "data_read_per_join": "16"
      },
      "used_columns": [
        "id",
        "name"
      ],
      "attached_condition": "(`appdb`.`plan_demo`.`name` = 'x')"
    }
  }
}"#;

    /// Representative MySQL 8.4 output for `EXPLAIN ANALYZE SELECT * FROM p
    /// JOIN c ON c.parent_id = p.id WHERE c.qty > 1` — a nested loop join
    /// driving an index lookup, with a filter node carrying fractional
    /// estimated rows.
    pub(crate) const MYSQL_ANALYZE_TREE: &str = "\
-> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.0553..0.0576 rows=2 loops=1)
    -> Table scan on p  (cost=0.7 rows=2) (actual time=0.0329..0.0341 rows=2 loops=1)
    -> Filter: (c.qty > 1)  (cost=0.3 rows=0.667) (actual time=0.0104..0.0111 rows=1 loops=2)
        -> Index lookup on c using parent_id (parent_id=p.id)  (cost=0.3 rows=1) (actual time=0.00921..0.00983 rows=1 loops=2)
";

    /// Representative MariaDB 10.11 output for `ANALYZE FORMAT=JSON
    /// SELECT * FROM p JOIN c ON c.parent_id = p.id`: the EXPLAIN
    /// FORMAT=JSON shape plus per-table r_* runtime fields.
    pub(crate) const MARIADB_ANALYZE_JSON: &str = r#"{
  "query_block": {
    "select_id": 1,
    "r_loops": 1,
    "r_total_time_ms": 0.0554,
    "nested_loop": [
      {
        "table": {
          "table_name": "p",
          "access_type": "ALL",
          "r_loops": 1,
          "rows": 2,
          "r_rows": 2,
          "r_table_time_ms": 0.0177,
          "r_other_time_ms": 0.0212,
          "filtered": 100,
          "r_filtered": 100
        }
      },
      {
        "table": {
          "table_name": "c",
          "access_type": "ref",
          "possible_keys": ["parent_id"],
          "key": "parent_id",
          "key_length": "5",
          "used_key_parts": ["parent_id"],
          "ref": ["appdb.p.id"],
          "r_loops": 2,
          "rows": 1,
          "r_rows": 1,
          "r_table_time_ms": 0.0106,
          "r_other_time_ms": 0.008,
          "filtered": 100,
          "r_filtered": 100
        }
      }
    ]
  }
}"#;
}

#[cfg(test)]
mod tests {
    use super::fixtures::{MARIADB_ANALYZE_JSON, MYSQL_ANALYZE_TREE, MYSQL_FULL_SCAN};
    use super::*;
    use crate::plans::dialects::{PlanDialect, parse_captured_plan};
    use crate::plans::findings_for_plan;

    #[test]
    fn reading_back_a_temporary_table_is_not_a_table_scan() {
        let tree = "-> Sort: revenue DESC  (actual time=84.3..84.3 rows=5 loops=1)\n    -> Table scan on <temporary>  (actual time=84.3..84.3 rows=5 loops=1)\n        -> Aggregate using temporary table  (actual time=84.3..84.3 rows=5 loops=1)\n            -> Table scan on o  (cost=60294 rows=598688) (actual time=0.0888..53.9 rows=600000 loops=1)\n";
        let plan = parse_mysql_explain_analyze_tree(tree).unwrap();
        let scans: Vec<&str> = plan
            .nodes
            .iter()
            .flat_map(|n| n.warnings.iter())
            .filter_map(|w| match w {
                PlanWarning::TableScan { table, .. } => Some(table.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(scans, vec!["o"]);
    }

    #[test]
    fn mysql_full_scan_parses_and_is_too_small_for_a_finding() {
        let plan = parse_mysql_explain_json(MYSQL_FULL_SCAN).unwrap();
        assert_eq!(plan.nodes.len(), 2, "query block + table");
        assert_eq!(plan.total_subtree_cost, 0.35);
        assert_eq!(plan.nodes[1].estimated_rows, 1.0);
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn mysql_nested_loop_join_yields_one_node_per_table() {
        let doc = r#"{"query_block": {"select_id": 1, "nested_loop": [
            {"table": {"table_name": "p", "access_type": "index", "key": "PRIMARY"}},
            {"table": {"table_name": "c", "access_type": "ref", "key": "parent_id"}}
        ]}}"#;
        let plan = parse_mysql_explain_json(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert!(plan.nodes.iter().all(|n| n.warnings.is_empty()));
    }

    #[test]
    fn malformed_mysql_documents_yield_typed_errors_not_panics() {
        assert!(parse_mysql_explain_json("[1,2,3]").is_err());
    }

    #[test]
    fn mysql_explain_analyze_tree_parses_nested_loop_with_actuals() {
        let plan = parse_mysql_explain_analyze_tree(MYSQL_ANALYZE_TREE).unwrap();
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "Nested loop inner join");
        assert_eq!(root.estimated_cost, 1.4);
        assert_eq!(root.estimated_rows, 2.0);
        assert_eq!(root.actual_rows, Some(2.0));
        assert_eq!(root.actual_rebinds, Some(1));
        assert_eq!(root.output_list, vec!["Actual time: 0.0576 ms"]);
        assert_eq!(plan.total_subtree_cost, 1.4);
        assert_eq!(plan.estimated_rows, 2.0);
        // Depth from indentation: join → {table scan, filter}, filter → lookup.
        assert_eq!(
            plan.edges
                .iter()
                .map(|e| (e.from_node, e.to_node))
                .collect::<Vec<_>>(),
            vec![(1, 2), (1, 3), (3, 4)]
        );
        // The filter's fractional estimate survives.
        assert_eq!(plan.nodes[2].estimated_rows, 0.667);
        // Actual rows is the rows×loops TOTAL (rows is a per-loop average);
        // the loop count itself rides in actual_rebinds.
        let lookup = &plan.nodes[3];
        assert_eq!(lookup.actual_rows, Some(2.0));
        assert_eq!(lookup.actual_rebinds, Some(2));
        // The table scan is marked on its node (the warning count below).
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
        assert_eq!(
            plan.nodes.iter().filter(|n| !n.warnings.is_empty()).count(),
            1
        );
    }

    #[test]
    fn mysql_tree_without_actual_sections_keeps_actual_rows_none() {
        // Plain EXPLAIN FORMAT=TREE output and never-executed operators
        // carry no actual section.
        let doc = "\
-> Nested loop inner join  (cost=1.4 rows=2) (actual time=0.05..0.06 rows=2 loops=1)
    -> Table scan on p  (cost=0.7 rows=2)
    -> Index lookup on c using parent_id (parent_id=p.id)  (cost=0.3 rows=1) (never executed)
";
        let plan = parse_mysql_explain_analyze_tree(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[0].actual_rows, Some(2.0));
        assert_eq!(plan.nodes[1].actual_rows, None);
        assert_eq!(plan.nodes[1].estimated_cost, 0.7);
        assert_eq!(plan.nodes[2].actual_rows, None);
        assert_eq!(
            plan.nodes[2].physical_op.trim_end(),
            "Index lookup on c using parent_id (parent_id=p.id)"
        );
    }

    #[test]
    fn mysql_tree_with_no_operator_lines_is_a_typed_error() {
        assert!(parse_mysql_explain_analyze_tree("").is_err());
        assert!(parse_mysql_explain_analyze_tree("not a tree at all").is_err());
    }

    #[test]
    fn mariadb_analyze_json_populates_actual_statistics() {
        let plans = parse_captured_plan(PlanDialect::MariaDb, MARIADB_ANALYZE_JSON).unwrap();
        let plan = &plans[0];
        assert_eq!(plan.nodes.len(), 3, "query block + two tables");
        // Root carries the whole block's wall time.
        assert_eq!(plan.nodes[0].output_list, vec!["Actual time: 0.0554 ms"]);
        // p: 2 rows in 1 loop; MariaDB's `rows` maps to estimated_rows.
        let p = &plan.nodes[1];
        assert_eq!(p.estimated_rows, 2.0);
        assert_eq!(p.actual_rows, Some(2.0));
        assert_eq!(p.actual_rebinds, Some(1));
        assert!(p.output_list.iter().any(|o| o == "Actual time: 0.0177 ms"));
        assert!(p.output_list.iter().any(|o| o == "Actual filtered: 100%"));
        assert!(!p.warnings.is_empty(), "ALL access is a table scan");
        // c: r_rows is a per-loop average → actual_rows = r_rows × r_loops.
        let c = &plan.nodes[2];
        assert_eq!(c.actual_rows, Some(2.0));
        assert_eq!(c.actual_rebinds, Some(2));
    }

    #[test]
    fn mariadb_never_accessed_table_keeps_actual_rows_none() {
        // MariaDB emits r_rows: null when a table was never accessed.
        let doc = r#"{"query_block": {"select_id": 1, "r_total_time_ms": 0.01,
            "table": {"table_name": "t", "access_type": "ref", "key": "pk",
                      "rows": 1, "r_loops": 0, "r_rows": null, "r_filtered": null}}}"#;
        let plan = parse_mysql_explain_json(doc).unwrap();
        assert_eq!(plan.nodes[1].actual_rows, None);
        assert_eq!(plan.nodes[1].estimated_rows, 1.0);
    }

    #[test]
    fn mariadb_estimated_json_still_parses_without_r_fields() {
        // MariaDb estimated routes through the same JSON parser; without
        // ANALYZE there are no r_* fields and no actual statistics.
        let plans = parse_captured_plan(PlanDialect::MariaDb, MYSQL_FULL_SCAN).unwrap();
        assert_eq!(plans[0].nodes.len(), 2);
        assert!(plans[0].nodes.iter().all(|n| n.actual_rows.is_none()));
        assert!(plans[0].nodes.iter().all(|n| n.actual_rebinds.is_none()));
    }

    macro_rules! fixture {
        ($path:literal) => {
            include_str!(concat!("../../../tests/golden/plans/", $path))
        };
    }

    fn edge_pairs(plan: &ExecutionPlan) -> Vec<(i32, i32)> {
        plan.edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect()
    }

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.physical_op.as_str()).collect()
    }

    fn close(actual: Option<f64>, expected: f64) -> bool {
        actual.is_some_and(|a| (a - expected).abs() <= expected.abs() * 1e-9 + 1e-12)
    }

    #[test]
    fn mysql_v1_json_join_keeps_ordering_and_grouping_blocks() {
        let plan = parse_mysql_explain_json(fixture!("mysql/mysql84-q1-join-v1.json")).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MySql));
        assert_eq!(plan.source_format.as_deref(), Some("mysql-json"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(plan.total_subtree_cost, 8959.45);
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Ordering operation",
                "Grouping operation",
                "Table: o",
                "Table: c",
                "Table: oi"
            ]
        );
        assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (3, 4), (3, 5), (3, 6)]);
        assert_eq!(plan.nodes[0].subplan_name.as_deref(), Some("Select #1"));
        assert!(plan.nodes[1].extra.iter().any(|e| e == "Using filesort"));
        assert!(plan.nodes[2].extra.iter().any(|e| e == "Using temporary"));
        let o = &plan.nodes[3];
        assert_eq!(o.logical_op, "Access: ALL");
        assert_eq!(o.relation.as_deref(), Some("o"));
        assert_eq!(o.estimated_rows, 40131.0);
        assert_eq!(o.estimated_rows_read, Some(40131.0));
        assert_eq!(o.filtered_percent, Some(10.0));
        assert_eq!(o.estimated_cost, 4053.35);
        assert_eq!(o.estimated_io, 3652.04);
        assert_eq!(o.filter.as_deref(), Some("(`shop`.`o`.`status` = 'paid')"));
        assert_eq!(o.output_columns, ["id", "customer_id", "status"]);
        assert!(matches!(
            o.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "o"
        ));
        let c = &plan.nodes[4];
        assert_eq!(c.logical_op, "Access: eq_ref");
        assert_eq!(c.index_name.as_deref(), Some("PRIMARY"));
        assert_eq!(c.property("key_length"), Some("4"));
        assert_eq!(c.property("ref"), Some("shop.o.customer_id"));
        assert_eq!(c.estimated_cost, 5457.94);
        assert!(c.warnings.is_empty());
        let oi = &plan.nodes[5];
        assert_eq!(oi.index_name.as_deref(), Some("order_items_order_idx"));
        assert_eq!(oi.estimated_rows, 2.0);
        assert!(
            oi.output_list
                .iter()
                .any(|l| l == "Key: order_items_order_idx")
        );
    }

    #[test]
    fn mysql_v1_json_subqueries_hang_under_what_reads_them() {
        let plan =
            parse_mysql_explain_json(fixture!("mysql/mysql84-q3-subqueries-v1.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Ordering operation",
                "Table: c",
                "Table: <subquery3>",
                "Materialized subquery",
                "Query Block",
                "Table: orders",
                "Query Block",
                "Table: o"
            ]
        );
        assert_eq!(
            edge_pairs(&plan),
            [
                (1, 2),
                (2, 3),
                (2, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (2, 8),
                (8, 9)
            ]
        );
        let sub = &plan.nodes[3];
        assert_eq!(sub.relation, None);
        assert_eq!(sub.index_name.as_deref(), Some("<auto_distinct_key>"));
        assert!(plan.nodes[4].extra.iter().any(|e| e == "Using temporary"));
        let orders = &plan.nodes[6];
        assert_eq!(orders.estimated_rows, 40131.0);
        assert_eq!(orders.filtered_percent, Some(33.33));
        let select2 = &plan.nodes[7];
        assert_eq!(select2.subplan_name.as_deref(), Some("Select #2"));
        assert_eq!(select2.estimated_cost, 2.82);
        assert_eq!(
            select2.property("subquery_list"),
            Some("select_list_subqueries")
        );
        assert_eq!(select2.property("dependent"), Some("true"));
        assert_eq!(select2.property("cacheable"), Some("false"));
        assert_eq!(
            plan.nodes[8].index_name.as_deref(),
            Some("orders_customer_idx")
        );
        assert_eq!(plan.nodes[8].estimated_rows, 8.0);
    }

    #[test]
    fn mysql_v1_json_derived_table_is_an_alias_over_its_query_block() {
        let plan = parse_mysql_explain_json(fixture!("mysql/mysql84-derived-v1.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Table: c",
                "Table: d",
                "Materialized subquery",
                "Query Block",
                "Grouping operation",
                "Table: orders"
            ]
        );
        assert_eq!(
            edge_pairs(&plan),
            [(1, 2), (1, 3), (3, 4), (4, 5), (5, 6), (6, 7)]
        );
        let d = &plan.nodes[2];
        assert_eq!(d.alias.as_deref(), Some("d"));
        assert_eq!(d.relation, None);
        assert_eq!(d.index_name.as_deref(), Some("<auto_key0>"));
        assert_eq!(d.estimated_rows, 10.0);
        assert!(d.warnings.is_empty());
        assert_eq!(plan.nodes[3].property("cacheable"), Some("true"));
        assert_eq!(plan.nodes[4].estimated_cost, 4053.35);
        assert_eq!(plan.nodes[6].logical_op, "Access: index");
    }

    #[test]
    fn mysql_v1_json_union_window_and_partitions() {
        let plan = parse_mysql_explain_json(fixture!("mysql/mysql84-q4-union-v1.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Union result",
                "Query Block",
                "Table: customers",
                "Query Block",
                "Table: orders"
            ]
        );
        assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (3, 4), (2, 5), (5, 6)]);
        assert!(plan.nodes[1].output_list.iter().any(|l| l == "<union1,2>"));
        assert!(plan.nodes[1].extra.iter().any(|e| e == "Using temporary"));
        assert_eq!(plan.nodes[2].estimated_cost, 502.80);
        assert_eq!(plan.nodes[3].filtered_percent, Some(10.0));

        let plan = parse_mysql_explain_json(fixture!("mysql/mysql84-q6-window-v1.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Windowing",
                "Grouping operation",
                "Table: orders"
            ]
        );
        assert_eq!(plan.total_subtree_cost, 24118.35);
        assert_eq!(
            plan.nodes[1].property("cost_info.sort_cost"),
            Some("20065.00")
        );
        assert!(
            plan.nodes[1]
                .property("windows")
                .is_some_and(|w| w.contains("rank"))
        );
        assert_eq!(plan.nodes[3].estimated_io, 2046.85);

        let plan =
            parse_mysql_explain_json(fixture!("mysql/mysql84-q5-partitions-v1.json")).unwrap();
        assert_eq!(
            ops(&plan),
            ["Query Block", "Grouping operation", "Table: events"]
        );
        assert!(!plan.nodes[2].partitions.is_empty());
    }

    #[test]
    fn mariadb_explain_json_is_recognised_without_runtime_fields() {
        let plan =
            parse_mysql_explain_json(fixture!("mariadb/mariadb-q1-join-explain.json")).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(plan.source_format.as_deref(), Some("mariadb-json"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Filesort",
                "Temporary table",
                "Table: o",
                "Table: c",
                "Table: oi"
            ]
        );
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
        assert!(plan.nodes.iter().all(|n| !n.never_executed));
    }

    #[test]
    fn mariadb_analyze_json_join_fills_runtime_fields() {
        let plan =
            parse_mysql_explain_json(fixture!("mariadb/mariadb-q1-join-analyze.json")).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MariaDb));
        assert_eq!(plan.source_format.as_deref(), Some("mariadb-json"));
        assert_eq!(plan.statement_type, "ANALYZE");
        assert!(plan.has_actuals);
        assert!(close(plan.planning_time_ms, 0.320105487));
        assert!(close(plan.execution_time_ms, 28.68135161));
        assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (3, 4), (3, 5), (3, 6)]);
        let root = &plan.nodes[0];
        assert_eq!(root.estimated_cost, 155.8313818);
        assert_eq!(root.output_list, ["Actual time: 28.68135161 ms"]);
        let sort = &plan.nodes[1];
        assert_eq!(sort.physical_op, "Filesort");
        assert_eq!(sort.sort_keys, ["sum(oi.quantity * oi.unit_price) desc"]);
        assert_eq!(sort.actual_rows, Some(2.0));
        assert!(close(sort.actual_self_time_ms, 0.026453277));
        assert!(close(sort.peak_memory_kb, 544.0 / 1024.0));
        assert_eq!(sort.property("r_sort_mode"), Some("sort_key,rowid"));
        let o = &plan.nodes[3];
        assert_eq!(o.estimated_rows, 40131.0);
        assert_eq!(o.estimated_executions, Some(1.0));
        assert_eq!(o.estimated_cost, 6.6889758);
        assert_eq!(o.actual_rows, Some(40000.0));
        assert_eq!(o.rows_read, Some(40000.0));
        assert_eq!(o.loops, Some(1.0));
        assert_eq!(o.rows_removed_by_filter, Some(30000.0));
        assert_eq!(o.filter.as_deref(), Some("o.`status` = 'paid'"));
        assert!(close(o.actual_time_ms, 2.737393482 + 1.157403791));
        assert_eq!(
            o.buffers.as_ref().and_then(|b| b.logical_reads),
            Some(118.0)
        );
        assert_eq!(o.property("r_total_filtered"), Some("25"));
        let c = &plan.nodes[4];
        assert_eq!(c.estimated_executions, Some(40131.0));
        assert_eq!(c.loops, Some(10000.0));
        assert_eq!(c.actual_rows, Some(10000.0));
        let oi = &plan.nodes[5];
        assert_eq!(oi.actual_rows_total, Some(25000.0));
        assert_eq!(oi.actual_rows_per_loop(), Some(2.5));
        assert_eq!(oi.estimated_cost, 113.229612);
    }

    #[test]
    fn mariadb_analyze_json_subqueries_union_and_window() {
        let plan = parse_mysql_explain_json(fixture!("mariadb/mariadb-q3-subqueries-analyze.json"))
            .unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Read sorted file",
                "Filesort",
                "Table: c",
                "Table: <subquery3>",
                "Materialized subquery",
                "Query Block",
                "Table: orders",
                "Subquery cache",
                "Query Block",
                "Table: o"
            ]
        );
        assert_eq!(
            edge_pairs(&plan),
            [
                (1, 2),
                (2, 3),
                (3, 4),
                (1, 5),
                (5, 6),
                (6, 7),
                (7, 8),
                (1, 9),
                (9, 10),
                (10, 11)
            ]
        );
        assert_eq!(plan.nodes[1].actual_rows, Some(295.0));
        assert_eq!(plan.nodes[2].sort_keys, ["c.`name`"]);
        assert_eq!(plan.nodes[2].actual_rows, Some(5000.0));
        assert_eq!(plan.nodes[2].peak_memory_kb, Some(2047.0));
        assert_eq!(plan.nodes[4].loops, Some(295.0));
        assert!(close(plan.nodes[4].actual_rows, 0.06779661 * 295.0));
        assert_eq!(plan.nodes[5].property("unique"), Some("1"));
        assert_eq!(plan.nodes[7].filter.as_deref(), Some("orders.total > 900"));
        assert_eq!(plan.nodes[8].loops, Some(20.0));
        assert_eq!(plan.nodes[8].property("r_hit_ratio"), Some("0"));
        assert_eq!(plan.nodes[8].property("subquery_list"), Some("subqueries"));
        assert_eq!(plan.nodes[9].subplan_name.as_deref(), Some("Select #2"));
        assert!(close(plan.nodes[9].actual_time_ms, 0.160344354));
        assert_eq!(plan.nodes[10].actual_rows, Some(160.0));

        let plan =
            parse_mysql_explain_json(fixture!("mariadb/mariadb-q4-union-analyze.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Union result",
                "Query Block",
                "Table: customers",
                "Query Block",
                "Table: orders"
            ]
        );
        assert_eq!(plan.nodes[1].actual_rows, Some(691.0));
        assert!(close(plan.nodes[2].actual_time_ms, 0.416753681));
        assert_eq!(plan.nodes[3].rows_removed_by_filter, Some(4375.0));
        assert_eq!(plan.nodes[4].property("operation"), Some("UNION"));
        assert_eq!(plan.nodes[5].actual_rows, Some(40000.0));
        assert!(close(plan.planning_time_ms, 0.074110836));

        let plan =
            parse_mysql_explain_json(fixture!("mariadb/mariadb-q6-window-analyze.json")).unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Filesort",
                "Window functions computation",
                "Filesort",
                "Temporary table",
                "Table: orders"
            ]
        );
        assert_eq!(plan.nodes[1].sort_keys, ["orders.`status`"]);
        assert_eq!(plan.nodes[1].peak_memory_kb, Some(3.0));
        assert_eq!(plan.nodes[3].sort_keys, ["count(0) desc"]);
        assert!(close(plan.nodes[3].peak_memory_kb, 360.0 / 1024.0));
        assert_eq!(
            plan.nodes[5].buffers.as_ref().and_then(|b| b.logical_reads),
            Some(130.0)
        );
    }

    #[test]
    fn mariadb_analyze_json_block_join_rowid_filter_and_unread_table() {
        let plan = parse_mysql_explain_json(fixture!("mariadb/mariadb-block-nl-join-analyze.json"))
            .unwrap();
        assert_eq!(
            ops(&plan),
            [
                "Query Block",
                "Table: p",
                "Block nested loop join",
                "Table: c"
            ]
        );
        assert_eq!(edge_pairs(&plan), [(1, 2), (1, 3), (3, 4)]);
        let join = &plan.nodes[2];
        assert_eq!(join.join_predicate.as_deref(), Some("c.`name` = p.`name`"));
        assert_eq!(join.property("buffer_type"), Some("flat"));
        assert_eq!(join.property("buffer_size"), Some("256KiB"));
        assert_eq!(join.property("join_type"), Some("BNL"));
        assert_eq!(join.property("r_effective_rows"), Some("5000"));
        assert_eq!(plan.nodes[3].estimated_executions, Some(2000.0));
        assert_eq!(plan.nodes[3].actual_rows, Some(5000.0));

        let plan = parse_mysql_explain_json(fixture!("mariadb/mariadb-rowid-filter-analyze.json"))
            .unwrap();
        let o = &plan.nodes[2];
        assert_eq!(o.physical_op, "Table: o");
        assert!(o.extra.iter().any(|e| e == "Using rowid filter"));
        assert_eq!(
            o.property("rowid_filter.range.key"),
            Some("orders_placed_idx")
        );
        assert_eq!(o.property("rowid_filter.r_lookups"), Some("10000"));
        assert_eq!(o.property("r_index_rows"), Some("8"));
        assert_eq!(o.loops, Some(1250.0));
        assert!(close(o.actual_rows, 1360.0));
        assert_eq!(
            o.filter.as_deref(),
            Some("trigcond(o.placed_at > '2023-01-25')")
        );

        let plan =
            parse_mysql_explain_json(fixture!("mariadb/mariadb-never-executed-analyze.json"))
                .unwrap();
        let c = &plan.nodes[1];
        assert_eq!(
            c.index_cond.as_deref(),
            Some("c.email = 'nobody@example.com'")
        );
        assert_eq!(c.actual_rows, Some(0.0));
        assert!(!c.never_executed);
        let o = &plan.nodes[2];
        assert!(o.never_executed);
        assert_eq!(o.actual_rows, None);
        assert_eq!(o.loops, Some(0.0));
    }

    #[test]
    fn v1_entry_point_hands_version_2_documents_to_the_v2_parser() {
        let plan = parse_mysql_explain_json(fixture!("mysql/mysql84-q1-join-v2.json")).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("mysql-json-v2"));
        assert!(parse_mysql_explain_json("{\"a\": 1}").is_err());
        assert!(parse_mysql_explain_json("{\"query_block\": 3}").is_err());
    }

    #[test]
    fn deeply_nested_json_is_an_error_not_a_stack_overflow() {
        let mut doc = String::from("{\"query_block\": ");
        for _ in 0..100 {
            doc.push_str("{\"ordering_operation\": ");
        }
        doc.push_str("{\"table\": {\"table_name\": \"t\", \"access_type\": \"ALL\"}}");
        for _ in 0..100 {
            doc.push('}');
        }
        doc.push('}');
        // serde_json refuses documents nested deeper than 128 levels, which
        // is well inside the walker's own limit.
        let _ = parse_mysql_explain_json(&doc);
        let shallow = "{\"query_block\": {\"ordering_operation\": {\"table\": {\"table_name\": \"t\", \"access_type\": \"ALL\"}}}}";
        assert_eq!(parse_mysql_explain_json(shallow).unwrap().nodes.len(), 3);
    }

    #[test]
    fn tree_analyze_join_reads_every_annotation() {
        let plan =
            parse_mysql_explain_analyze_tree(fixture!("mysql/mysql84-q1-join-analyze-tree.txt"))
                .unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MySql));
        assert_eq!(plan.source_format.as_deref(), Some("mysql-tree"));
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        assert_eq!(plan.execution_time_ms, Some(36.1));
        assert_eq!(plan.nodes.len(), 9);
        assert_eq!(
            edge_pairs(&plan),
            [
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (5, 6),
                (6, 7),
                (5, 8),
                (4, 9)
            ]
        );
        assert_eq!(plan.nodes[0].sort_keys, ["revenue DESC"]);
        assert_eq!(plan.nodes[1].relation, None);
        assert!(plan.nodes[1].warnings.is_empty());
        let join = &plan.nodes[3];
        assert_eq!(join.physical_op, "Nested loop inner join");
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.estimated_cost, 8959.0);
        assert_eq!(join.estimated_rows, 10004.0);
        assert_eq!(join.actual_rows, Some(25000.0));
        assert_eq!(join.actual_startup_ms, Some(0.349));
        assert_eq!(join.actual_time_ms, Some(30.7));
        let filter = &plan.nodes[5];
        assert_eq!(filter.filter.as_deref(), Some("o.`status` = 'paid'"));
        assert_eq!(filter.actual_rows, Some(10000.0));
        let scan = &plan.nodes[6];
        assert_eq!(scan.relation.as_deref(), Some("o"));
        assert_eq!(scan.estimated_rows, 40131.0);
        assert!(matches!(
            scan.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "o"
        ));
        let c = &plan.nodes[7];
        assert_eq!(c.relation.as_deref(), Some("c"));
        assert_eq!(c.index_name.as_deref(), Some("PRIMARY"));
        assert_eq!(c.index_cond.as_deref(), Some("id=o.customer_id"));
        assert_eq!(c.estimated_cost, 0.25);
        assert_eq!(c.loops, Some(10000.0));
        assert_eq!(c.actual_rows, Some(10000.0));
        assert!(close(c.actual_startup_ms, 495e-6));
        assert!(close(c.actual_time_ms, 514e-6 * 10000.0));
        let oi = &plan.nodes[8];
        assert_eq!(oi.index_name.as_deref(), Some("order_items_order_idx"));
        assert_eq!(oi.estimated_rows, 2.49);
        assert_eq!(oi.actual_rows_total, Some(25000.0));
        assert_eq!(oi.actual_rows_per_loop(), Some(2.5));
        assert!(
            oi.output_list
                .iter()
                .any(|l| l == "Actual time: 0.00179 ms")
        );
    }

    #[test]
    fn tree_without_analyze_has_estimates_only() {
        let plan =
            parse_mysql_explain_analyze_tree(fixture!("mysql/mysql84-q1-join-tree.txt")).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(plan.nodes.len(), 9);
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
        assert!(plan.nodes.iter().all(|n| n.actual_time_ms.is_none()));
        assert_eq!(plan.nodes[3].estimated_cost, 8959.0);
        assert_eq!(plan.nodes[8].estimated_rows, 2.49);
        assert_eq!(plan.nodes[8].index_cond.as_deref(), Some("order_id=o.id"));
    }

    #[test]
    fn tree_subqueries_cost_ranges_and_scientific_notation() {
        let plan = parse_mysql_explain_analyze_tree(fixture!(
            "mysql/mysql84-q3-subqueries-analyze-tree.txt"
        ))
        .unwrap();
        assert_eq!(plan.nodes.len(), 12);
        let limit = &plan.nodes[0];
        assert_eq!(limit.property("limit"), Some("20 row(s)"));
        assert_eq!(limit.estimated_cost, 6.65e6);
        assert_eq!(plan.nodes[1].estimated_rows, 66.5e6);
        assert_eq!(plan.nodes[2].sort_keys, ["c.`name`"]);
        let lookup = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.contains("<subquery3>"))
            .unwrap();
        assert_eq!(lookup.relation, None);
        assert_eq!(lookup.index_name.as_deref(), Some("<auto_distinct_key>"));
        assert_eq!(lookup.index_cond.as_deref(), Some("customer_id=c.id"));
        assert_eq!(lookup.estimated_startup_cost, Some(5391.0));
        assert_eq!(lookup.estimated_cost, 5391.0);
        assert_eq!(lookup.loops, Some(295.0));
        assert!(close(lookup.actual_rows, 0.0678 * 295.0));
        // The select-list subquery is printed as a second top-level tree; it
        // hangs under the first.
        let select = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.starts_with("Select #2"))
            .unwrap();
        assert_eq!(
            select.subplan_name.as_deref(),
            Some("Select #2 (subquery in projection; dependent)")
        );
        assert!(
            plan.edges
                .iter()
                .any(|e| e.from_node == 1 && e.to_node == select.id)
        );
        let aggregate = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.starts_with("Aggregate: max"))
            .unwrap();
        assert_eq!(aggregate.property("functions"), Some("max(o.total)"));
        assert_eq!(aggregate.loops, Some(20.0));
        assert_eq!(aggregate.actual_rows, Some(20.0));
    }

    #[test]
    fn tree_union_hash_join_reverse_scan_and_never_executed() {
        let plan =
            parse_mysql_explain_analyze_tree(fixture!("mysql/mysql84-q4-union-analyze-tree.txt"))
                .unwrap();
        assert_eq!(plan.nodes.len(), 6);
        assert_eq!(plan.nodes[0].estimated_startup_cost, Some(5943.0));
        assert_eq!(plan.nodes[0].estimated_cost, 6119.0);
        assert_eq!(plan.nodes[0].actual_rows, Some(691.0));
        assert!(plan.nodes[0].warnings.is_empty());
        assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (3, 4), (2, 5), (5, 6)]);
        assert_eq!(
            plan.nodes[4].filter.as_deref(),
            Some("orders.total < 12.00")
        );

        let plan =
            parse_mysql_explain_analyze_tree(fixture!("mysql/mysql84-hash-join-analyze-tree.txt"))
                .unwrap();
        assert_eq!(plan.nodes.len(), 6);
        assert_eq!(plan.nodes[0].property("functions"), Some("count(0)"));
        let join = &plan.nodes[1];
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.join_predicate.as_deref(), Some("c.`name` = p.`name`"));
        assert_eq!(join.estimated_cost, 331711.0);
        assert_eq!(join.actual_rows, Some(0.0));
        assert_eq!(plan.nodes[3].physical_op, "Hash");
        assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (2, 4), (4, 5), (5, 6)]);
        assert_eq!(plan.nodes[4].actual_rows, Some(1600.0));

        let plan = parse_mysql_explain_analyze_tree(fixture!(
            "mysql/mysql84-covering-reverse-analyze-tree.txt"
        ))
        .unwrap();
        let scan = &plan.nodes[2];
        assert_eq!(scan.relation.as_deref(), Some("orders"));
        assert_eq!(scan.index_name.as_deref(), Some("orders_customer_idx"));
        assert_eq!(scan.index_cond.as_deref(), Some("(customer_id < 50)"));
        assert!(scan.extra.iter().any(|e| e == "Using index"));
        assert!(scan.extra.iter().any(|e| e == "Backward index scan"));
        assert!(scan.warnings.is_empty());
        assert_eq!(scan.actual_time_ms, Some(0.0452));

        let plan = parse_mysql_explain_analyze_tree(fixture!(
            "mysql/mysql84-never-executed-analyze-tree.txt"
        ))
        .unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(
            plan.nodes[1].index_cond.as_deref(),
            Some("email='nobody@example.com'")
        );
        assert_eq!(plan.nodes[1].actual_rows, Some(0.0));
        let unread = &plan.nodes[2];
        assert!(unread.never_executed);
        assert_eq!(unread.actual_rows, None);
        assert_eq!(unread.estimated_cost, 2.82);
        assert_eq!(unread.estimated_rows, 8.06);
        assert_eq!(unread.index_cond.as_deref(), Some("customer_id=c.id"));
    }

    #[test]
    fn tree_from_mysql_9_and_from_each_client_decoration() {
        let plan =
            parse_mysql_explain_analyze_tree(fixture!("mysql/mysql97-q2-range-analyze-tree.txt"))
                .unwrap();
        assert_eq!(plan.nodes.len(), 5);
        assert_eq!(plan.execution_time_ms, Some(0.498));
        assert_eq!(
            plan.nodes[4].index_cond.as_deref(),
            Some("customer_id = c.id")
        );
        assert_eq!(
            plan.nodes[2].index_cond.as_deref(),
            Some("(100 <= id <= 140)")
        );

        for (doc, last_row_ms) in [
            (
                fixture!("mysql/mysql84-q2-range-analyze-tree-boxed.txt"),
                0.956,
            ),
            (
                fixture!("mysql/mysql84-q2-range-analyze-tree-vertical.txt"),
                0.754,
            ),
            (
                fixture!("mysql/mysql84-q2-range-analyze-tree-batch.txt"),
                0.374,
            ),
        ] {
            let plan = parse_mysql_explain_analyze_tree(doc).unwrap();
            assert_eq!(plan.nodes.len(), 5, "{last_row_ms}");
            assert_eq!(edge_pairs(&plan), [(1, 2), (2, 3), (1, 4), (4, 5)]);
            let root = &plan.nodes[0];
            assert_eq!(root.physical_op, "Nested loop inner join");
            assert_eq!(root.estimated_cost, 124.0);
            assert_eq!(root.estimated_rows, 110.0);
            assert_eq!(root.actual_rows, Some(149.0));
            assert_eq!(root.actual_time_ms, Some(last_row_ms));
            let filter = &plan.nodes[3];
            assert_eq!(filter.filter.as_deref(), Some("o.total > 500.00"));
            assert_eq!(filter.loops, Some(41.0));
            assert!(close(filter.actual_rows, 3.63 * 41.0));
            let lookup = &plan.nodes[4];
            assert_eq!(
                lookup.physical_op,
                "Index lookup on o using orders_customer_idx (customer_id=c.id)"
            );
            assert_eq!(lookup.actual_rows, Some(328.0));
        }
    }

    #[test]
    fn tree_for_a_statement_without_an_iterator_plan_is_an_error() {
        let err = parse_mysql_explain_analyze_tree("-> <not executable by iterator executor>\n")
            .unwrap_err();
        assert!(err.to_string().contains("FORMAT=TRADITIONAL"), "{err}");
    }

    #[test]
    fn mangled_documents_never_panic() {
        for doc in [
            fixture!("mysql/mysql84-q3-subqueries-v1.json"),
            fixture!("mariadb/mariadb-q3-subqueries-analyze.json"),
            fixture!("mariadb/mariadb-rowid-filter-analyze.json"),
            MARIADB_ANALYZE_JSON,
        ] {
            super::common::assert_survives_mangling(doc, |doc| {
                let _ = parse_mysql_explain_json(doc);
            });
        }
        for doc in [
            fixture!("mysql/mysql84-q3-subqueries-analyze-tree.txt"),
            fixture!("mysql/mysql84-covering-reverse-analyze-tree.txt"),
            fixture!("mysql/mysql84-q2-range-analyze-tree-boxed.txt"),
            fixture!("mysql/mysql84-q2-range-analyze-tree-batch.txt"),
            MYSQL_ANALYZE_TREE,
        ] {
            super::common::assert_survives_mangling(doc, |doc| {
                let _ = parse_mysql_explain_analyze_tree(doc);
            });
        }
        for doc in [
            "",
            "->",
            "-> ",
            "-> (cost=",
            "|",
            "EXPLAIN: -> x (actual time=..",
            "\\n",
        ] {
            let _ = parse_mysql_explain_analyze_tree(doc);
            let _ = parse_mysql_explain_json(doc);
        }
    }
}
