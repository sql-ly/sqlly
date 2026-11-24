//! MySQL `EXPLAIN FORMAT=JSON` with `explain_json_format_version = 2`
//! (MySQL 8.3 and later), and `EXPLAIN ANALYZE FORMAT=JSON` (8.4 and later),
//! which always uses this shape.
//!
//! Version 2 is the iterator tree that `FORMAT=TREE` prints, as JSON: every
//! node has an `operation` string (the same text as the TREE line), an
//! `access_type` naming the kind of iterator, estimates, and its inputs in
//! `inputs`. Subqueries in the select list hang in
//! `inputs_from_select_list`.
//!
//! Two wrappers exist. MySQL 8.4 puts the statement's `query` and
//! `query_type` on the root node itself. MySQL 9.x moves the root node into
//! `query_plan` and adds `json_schema_version`.

use super::common;
use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;
use serde_json::{Map, Value};

/// Deepest nesting accepted. serde_json already refuses documents nested
/// deeper than 128 levels; this is the parser's own guard.
const MAX_DEPTH: usize = 512;
/// Nodes read from one document.
const MAX_NODES: usize = 50_000;

/// What MySQL prints as the whole plan for a statement the iterator executor
/// does not run (single-table `UPDATE` / `DELETE`, for example).
const NOT_EXECUTABLE: &str = "<not executable by iterator executor>";

/// Keys read into typed fields, or wrapper keys of the 8.4 root, so they are
/// not repeated in `properties`.
const CONSUMED: &[&str] = &[
    "operation",
    "inputs",
    "estimated_rows",
    "estimated_total_cost",
    "estimated_first_row_cost",
    "actual_rows",
    "actual_loops",
    "actual_first_row_ms",
    "actual_last_row_ms",
    "table_name",
    "schema_name",
    "alias",
    "index_name",
    "lookup_condition",
    "ranges",
    "condition",
    "hash_condition",
    "join_type",
    "sort_fields",
    "used_columns",
    "query",
    "query_type",
    "json_schema_version",
];

/// Parse MySQL `EXPLAIN FORMAT=JSON` version 2 output: the 8.4 shape (the
/// root object is the root plan node plus `query` and `query_type`) and the
/// 9.x shape (root node under `query_plan`, with `json_schema_version`).
///
/// `actual_rows` is the average per loop, as the document reports it;
/// `actual_rows_total` is that times `actual_loops`. `actual_last_row_ms` is
/// per loop too, so `actual_time_ms` is it times the loop count. A node with
/// `"actual_rows": null` in an analyzed plan never ran.
pub fn parse_mysql_explain_json_v2(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("EXPLAIN FORMAT=JSON (v2): {e}")))?;
    let top = value.as_object().ok_or_else(|| {
        PlanError::InvalidStructure(
            "EXPLAIN FORMAT=JSON (v2): the document is not an object".into(),
        )
    })?;
    let root = match top.get("query_plan") {
        Some(Value::Object(plan)) => plan,
        _ => top,
    };
    let operation = root.get("operation").and_then(Value::as_str);
    if operation.is_none() && !root.contains_key("access_type") {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN FORMAT=JSON (v2): no `operation` on the root node".into(),
        ));
    }
    let query_type = top
        .get("query_type")
        .or_else(|| root.get("query_type"))
        .and_then(Value::as_str);
    if operation == Some(NOT_EXECUTABLE) && !root.contains_key("inputs") {
        return Err(PlanError::Other(format!(
            "MySQL printed no plan tree for this {} statement: version 2 of FORMAT=JSON (and FORMAT=TREE) only describes statements the iterator executor runs. Use EXPLAIN FORMAT=TRADITIONAL, or SET explain_json_format_version = 1 and run EXPLAIN FORMAT=JSON again.",
            query_type.unwrap_or("data-change")
        )));
    }

    let mut nodes: Vec<PlanNode> = Vec::new();
    let mut edges: Vec<PlanEdge> = Vec::new();
    walk(root, 0, 0, &mut nodes, &mut edges)?;

    let analyzed = nodes
        .iter()
        .any(|n| n.actual_rows.is_some() || n.actual_time_ms.is_some());
    if !analyzed {
        // `"actual_rows": null` only means "never executed" next to nodes
        // that did run.
        for node in &mut nodes {
            node.never_executed = false;
        }
    }
    let mut settings = Vec::new();
    if let Some(query_type) = query_type {
        settings.push(property("query_type", query_type));
    }
    if let Some(version) = top.get("json_schema_version").and_then(scalar_text) {
        settings.push(property("json_schema_version", &version));
    }
    let mut plan = ExecutionPlan {
        statement_text: top
            .get("query")
            .or_else(|| root.get("query"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        statement_type: if analyzed {
            "EXPLAIN ANALYZE"
        } else {
            "EXPLAIN"
        }
        .to_string(),
        nodes,
        edges,
        dialect: Some(PlanDialect::MySql),
        source_format: Some("mysql-json-v2".to_string()),
        has_actuals: analyzed,
        settings,
        ..Default::default()
    };
    // The root's last-row time is the statement's execution time.
    if analyzed {
        plan.execution_time_ms = plan.nodes.first().and_then(|n| n.actual_time_ms);
    }
    common::finish_plan(&mut plan);
    Ok(plan)
}

fn property(name: &str, value: &str) -> PlanProperty {
    PlanProperty {
        name: name.to_string(),
        value: value.to_string(),
    }
}

fn finite(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => common::parse_count(s),
        _ => None,
    }
    .filter(|n| n.is_finite())
}

