//! PostgreSQL `EXPLAIN` in every output format.
//!
//! `FORMAT JSON` is the reference shape: [`build_pg_node`] reads one plan
//! node object with PostgreSQL's own key names. The text parser
//! (`pg_text`), and the YAML and XML readers (`pg_structured`) turn their
//! documents into the same objects, so all four formats fill the model the
//! same way.

use std::collections::HashMap;

use serde_json::{Map, Value};

use super::PlanDialect;
use super::common::finish_plan;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{
    ExecutionPlan, PlanBuffers, PlanEdge, PlanNode, PlanProperty, PlanSpill, PlanTrigger,
};
use crate::plans::warning::PlanWarning;

/// JSON nests one level per plan node; anything deeper than this is not a
/// plan a person can read and is cut off rather than recursed into.
const MAX_DEPTH: usize = 512;

/// Parse PostgreSQL `EXPLAIN (FORMAT JSON)` / `EXPLAIN (ANALYZE, FORMAT
/// JSON)` output into the shared model. Returns the first statement's plan;
/// see [`parse_pg_explain_json_multi`] for documents with several.
pub fn parse_pg_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    parse_pg_explain_json_multi(doc)?
        .into_iter()
        .next()
        .ok_or(PlanError::EmptyPlan)
}

/// Parse every plan in a PostgreSQL JSON EXPLAIN document. Accepts the
/// array `EXPLAIN` prints (one element per statement, more than one when a
/// rule rewrites the query), a lone `{"Plan": ...}` object, and the object
/// `auto_explain` logs (`{"Query Text": ..., "Plan": ...}`).
pub fn parse_pg_explain_json_multi(doc: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let value: Value = serde_json::from_str(doc.trim()).map_err(|e| {
        PlanError::InvalidStructure(format!(
            "EXPLAIN JSON: {e}. Copy the whole value, from the opening [ to the closing ]."
        ))
    })?;
    plans_from_pg_value(&value, "postgresql-json")
}

/// Build plans from an already-decoded PostgreSQL EXPLAIN value (the JSON
/// shape, whichever format it was written in).
pub(super) fn plans_from_pg_value(value: &Value, format: &str) -> PlanResult<Vec<ExecutionPlan>> {
    let entries: Vec<&Value> = match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    };
    let mut plans = Vec::new();
    for entry in entries {
        let Some(entry) = entry.as_object() else {
            continue;
        };
        let Some(root) = field(entry, "Plan").and_then(Value::as_object) else {
            continue;
        };
        let mut builder = PgPlanBuilder::default();
        walk_pg_node(root, None, 0, &mut builder);
        plans.push(builder.finish(entry, format));
    }
    if plans.is_empty() {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN JSON: no \"Plan\" object found. Expected [{\"Plan\": {...}}] as printed by EXPLAIN (FORMAT JSON).".into(),
        ));
    }
    Ok(plans)
}

fn walk_pg_node(
    node: &Map<String, Value>,
    parent: Option<i32>,
    depth: usize,
    builder: &mut PgPlanBuilder,
) {
    let id = builder.push(node, parent);
    if depth >= MAX_DEPTH {
        return;
    }
    if let Some(children) = field(node, "Plans").and_then(Value::as_array) {
        for child in children {
            if let Some(child) = child.as_object() {
                walk_pg_node(child, Some(id), depth + 1, builder);
            }
        }
    }
}

/// Collects nodes in document order, then applies the corrections that need
/// the whole tree.
#[derive(Default)]
pub(super) struct PgPlanBuilder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