fn number(map: &Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(finite)
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    map.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Strings of an array (other scalars as their JSON text).
fn string_list(map: &Map<String, Value>, key: &str) -> Vec<String> {
    match map.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(scalar_text).collect(),
        Some(other) => scalar_text(other).into_iter().collect(),
        None => Vec::new(),
    }
}

/// A property value for anything: scalars as text, arrays of scalars joined
/// with `, `, everything else as compact JSON.
fn property_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Array(items) if items.iter().all(|i| !i.is_object() && !i.is_array()) => Some(
            items
                .iter()
                .filter_map(scalar_text)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        Value::Array(_) | Value::Object(_) => Some(value.to_string()),
        scalar => scalar_text(scalar),
    }
}

/// `inner join` -> `Inner`, `left join` -> `Left`, `semijoin` -> `Semi`,
/// `antijoin` -> `Anti`, `full outer join` -> `Full`.
fn join_type_name(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    let name = if lower.contains("anti") {
        "Anti"
    } else if lower.contains("semi") {
        "Semi"
    } else if lower.contains("full") {
        "Full"
    } else if lower.contains("left") {
        "Left"
    } else if lower.contains("right") {
        "Right"
    } else if lower.contains("inner") {
        "Inner"
    } else {
        return raw.to_string();
    };
    name.to_string()
}

/// Outer parentheses of `(a = b)` removed when they wrap the whole text.
fn unwrap_parens(text: &str) -> &str {
    let t = text.trim();
    let Some(inner) = t.strip_prefix('(').and_then(|r| r.strip_suffix(')')) else {
        return t;
    };
    let mut depth = 0i32;
    for c in inner.chars() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth < 0 {
                    return t;
                }
            }
            _ => {}
        }
    }
    if depth == 0 { inner.trim() } else { t }
}

fn walk(
    map: &Map<String, Value>,
    parent: i32,
    depth: usize,
    nodes: &mut Vec<PlanNode>,
    edges: &mut Vec<PlanEdge>,
) -> PlanResult<i32> {
    if depth > MAX_DEPTH {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN FORMAT=JSON (v2): the plan is nested deeper than 512 levels".into(),
        ));
    }
    if nodes.len() >= MAX_NODES {
        return Err(PlanError::InvalidStructure(
            "EXPLAIN FORMAT=JSON (v2): the plan has more than 50000 nodes".into(),
        ));
    }
    let id = nodes.len() as i32 + 1;
    let access_type = text(map, "access_type");
    let operation = text(map, "operation")
        .or(text(map, "heading"))
        .or(access_type)
        .unwrap_or("?")
        .trim()
        .to_string();

    let mut node = PlanNode {
        id,
        logical_op: operation.clone(),
        physical_op: operation,
        estimated_rows: number(map, "estimated_rows").unwrap_or(0.0),
        estimated_cost: number(map, "estimated_total_cost").unwrap_or(0.0),
        estimated_startup_cost: number(map, "estimated_first_row_cost"),
        ..Default::default()
    };

    // Runtime side.
    let loops = number(map, "actual_loops");
    let times = loops.unwrap_or(1.0);
    if let Some(rows) = number(map, "actual_rows") {
        node.actual_rows = Some(rows);
        node.actual_rows_total = Some(rows * times);
    } else if matches!(map.get("actual_rows"), Some(Value::Null)) {
        node.never_executed = true;
    }
    node.loops = loops;
    node.actual_startup_ms = number(map, "actual_first_row_ms");
    if let Some(last_row_ms) = number(map, "actual_last_row_ms") {
        node.actual_time_ms = Some(last_row_ms * times);
        node.output_list
            .push(format!("Actual time: {last_row_ms} ms"));
    }

    // What it reads.
    let table = text(map, "table_name");
    let internal = table.is_some_and(|t| t.starts_with('<'));
    if let Some(table) = table {
        node.output_list.push(table.to_string());
        if !internal {
            node.relation = Some(table.to_string());
            // Format 2 prints the table's own name here and the query's
            // alias in a separate `alias` member.
            node.relation_resolved = true;
        }
    }
    node.schema = text(map, "schema_name").map(str::to_string);
    node.alias = text(map, "alias")
        .filter(|alias| Some(*alias) != table)
        .map(str::to_string);
    if let Some(index) = text(map, "index_name") {
        node.index_name = Some(index.to_string());
        node.output_list.push(format!("Key: {index}"));
    }
    let ranges = string_list(map, "ranges");
    node.index_cond = text(map, "lookup_condition")
        .map(str::to_string)
        .or_else(|| (!ranges.is_empty()).then(|| ranges.join(" OR ")));
    if let Some(condition) = text(map, "condition") {
        node.filter = Some(unwrap_parens(condition).to_string());
        node.output_list.push(format!("Condition: {condition}"));
    }
    if let Some(join_type) = text(map, "join_type") {
        node.join_type = Some(join_type_name(join_type));
    }
    let hash_condition = string_list(map, "hash_condition");
    if !hash_condition.is_empty() {
        node.join_predicate = Some(
            hash_condition
                .iter()
                .map(|c| unwrap_parens(c))
                .collect::<Vec<_>>()
                .join(" AND "),
        );
    }
    node.sort_keys = string_list(map, "sort_fields");
    node.output_columns = string_list(map, "used_columns");
    if let Some(heading) = text(map, "heading")
        && heading.starts_with("Select #")
    {
        node.subplan_name = Some(heading.to_string());
    }
    if matches!(map.get("covering"), Some(Value::Bool(true))) {
        node.extra.push("Using index".to_string());
    }
    if matches!(map.get("reverse"), Some(Value::Bool(true))) {
        node.extra.push("Backward index scan".to_string());
    }
    // A scan of the whole table. `<temporary>` and friends are the server's
    // own work tables, which nobody can index.
    if access_type == Some("table")
        && !internal
        && let Some(table) = table
    {
        node.warnings.push(PlanWarning::TableScan {
            table: text(map, "alias").unwrap_or(table).to_string(),
            schema: node.schema.clone().unwrap_or_default(),
        });
    }

    for (key, value) in map {
        if CONSUMED.contains(&key.as_str()) || key.starts_with("inputs") {
            continue;
        }
        if let Some(value) = property_text(value) {
            node.properties.push(property(key, &value));
        }
    }

    nodes.push(node);
    if parent > 0 {
        edges.push(PlanEdge {
            from_node: parent,
            to_node: id,
        });
    }

    // `inputs` first, then the other input lists (`inputs_from_select_list`:
    // subqueries in the projection) in key order.
    let other_lists = map
        .iter()
        .filter(|(key, _)| key.starts_with("inputs") && key.as_str() != "inputs")
        .map(|(_, value)| value);
    for list in map.get("inputs").into_iter().chain(other_lists) {
        let items: &[Value] = match list {
            Value::Array(items) => items,
            single @ Value::Object(_) => std::slice::from_ref(single),
            _ => &[],
        };
        for item in items {
            if let Value::Object(child) = item {
                walk(child, id, depth + 1, nodes, edges)?;
            }
        }
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOIN_84: &str = include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-v2.json");
    const RANGE_ANALYZE_84: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-q2-range-analyze-v2.json");
    const HASH_84: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql84-hash-join-v2.json");
    const SUBQUERIES_97: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql97-q3-subqueries-v2.json");
    const JOIN_ANALYZE_97: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql97-q1-join-analyze-v2.json");
    const NEVER_97: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql97-never-executed-analyze-v2.json");
    const DELETE_97: &str =
        include_str!("../../../tests/golden/plans/mysql/mysql97-q7-delete-v2.json");

    fn edges(plan: &ExecutionPlan) -> Vec<(i32, i32)> {
        plan.edges
            .iter()
            .map(|e| (e.from_node, e.to_node))
            .collect()
    }

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() <= b.abs() * 1e-9 + 1e-12)
    }

    #[test]
    fn mysql84_hash_join_shape_with_the_root_node_at_top_level() {
        let plan = parse_mysql_explain_json_v2(HASH_84).unwrap();
        assert_eq!(plan.dialect, Some(PlanDialect::MySql));
        assert_eq!(plan.source_format.as_deref(), Some("mysql-json-v2"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert!(
            plan.statement_text
                .starts_with("/* select#1 */ select count(0)")
        );
        let names: Vec<&str> = plan.nodes.iter().map(|n| n.physical_op.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Aggregate: count(0)",
                "Inner hash join (c.`name` = p.`name`)",
                "Table scan on c",
                "Filter: (p.price > 100.00)",
                "Table scan on p",
            ]
        );
        assert_eq!(edges(&plan), vec![(1, 2), (2, 3), (2, 4), (4, 5)]);
        assert!(close(Some(plan.total_subtree_cost), 364861.3829551784));
        assert_eq!(plan.estimated_rows, 1.0);

        let root = &plan.nodes[0];
        assert_eq!(root.property("functions"), Some("count(0)"));
        assert_eq!(root.property("access_type"), Some("aggregate"));
        assert_eq!(root.property("query"), None);
        assert_eq!(root.property("query_type"), None);

        let join = &plan.nodes[1];
        assert_eq!(join.join_type.as_deref(), Some("Inner"));
        assert_eq!(join.join_predicate.as_deref(), Some("c.`name` = p.`name`"));
        assert_eq!(join.property("join_algorithm"), Some("hash"));
        assert!(close(Some(join.estimated_rows), 331500.1794382956));

        let c = &plan.nodes[2];
        assert_eq!(c.relation.as_deref(), Some("customers"));
        assert_eq!(c.schema.as_deref(), Some("shop"));
        assert_eq!(c.alias.as_deref(), Some("c"));
        assert_eq!(c.output_columns, vec!["name"]);
        assert_eq!(c.estimated_rows, 4973.0);
        assert!(matches!(
            c.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "c" && schema == "shop"
        ));

        let filter = &plan.nodes[3];
        assert_eq!(filter.filter.as_deref(), Some("p.price > 100.00"));
        assert_eq!(filter.property("heading"), Some("Hash"));
        assert!(filter.warnings.is_empty());
        assert_eq!(plan.nodes[4].estimated_cost, 202.25);
    }

    #[test]
    fn mysql84_plain_join_parses() {
        let plan = parse_mysql_explain_json_v2(JOIN_84).unwrap();
        assert!(!plan.has_actuals);
        assert!(plan.nodes.len() >= 5, "{}", plan.nodes.len());
        assert_eq!(plan.edges.len(), plan.nodes.len() - 1);
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
        assert!(
            plan.nodes
                .iter()
                .any(|n| n.sort_keys == vec!["revenue DESC"]),
            "sort_fields become sort_keys"
        );
        assert!(
            plan.nodes
                .iter()
                .any(|n| n.index_name.as_deref() == Some("order_items_order_idx"))
        );
    }

    #[test]
    fn mysql84_analyze_fills_rows_loops_and_times() {
        let plan = parse_mysql_explain_json_v2(RANGE_ANALYZE_84).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        assert_eq!(plan.nodes.len(), 5);
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "Nested loop inner join");
        assert_eq!(root.actual_rows, Some(149.0));
        assert_eq!(root.loops, Some(1.0));
        assert_eq!(root.actual_rows_total, Some(149.0));
        assert!(close(root.actual_time_ms, 0.7391709999999999));
        assert!(close(plan.execution_time_ms, 0.7391709999999999));
        assert!(close(Some(root.estimated_rows), 110.07670455062271));
        assert!(close(Some(root.estimated_cost), 124.06258732750302));
        assert!(root.actual_startup_ms.is_some());
        assert!(root.actual_self_time_ms.is_some());

        let range = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.starts_with("Index range scan on c"))
            .unwrap();
        assert_eq!(
            range.physical_op,
            "Index range scan on c using PRIMARY over (100 <= id <= 140)"
        );
        assert_eq!(range.index_name.as_deref(), Some("PRIMARY"));
        assert_eq!(range.index_cond.as_deref(), Some("(100 <= id <= 140)"));
        assert!(range.warnings.is_empty());

        let lookup = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.starts_with("Index lookup on o"))
            .unwrap();
        assert_eq!(lookup.actual_rows, Some(8.0));
        assert_eq!(lookup.loops, Some(41.0));
        assert_eq!(lookup.actual_rows_total, Some(328.0));
        assert_eq!(lookup.relation.as_deref(), Some("orders"));
        assert_eq!(lookup.alias.as_deref(), Some("o"));

        let filter = plan
            .nodes
            .iter()
            .find(|n| n.physical_op.starts_with("Filter: (o.total"))
            .unwrap();
        assert!(close(filter.actual_rows, 3.6341463414634148));
        assert!(close(filter.actual_rows_total, 149.0));
        assert_eq!(filter.filter.as_deref(), Some("o.total > 500.00"));
    }

    #[test]
    fn mysql9_query_plan_wrapper_and_select_list_subquery() {
        let plan = parse_mysql_explain_json_v2(SUBQUERIES_97).unwrap();
        assert!(plan.statement_text.contains("select max(`o`.`total`)"));
        assert_eq!(
            plan.settings,
            vec![
                property("query_type", "select"),
                property("json_schema_version", "2.0")
            ]
        );
        let names: Vec<&str> = plan.nodes.iter().map(|n| n.physical_op.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Limit: 20 row(s)",
                "Nested loop inner join",
                "Sort: c.`name`",
                "Filter: (c.id is not null)",
                "Table scan on c",
                "Single-row index lookup on <subquery3> using <auto_distinct_key> (customer_id = c.id)",
                "Materialize with deduplication",
                "Filter: (orders.total > 900.00)",
                "Table scan on orders",
                "Aggregate: max(o.total)",
                "Index lookup on o using orders_customer_idx (customer_id = c.id)",
            ]
        );
        assert_eq!(
            edges(&plan),
            vec![
                (1, 2),
                (2, 3),
                (3, 4),
                (4, 5),
                (2, 6),
                (6, 7),
                (7, 8),
                (8, 9),
                (1, 10),
                (10, 11)
            ]
        );
        let limit = &plan.nodes[0];
        assert_eq!(limit.property("limit"), Some("20"));
        assert_eq!(limit.property("limit_offset"), Some("0"));
        assert!(close(Some(limit.estimated_cost), 6674121.110725653));
        assert_eq!(plan.nodes[2].sort_keys, vec!["c.`name`"]);

        let lookup = &plan.nodes[5];
        assert_eq!(lookup.relation, None, "<subquery3> is not a real table");
        assert_eq!(lookup.index_name.as_deref(), Some("<auto_distinct_key>"));
        assert_eq!(lookup.index_cond.as_deref(), Some("customer_id = c.id"));
        assert_eq!(lookup.property("lookup_references"), Some("shop.c.id"));
        assert_eq!(lookup.property("covering"), Some("false"));
        assert!(close(lookup.estimated_startup_cost, 7135.337094127391));
        assert_eq!(plan.nodes[6].property("deduplication"), Some("true"));

        let subquery = &plan.nodes[9];
        assert_eq!(
            subquery.subplan_name.as_deref(),
            Some("Select #2 (subquery in projection; dependent)")
        );
        assert_eq!(subquery.property("subquery_location"), Some("projection"));
        assert_eq!(subquery.property("dependent"), Some("true"));
        // The scan of `orders` has no alias in the document.
        assert_eq!(plan.nodes[8].alias, None);
        assert_eq!(plan.nodes[8].relation.as_deref(), Some("orders"));
    }

    #[test]
    fn mysql9_analyze_and_never_executed_nodes() {
        let plan = parse_mysql_explain_json_v2(JOIN_ANALYZE_97).unwrap();
        assert!(plan.has_actuals);
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.execution_time_ms.is_some());
        assert!(plan.nodes.iter().all(|n| !n.never_executed));
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_some()));
        for node in &plan.nodes {
            let (Some(rows), Some(loops), Some(total)) =
                (node.actual_rows, node.loops, node.actual_rows_total)
            else {
                panic!("{node:?}");
            };
            assert!((rows * loops - total).abs() < 1e-6);
        }

        let plan = parse_mysql_explain_json_v2(NEVER_97).unwrap();
        assert_eq!(plan.nodes.len(), 3);
        let root = &plan.nodes[0];
        assert_eq!(root.physical_op, "Nested loop inner join");
        assert_eq!(root.join_type.as_deref(), Some("Inner"));
        assert_eq!(root.actual_rows, Some(0.0));
        assert!(close(root.actual_time_ms, 0.012542));
        assert!(close(Some(root.estimated_cost), 3.1727191448211673));
        let c = &plan.nodes[1];
        assert_eq!(c.relation.as_deref(), Some("customers"));
        assert_eq!(c.index_name.as_deref(), Some("customers_email_idx"));
        assert_eq!(
            c.index_cond.as_deref(),
            Some("email = 'nobody@example.com'")
        );
        assert_eq!(c.estimated_cost, 0.35);
        assert!(!c.never_executed);
        let o = &plan.nodes[2];
        assert!(o.never_executed);
        assert_eq!(o.actual_rows, None);
        assert_eq!(o.loops, None);
        assert_eq!(o.actual_time_ms, None);
        assert!(close(Some(o.estimated_cost), 2.822719144821167));
    }

    #[test]
    fn statements_without_an_iterator_plan_get_a_specific_error() {
        for doc in [
            DELETE_97,
            r#"{"operation": "<not executable by iterator executor>", "query_type": "delete"}"#,
        ] {
            let error = parse_mysql_explain_json_v2(doc).unwrap_err().to_string();
            assert!(error.contains("delete statement"), "{error}");
            assert!(error.contains("explain_json_format_version = 1"), "{error}");
        }
    }

    #[test]
    fn other_json_documents_are_errors() {
        for doc in [
            "",
            "[1,2,3]",
            "{}",
            r#"{"query_block": {"select_id": 1}}"#,
            r#"{"query_plan": 3}"#,
            include_str!("../../../tests/golden/plans/mysql/mysql84-q1-join-v1.json"),
            include_str!("../../../tests/golden/plans/mariadb/mariadb-q1-join-analyze.json"),
        ] {
            assert!(parse_mysql_explain_json_v2(doc).is_err(), "{doc}");
        }
    }

    #[test]
    fn mangled_documents_never_panic() {
        for sample in [RANGE_ANALYZE_84, SUBQUERIES_97, NEVER_97, DELETE_97] {
            common::assert_survives_mangling(sample, |doc| {
                let _ = parse_mysql_explain_json_v2(doc);
            });
        }
    }
}