impl PgPlanBuilder {
    /// Add one node object (PostgreSQL JSON key names) under `parent`.
    pub(super) fn push(&mut self, node: &Map<String, Value>, parent: Option<i32>) -> i32 {
        let id = self.nodes.len() as i32 + 1;
        self.nodes.push(build_pg_node(node, id));
        if let Some(parent) = parent {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        id
    }

    pub(super) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Finish the plan: `top` holds the statement-level keys (`Planning
    /// Time`, `Execution Time`, `Triggers`, `Settings`, `JIT`, ...).
    pub(super) fn finish(self, top: &Map<String, Value>, format: &str) -> ExecutionPlan {
        let mut plan = ExecutionPlan {
            nodes: self.nodes,
            edges: self.edges,
            dialect: Some(PlanDialect::Postgres),
            source_format: Some(format.to_string()),
            ..Default::default()
        };
        apply_statement_level(top, &mut plan);
        plan.has_actuals =
            plan.execution_time_ms.is_some() || plan.nodes.iter().any(|node| node.loops.is_some());
        plan.statement_type = if plan.has_actuals {
            "EXPLAIN ANALYZE".to_string()
        } else {
            "EXPLAIN".to_string()
        };
        normalize_runtime(&mut plan);
        finish_plan(&mut plan);
        plan
    }
}

/// Lookup that ignores case, spaces and punctuation, so `Node Type`,
/// `Node-Type` (XML) and `node type` are one key.
fn norm(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn field<'a>(object: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    if let Some(value) = object.get(name) {
        return Some(value);
    }
    let wanted = norm(name);
    object
        .iter()
        .find(|(key, _)| norm(key) == wanted)
        .map(|(_, value)| value)
}

/// The fields of one object, remembering which were read so the rest can be
/// kept as generic properties.
struct Fields<'a> {
    items: Vec<(String, &'a str, &'a Value, bool)>,
}

impl<'a> Fields<'a> {
    fn new(object: &'a Map<String, Value>) -> Self {
        Self {
            items: object
                .iter()
                .map(|(key, value)| (norm(key), key.as_str(), value, false))
                .collect(),
        }
    }

    fn take(&mut self, name: &str) -> Option<&'a Value> {
        let wanted = norm(name);
        let item = self.items.iter_mut().find(|item| item.0 == wanted)?;
        item.3 = true;
        Some(item.2)
    }

    fn string(&mut self, name: &str) -> Option<String> {
        scalar_text(self.take(name)?).filter(|text| !text.is_empty())
    }

    fn number(&mut self, name: &str) -> Option<f64> {
        as_number(self.take(name)?)
    }

    fn flag(&mut self, name: &str) -> bool {
        match self.take(name) {
            Some(Value::Bool(value)) => *value,
            Some(Value::String(text)) => text.eq_ignore_ascii_case("true"),
            _ => false,
        }
    }

    fn list(&mut self, name: &str) -> Vec<String> {
        match self.take(name) {
            Some(Value::Array(items)) => items.iter().filter_map(scalar_text).collect(),
            Some(other) => scalar_text(other).into_iter().collect(),
            None => Vec::new(),
        }
    }
}

fn as_number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn as_u32(value: Option<f64>) -> Option<u32> {
    value
        .filter(|n| *n >= 0.0 && *n <= u32::MAX as f64)
        .map(|n| n as u32)
}

/// The operator name as text EXPLAIN prints it: `Parallel Seq Scan`,
/// `Finalize GroupAggregate`, `Hash Left Join`, `Index Scan Backward`,
/// `Update`.
#[allow(clippy::too_many_arguments)]
fn display_name(
    node_type: &str,
    strategy: Option<&str>,
    partial_mode: Option<&str>,
    operation: Option<&str>,
    join_type: Option<&str>,
    command: Option<&str>,
    backward: bool,
    parallel: bool,
) -> String {
    let mut name = match node_type {
        "Aggregate" => {
            let base = match strategy {
                Some("Sorted") => "GroupAggregate",
                Some("Hashed") => "HashAggregate",
                Some("Mixed") => "MixedAggregate",
                _ => "Aggregate",
            };
            match partial_mode {
                Some(mode) if mode != "Simple" => format!("{mode} {base}"),
                _ => base.to_string(),
            }
        }
        "SetOp" => {
            let base = if strategy == Some("Hashed") {
                "HashSetOp"
            } else {
                "SetOp"
            };
            match command {
                Some(command) => format!("{base} {command}"),
                None => base.to_string(),
            }
        }
        "ModifyTable" => operation.unwrap_or("ModifyTable").to_string(),
        "Nested Loop" => match join_type {
            Some(kind) if kind != "Inner" => format!("Nested Loop {kind} Join"),
            _ => "Nested Loop".to_string(),
        },
        "Hash Join" | "Merge Join" => {
            let method = node_type.trim_end_matches(" Join");
            match join_type {
                Some(kind) if kind != "Inner" => format!("{method} {kind} Join"),
                _ => node_type.to_string(),
            }
        }
        other => other.to_string(),
    };
    if backward {
        name.push_str(" Backward");
    }
    if parallel {
        name = format!("Parallel {name}");
    }
    name
}

/// Key the text parser sets when it already knows the printed name.
pub(super) const DISPLAY_KEY: &str = "Display Name";
/// Key the text parser uses for bare lines without a `name: value` shape.
pub(super) const EXTRA_KEY: &str = "Extra Lines";

fn build_pg_node(object: &Map<String, Value>, id: i32) -> PlanNode {
    let mut f = Fields::new(object);
    f.take("Plans");
    let node_type = f
        .string("Node Type")
        .unwrap_or_else(|| "Unknown".to_string());
    let strategy = f.string("Strategy");
    let partial_mode = f.string("Partial Mode");
    let operation = f.string("Operation");
    let join_type = f.string("Join Type");
    let command = f.string("Command");
    let scan_direction = f.string("Scan Direction");
    let parallel = f.flag("Parallel Aware");
    let printed = f.string(DISPLAY_KEY);
    let logical_op = printed.unwrap_or_else(|| {
        display_name(
            &node_type,
            strategy.as_deref(),
            partial_mode.as_deref(),
            operation.as_deref(),
            join_type.as_deref(),
            command.as_deref(),
            scan_direction.as_deref() == Some("Backward"),
            parallel,
        )
    });

    let mut node = PlanNode {
        id,
        logical_op,
        physical_op: node_type.clone(),
        parallel,
        join_type,
        ..Default::default()
    };
    node.relation = f.string("Relation Name");
    node.schema = f.string("Schema");
    node.alias = f
        .string("Alias")
        .filter(|alias| node.relation.as_deref() != Some(alias.as_str()));
    node.index_name = f.string("Index Name");
    node.cte_name = f.string("CTE Name");
    node.parent_relationship = f.string("Parent Relationship");
    node.subplan_name = f.string("Subplan Name");
    if node.cte_name.is_none()
        && let Some(name) = node
            .subplan_name
            .as_deref()
            .and_then(|name| name.strip_prefix("CTE "))
    {
        node.cte_name = Some(name.to_string());
    }

    node.estimated_startup_cost = f.number("Startup Cost");
    node.estimated_cost = f.number("Total Cost").unwrap_or(0.0);
    node.estimated_rows = f.number("Plan Rows").unwrap_or(0.0);
    node.estimated_row_width = f.number("Plan Width");

    let loops = f.number("Actual Loops");
    let rows = f.number("Actual Rows");
    let total_time = f.number("Actual Total Time");
    node.loops = loops;
    node.actual_rows = rows;
    node.actual_startup_ms = f.number("Actual Startup Time");
    let per_loop = |value: Option<f64>| match (value, loops) {
        (Some(value), Some(loops)) => Some(value * loops),
        (value, _) => value,
    };
    node.actual_rows_total = per_loop(rows);
    // Summed over loops here; `normalize_runtime` corrects parallel workers.
    node.actual_time_ms = per_loop(total_time);

    node.filter = f.string("Filter");
    let one_time_filter = f.string("One-Time Filter");
    let index_cond = f.string("Index Cond");
    let recheck_cond = f.string("Recheck Cond");
    let tid_cond = f.string("TID Cond");
    let hash_cond = f.string("Hash Cond");
    let merge_cond = f.string("Merge Cond");
    let join_filter = f.string("Join Filter");
    node.rows_removed_by_filter = per_loop(f.number("Rows Removed by Filter"));
    node.rows_removed_by_join_filter = per_loop(f.number("Rows Removed by Join Filter"));
    node.rows_removed_by_index_recheck = per_loop(f.number("Rows Removed by Index Recheck"));
    // Heap Fetches is already a total; the "Rows Removed" counters are per loop.
    node.heap_fetches = f.number("Heap Fetches");
    node.exact_heap_blocks = f.number("Exact Heap Blocks");
    node.lossy_heap_blocks = f.number("Lossy Heap Blocks");
    if let (Some(rows), Some(removed)) = (node.actual_rows_total, node.rows_removed_by_filter) {
        node.rows_read = Some(rows + removed);
    }

    node.sort_keys = f.list("Sort Key");
    let presorted = f.list("Presorted Key");
    node.group_keys = f.list("Group Key");
    node.output_columns = f.list("Output");
    node.sort_method = f.string("Sort Method");
    node.sort_space_kb = f.number("Sort Space Used");
    node.sort_space_type = f.string("Sort Space Type");

    node.hash_buckets = f.number("Hash Buckets");
    let original_buckets = f.number("Original Hash Buckets");
    node.hash_batches = as_u32(f.number("Hash Batches"));
    node.original_hash_batches = as_u32(f.number("Original Hash Batches"))
        .filter(|original| Some(*original) != node.hash_batches);
    let hashagg_batches = as_u32(f.number("HashAgg Batches"));
    if node.hash_batches.is_none() {
        node.hash_batches = hashagg_batches;
    }
    node.peak_memory_kb = f.number("Peak Memory Usage");
    let disk_usage = f.number("Disk Usage");

    node.workers_planned = as_u32(f.number("Workers Planned"));
    node.workers_launched = as_u32(f.number("Workers Launched"));
    node.buffers = read_buffers(&mut f);

    // Leader did not sort (or did not run): take the method from a worker.
    let mut worker_lines = Vec::new();
    if let Some(Value::Array(workers)) = f.take("Workers") {
        for worker in workers.iter().filter_map(Value::as_object) {
            let mut w = Fields::new(worker);
            let number = w.number("Worker Number").unwrap_or(0.0);
            let w_rows = w.number("Actual Rows");
            let w_loops = w.number("Actual Loops");
            let w_time = w.number("Actual Total Time");
            let w_sort = w.string("Sort Method");
            let w_space = w.number("Sort Space Used");
            let w_space_type = w.string("Sort Space Type");
            let summary = w.string("Summary");
            if let (Some(rows), Some(loops)) = (w_rows, w_loops) {
                node.thread_rows.push(rows * loops);
            }
            if node.sort_method.is_none() && w_sort.is_some() {
                node.sort_method = w_sort.clone();
                node.sort_space_kb = w_space;
                node.sort_space_type = w_space_type.clone();
            } else if w_space_type.as_deref() == Some("Disk")
                && node.sort_space_type.as_deref() != Some("Disk")
            {
                // Any process that sorted on disk is what matters.
                node.sort_method = w_sort.clone();
                node.sort_space_kb = w_space;
                node.sort_space_type = w_space_type.clone();
            }
            let text = summary.unwrap_or_else(|| {
                let mut parts = Vec::new();
                if let Some(time) = w_time {
                    parts.push(format!("time={time} ms"));
                }
                if let Some(rows) = w_rows {
                    parts.push(format!("rows={rows}"));
                }
                if let Some(loops) = w_loops {
                    parts.push(format!("loops={loops}"));
                }
                if let Some(method) = &w_sort {
                    let space = match (w_space, &w_space_type) {
                        (Some(kb), Some(kind)) => format!("  {kind}: {kb}kB"),
                        _ => String::new(),
                    };
                    parts.push(format!("Sort Method: {method}{space}"));
                }
                parts.join(", ")
            });
            worker_lines.push(PlanProperty {
                name: format!("Worker {number}"),
                value: text,
            });
        }
    }

    if node.sort_space_type.as_deref() == Some("Disk") {
        node.spill = Some(PlanSpill {
            kind: "sort".to_string(),
            written_kb: node.sort_space_kb,
            ..Default::default()
        });
    } else if disk_usage.is_some_and(|kb| kb > 0.0) {
        node.spill = Some(PlanSpill {
            kind: "hash".to_string(),
            written_kb: disk_usage,
            ..Default::default()
        });
    } else if node.hash_batches.is_some_and(|batches| batches > 1) {
        // More than one batch means the hash table did not fit in work_mem
        // and the extra batches went to temporary files.
        node.spill = Some(PlanSpill {
            kind: "hash".to_string(),
            ..Default::default()
        });
    }

    // What existing consumers read from `output_list`, in the old order.
    if let Some(relation) = &node.relation {
        node.output_list.push(relation.clone());
    }
    if let Some(filter) = &node.filter {
        node.output_list.push(format!("Filter: {filter}"));
    }
    if let Some(cond) = &index_cond {
        node.output_list.push(format!("Index Cond: {cond}"));
    }
    if let Some(index) = &node.index_name {
        node.output_list.push(format!("Index: {index}"));
    }
    for (label, value) in [
        ("Recheck Cond", &recheck_cond),
        ("TID Cond", &tid_cond),
        ("Hash Cond", &hash_cond),
        ("Merge Cond", &merge_cond),
        ("Join Filter", &join_filter),
        ("One-Time Filter", &one_time_filter),
    ] {
        if let Some(value) = value {
            node.output_list.push(format!("{label}: {value}"));
        }
    }

    node.index_cond = index_cond.or(tid_cond).or_else(|| recheck_cond.clone());
    let keyed_join = hash_cond.or(merge_cond);
    if keyed_join.is_some() {
        if let Some(join_filter) = join_filter {
            node.properties.push(PlanProperty {
                name: "Join Filter".to_string(),
                value: join_filter,
            });
        }
        node.join_predicate = keyed_join;
    } else {
        node.join_predicate = join_filter;
    }
    if let Some(filter) = one_time_filter {
        if node.filter.is_none() {
            node.filter = Some(filter.clone());
        }
        node.properties.push(PlanProperty {
            name: "One-Time Filter".to_string(),
            value: filter,
        });
    }
    if node.index_cond.is_some()
        && let Some(recheck) = recheck_cond
        && node.index_cond.as_deref() != Some(recheck.as_str())
    {
        node.properties.push(PlanProperty {
            name: "Recheck Cond".to_string(),
            value: recheck,
        });
    }
    if !presorted.is_empty() {
        node.properties.push(PlanProperty {
            name: "Presorted Key".to_string(),
            value: presorted.join(", "),
        });
    }
    if let Some(original) = original_buckets
        && Some(original) != node.hash_buckets
    {
        node.properties.push(PlanProperty {
            name: "Original Hash Buckets".to_string(),
            value: original.to_string(),
        });
    }
    for (name, value) in [
        ("Strategy", &strategy),
        ("Partial Mode", &partial_mode),
        ("Operation", &operation),
        ("Command", &command),
        ("Scan Direction", &scan_direction),
    ] {
        if let Some(value) = value
            && value != "Simple"
            && value != "Plain"
            && value != "Forward"
        {
            node.properties.push(PlanProperty {
                name: name.to_string(),
                value: value.clone(),
            });
        }
    }

    if let Some(removed) = f.number("Subplans Removed") {
        node.properties.push(PlanProperty {
            name: "Subplans Removed".to_string(),
            value: removed.to_string(),
        });
        // Children are counted in `normalize_runtime`, once the tree exists.
        node.partitions_total = as_u32(Some(removed));
    }
    if let Some(Value::Array(lines)) = f.take(EXTRA_KEY) {
        node.extra = lines.iter().filter_map(scalar_text).collect();
    }

    // Everything the document said that has no typed field.
    for (_, key, value, used) in &f.items {
        if *used {
            continue;
        }
        let text = match value {
            Value::Bool(false) | Value::Null => continue,
            Value::Array(items) => items
                .iter()
                .filter_map(scalar_text)
                .collect::<Vec<_>>()
                .join(", "),
            Value::Object(map) => map
                .iter()
                .filter_map(|(k, v)| scalar_text(v).map(|v| format!("{k}: {v}")))
                .collect::<Vec<_>>()
                .join(", "),
            other => scalar_text(other).unwrap_or_default(),
        };
        if text.is_empty() {
            continue;
        }
        node.properties.push(PlanProperty {
            name: key.replace('-', " "),
            value: text,
        });
    }
    node.properties.extend(worker_lines);

    if node_type == "Seq Scan"
        && let Some(table) = &node.relation
    {
        node.warnings.push(PlanWarning::TableScan {
            table: table.clone(),
            schema: node.schema.clone().unwrap_or_default(),
        });
    }
    node
}

fn read_buffers(f: &mut Fields) -> Option<PlanBuffers> {
    let mut buffers = PlanBuffers {
        shared_hit: f.number("Shared Hit Blocks"),
        shared_read: f.number("Shared Read Blocks"),
        shared_dirtied: f.number("Shared Dirtied Blocks"),
        shared_written: f.number("Shared Written Blocks"),
        local_hit: f.number("Local Hit Blocks"),
        local_read: f.number("Local Read Blocks"),
        local_dirtied: f.number("Local Dirtied Blocks"),
        local_written: f.number("Local Written Blocks"),
        temp_read: f.number("Temp Read Blocks"),
        temp_written: f.number("Temp Written Blocks"),
        ..Default::default()
    };
    let mut read_ms = None;
    let mut write_ms = None;
    let add = |slot: &mut Option<f64>, value: Option<f64>| {
        if let Some(value) = value {
            *slot = Some(slot.unwrap_or(0.0) + value);
        }
    };
    // PostgreSQL 16 and older: one pair. 17 and newer: shared / local / temp.
    for name in [
        "I/O Read Time",
        "Shared I/O Read Time",
        "Local I/O Read Time",
        "Temp I/O Read Time",
    ] {
        add(&mut read_ms, f.number(name));
    }
    for name in [
        "I/O Write Time",
        "Shared I/O Write Time",
        "Local I/O Write Time",
        "Temp I/O Write Time",
    ] {
        add(&mut write_ms, f.number(name));
    }
    buffers.io_read_ms = read_ms;
    buffers.io_write_ms = write_ms;
    if buffers == PlanBuffers::default() {
        return None;
    }
    if buffers.shared_hit.is_some() || buffers.shared_read.is_some() {
        buffers.logical_reads =
            Some(buffers.shared_hit.unwrap_or(0.0) + buffers.shared_read.unwrap_or(0.0));
        buffers.physical_reads = Some(buffers.shared_read.unwrap_or(0.0));
    }
    if let Some(written) = buffers.shared_written {
        buffers.writes = Some(written);
    }
    Some(buffers)
}

fn buffers_summary(buffers: &PlanBuffers) -> String {
    let mut groups = Vec::new();
    for (label, parts) in [
        (
            "shared",
            [
                ("hit", buffers.shared_hit),
                ("read", buffers.shared_read),
                ("dirtied", buffers.shared_dirtied),
                ("written", buffers.shared_written),
            ],
        ),
        (
            "local",
            [
                ("hit", buffers.local_hit),
                ("read", buffers.local_read),
                ("dirtied", buffers.local_dirtied),
                ("written", buffers.local_written),
            ],
        ),
        (
            "temp",
            [
                ("hit", None),
                ("read", buffers.temp_read),
                ("dirtied", None),
                ("written", buffers.temp_written),
            ],
        ),
    ] {
        let shown: Vec<String> = parts
            .iter()
            .filter_map(|(name, value)| value.filter(|v| *v > 0.0).map(|v| format!("{name}={v}")))
            .collect();
        if !shown.is_empty() {
            groups.push(format!("{label} {}", shown.join(" ")));
        }
    }
    groups.join(", ")
}

/// Statement-level keys: timings, triggers, settings, JIT.
fn apply_statement_level(top: &Map<String, Value>, plan: &mut ExecutionPlan) {
    let mut f = Fields::new(top);
    f.take("Plan");
    plan.planning_time_ms = f.number("Planning Time");
    // `Total Runtime` is what 9.3 and older call the execution time.
    plan.execution_time_ms = f.number("Execution Time").or(f.number("Total Runtime"));
    if let Some(text) = f.string("Query Text") {
        plan.statement_text = text;
    }
    plan.query_hash = f.string("Query Identifier");
    if let Some(Value::Object(settings)) = f.take("Settings") {
        for (name, value) in settings {
            if let Some(value) = scalar_text(value) {
                plan.settings.push(PlanProperty {
                    name: name.clone(),
                    value,
                });
            }
        }
    }
    if let Some(Value::Array(triggers)) = f.take("Triggers") {
        for trigger in triggers.iter().filter_map(Value::as_object) {
            let mut t = Fields::new(trigger);
            let name = t
                .string("Trigger Name")
                .or_else(|| t.string("Constraint Name"))
                .unwrap_or_default();
            plan.triggers.push(PlanTrigger {
                name,
                relation: t.string("Relation"),
                time_ms: t.number("Time"),
                calls: t.number("Calls"),
            });
        }
    }
    if let Some(Value::Object(jit)) = f.take("JIT") {
        let mut j = Fields::new(jit);
        plan.jit_functions = as_u32(j.number("Functions"));
        plan.jit_time_ms = match j.take("Timing") {
            Some(Value::Object(timing)) => field(timing, "Total").and_then(as_number),
            Some(other) => as_number(other),
            None => None,
        };
    }
    if let Some(Value::Object(planning)) = f.take("Planning") {
        let mut p = Fields::new(planning);
        if let Some(buffers) = read_buffers(&mut p) {
            let summary = buffers_summary(&buffers);
            if !summary.is_empty() {
                plan.notes.push(format!("Planning buffers: {summary}"));
            }
        }
        for name in ["Memory Used", "Memory Allocated"] {
            if let Some(kb) = p.number(name) {
                plan.notes
                    .push(format!("Planning {}: {kb} kB", name.to_lowercase()));
            }
        }
    }
    if let Some(Value::Object(serialization)) = f.take("Serialization") {
        let mut s = Fields::new(serialization);
        let mut parts = Vec::new();
        if let Some(time) = s.number("Time") {
            parts.push(format!("time={time} ms"));
        }
        if let Some(output) = s.number("Output Volume") {
            parts.push(format!("output={output} kB"));
        }
        if let Some(format) = s.string("Format") {
            parts.push(format!("format={format}"));
        }
        if !parts.is_empty() {
            plan.notes
                .push(format!("Serialization: {}", parts.join(", ")));
        }
    }
    if let Some(Value::Array(notes)) = f.take("Notes") {
        plan.notes.extend(notes.iter().filter_map(scalar_text));
    }
    for (_, key, value, used) in &f.items {
        if *used {
            continue;
        }
        if let Some(text) = scalar_text(value).filter(|text| !text.is_empty()) {
            plan.notes.push(format!("{key}: {text}"));
        }
    }
}

/// Corrections that need the whole tree: which nodes never ran, wall-clock
/// time for nodes run by parallel workers, self times, pruned partitions.
fn normalize_runtime(plan: &mut ExecutionPlan) {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for edge in &plan.edges {
        children
            .entry(edge.from_node)
            .or_default()
            .push(edge.to_node);
    }
    let index: HashMap<i32, usize> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id, position))
        .collect();

    // Processes sharing the work below each Gather: launched workers plus
    // the leader. EXPLAIN reports those nodes' times as a per-loop average
    // with one loop per process, so time x loops would count every process's
    // clock. Divide by the processes to get back to wall-clock time.
    let mut stack: Vec<(i32, f64)> = plan
        .nodes
        .first()
        .map(|n| (n.id, 1.0))
        .into_iter()
        .collect();
    let mut visited = 0usize;
    while let Some((id, processes)) = stack.pop() {
        visited += 1;
        if visited > plan.nodes.len() {
            break;
        }
        let Some(&position) = index.get(&id) else {
            continue;
        };
        let node = &mut plan.nodes[position];
        if plan.has_actuals && node.loops == Some(0.0) {
            node.never_executed = true;
            node.actual_time_ms = None;
            node.actual_startup_ms = None;
        }
        if processes > 1.0
            && let (Some(time), Some(loops)) = (node.actual_time_ms, node.loops)
            && loops > 1.0
        {
            node.actual_time_ms = Some(time / processes.min(loops));
        }
        let below = if matches!(node.physical_op.as_str(), "Gather" | "Gather Merge") {
            match node.workers_launched.or(node.workers_planned) {
                Some(workers) if plan.has_actuals => f64::from(workers) + 1.0,
                _ => 1.0,
            }
        } else {
            processes
        };
        if let Some(removed) = node.partitions_total.take() {
            let kept = children.get(&id).map_or(0, Vec::len) as u32;
            node.partitions_scanned = Some(kept);
            node.partitions_total = Some(kept.saturating_add(removed));
        }
        for child in children.get(&id).into_iter().flatten().rev() {
            stack.push((*child, below));
        }
    }

    // Self time. A CTE is planned as an InitPlan of the node that declares
    // it, but its time is spent inside the CTE Scan that reads it.
    let cte_time: HashMap<String, f64> = plan
        .nodes
        .iter()
        .filter_map(|node| {
            let name = node.subplan_name.as_deref()?.strip_prefix("CTE ")?;
            Some((name.to_string(), node.actual_time_ms?))
        })
        .collect();
    let inclusive: HashMap<i32, (Option<f64>, bool)> = plan
        .nodes
        .iter()
        .map(|node| {
            let is_cte = node
                .subplan_name
                .as_deref()
                .is_some_and(|name| name.starts_with("CTE "));
            (node.id, (node.actual_time_ms, is_cte))
        })
        .collect();
    for node in &mut plan.nodes {
        let Some(own) = node.actual_time_ms else {
            continue;
        };
        let mut inputs: f64 = children
            .get(&node.id)
            .into_iter()
            .flatten()
            .filter_map(|child| inclusive.get(child))
            .filter(|(_, is_cte)| !is_cte)
            .filter_map(|(time, _)| *time)
            .sum();
        if node.physical_op == "CTE Scan"
            && let Some(time) = node.cte_name.as_ref().and_then(|name| cte_time.get(name))
        {
            inputs += time;
        }
        node.actual_self_time_ms = Some((own - inputs).max(0.0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::dialects::{PlanDialect, parse_captured_plan};
    use crate::plans::findings_for_plan;

    #[test]
    fn pg_estimated_plan_without_actual_fields_parses() {
        // EXPLAIN without ANALYZE: no Actual Rows / Actual Total Time.
        let doc = r#"[{"Plan": {"Node Type": "Seq Scan", "Relation Name": "t", "Total Cost": 12.5, "Plan Rows": 100}}]"#;
        let plans = parse_captured_plan(PlanDialect::Postgres, doc).expect("parses");
        assert_eq!(plans.len(), 1);
        assert!(plans[0].nodes[0].actual_rows.is_none());
    }

    /// Real PostgreSQL 16 output for `EXPLAIN (ANALYZE, FORMAT JSON)
    /// SELECT * FROM plan_demo WHERE name = 'x'`.
    const PG_SEQ_SCAN: &str = r#"[
  {
    "Plan": {
      "Node Type": "Seq Scan",
      "Parallel Aware": false,
      "Async Capable": false,
      "Relation Name": "plan_demo",
      "Alias": "plan_demo",
      "Startup Cost": 0.00,
      "Total Cost": 25.88,
      "Plan Rows": 6,
      "Plan Width": 36,
      "Actual Startup Time": 0.001,
      "Actual Total Time": 0.001,
      "Actual Rows": 0,
      "Actual Loops": 1,
      "Filter": "(name = 'x'::text)",
      "Rows Removed by Filter": 0
    },
    "Planning Time": 0.269,
    "Triggers": [
    ],
    "Execution Time": 0.012
  }
]"#;

    #[test]
    fn pg_seq_scan_parses_and_is_too_small_for_a_finding() {
        let plan = parse_pg_explain_json(PG_SEQ_SCAN).unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].physical_op, "Seq Scan");
        assert_eq!(plan.total_subtree_cost, 25.88);
        assert_eq!(plan.nodes[0].actual_rows, Some(0.0));
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn pg_nested_plan_builds_edges() {
        let doc = r#"[{"Plan": {"Node Type": "Hash Join", "Total Cost": 100.0, "Plan Rows": 10,
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "a", "Total Cost": 30.0, "Plan Rows": 5},
              {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pk", "Total Cost": 40.0, "Plan Rows": 5}
            ]}}]"#;
        let plan = parse_pg_explain_json(doc).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.edges.len(), 2);
        assert!(plan.edges.iter().all(|e| e.from_node == 1));
        // Only the Seq Scan warns.
        assert_eq!(
            plan.nodes.iter().filter(|n| !n.warnings.is_empty()).count(),
            1
        );
    }

    #[test]
    fn malformed_pg_documents_yield_typed_errors_not_panics() {
        assert!(parse_pg_explain_json("not json").is_err());
        assert!(parse_pg_explain_json("{}").is_err());
    }
}
