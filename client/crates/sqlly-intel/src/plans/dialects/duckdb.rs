//! DuckDB: `EXPLAIN (FORMAT JSON)` and `EXPLAIN ANALYZE (FORMAT JSON)`.
//!
//! Both come back as the `explain_value` column of a two-column result. The
//! estimated form is a JSON array of operator objects (`name`, `children`,
//! `extra_info` with `Estimated Cardinality`, `Table`, `Filters`,
//! `Projections`, …). The ANALYZE form is a profiling object. DuckDB 2.0
//! keeps its operators in an `operator` array, wrapped in
//! `RESULT_COLLECTOR` / `EXECUTE` / `EXPLAIN_ANALYZE` nodes, and spells their
//! fields `type`, `intermediate_rows` (actual rows) and `timing` (seconds);
//! 1.x used `children`, `operator_type`, `operator_cardinality` and
//! `operator_timing`. The parser accepts both, drops the wrappers, and
//! reads whatever runtime fields are present.
//!
//! The DuckDB CLI prints a `Physical Plan` title box above the JSON; text
//! before the first `[` or `{` line and after the JSON value is ignored.

use serde_json::Value;

use crate::plans::dialects::PlanDialect;
use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode, PlanProperty};
use crate::plans::warning::PlanWarning;

/// Operators nested deeper than this are treated as a malformed document.
pub(super) const MAX_DEPTH: usize = 512;

/// Parse one DuckDB EXPLAIN JSON document into a plan. Actual (ANALYZE)
/// captures fill `actual_rows` and use per-operator time as the node cost so
/// the plan pane's cost shading highlights where the time went.
pub fn parse_duckdb_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value = read_json(doc)?;
    let roots: Vec<&Value> = match &value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![&value],
        _ => {
            return Err(PlanError::InvalidStructure(
                "DuckDB EXPLAIN JSON: expected an operator tree".into(),
            ));
        }
    };
    if roots.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DuckDB EXPLAIN JSON: no operators".into(),
        ));
    }
    let mut builder = Builder::default();
    for root in roots {
        builder.visit(root, None, 0)?;
    }
    if builder.nodes.is_empty() {
        return Err(PlanError::InvalidStructure(
            "DuckDB EXPLAIN JSON: no operators".into(),
        ));
    }
    let mut plan = ExecutionPlan {
        nodes: builder.nodes,
        edges: builder.edges,
        source_format: Some("duckdb-json".to_string()),
        ..Default::default()
    };
    if let Some(profile) = value.as_object() {
        // 1.x: `query_name` and `latency` (seconds) on the profiling root.
        // 2.0: `query.total_time`.
        if let Some(query) = profile.get("query_name").and_then(Value::as_str) {
            plan.statement_text = strip_explain_prefix(query).to_string();
        }
        let seconds = profile.get("latency").and_then(number).or_else(|| {
            profile
                .get("query")
                .and_then(|q| q.get("total_time"))
                .and_then(number)
        });
        plan.execution_time_ms = seconds.filter(|s| *s > 0.0).map(|s| s * 1000.0);
    }
    finish_duckdb_plan(&mut plan);
    Ok(plan)
}

/// Parse the first JSON value in `doc`, skipping any lines before it.
fn read_json(doc: &str) -> PlanResult<Value> {
    let text = doc.trim();
    let first_error = match serde_json::from_str::<Value>(text) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if (trimmed.starts_with('[') || trimmed.starts_with('{'))
            && let Some(rest) = text.get(offset..)
            && let Some(Ok(value)) = serde_json::Deserializer::from_str(rest)
                .into_iter::<Value>()
                .next()
        {
            return Ok(value);
        }
        offset += line.len();
    }
    Err(PlanError::InvalidStructure(format!(
        "DuckDB EXPLAIN JSON: {first_error}"
    )))
}

/// `EXPLAIN ANALYZE SELECT ...` → `SELECT ...`.
pub(super) fn strip_explain_prefix(query: &str) -> &str {
    let mut rest = query.trim();
    for word in ["EXPLAIN", "ANALYZE"] {
        if let Some(head) = rest.get(..word.len())
            && head.eq_ignore_ascii_case(word)
            && rest
                .get(word.len()..)
                .is_some_and(|tail| tail.starts_with(char::is_whitespace))
        {
            rest = rest.get(word.len()..).unwrap_or("").trim_start();
        }
    }
    rest
}

/// Plan-level bookkeeping shared by the JSON and text parsers: statement
/// type, cost total, the plan's row estimate, and inclusive times.
pub(super) fn finish_duckdb_plan(plan: &mut ExecutionPlan) {
    let analyzed = plan.execution_time_ms.is_some()
        || plan
            .nodes
            .iter()
            .any(|n| n.actual_rows.is_some() || n.actual_self_time_ms.is_some());
    plan.statement_type = if analyzed {
        "EXPLAIN ANALYZE".to_string()
    } else {
        "EXPLAIN".to_string()
    };
    plan.dialect = Some(PlanDialect::DuckDb);
    plan.total_subtree_cost = plan.nodes.iter().map(|n| n.estimated_cost).sum();
    // The plan's own estimate is the top operator's; ANALYZE wraps the tree
    // in a query node with no estimate, so fall through to the first that
    // has one.
    plan.estimated_rows = plan
        .nodes
        .iter()
        .find(|n| n.estimated_rows > 0.0)
        .map(|n| n.estimated_rows)
        .unwrap_or(0.0);
    // DuckDB times each operator on its own. The model's `actual_time_ms`
    // includes the inputs, so add them up from the leaves. Children always
    // have higher ids than their parent here.
    if plan.nodes.iter().any(|n| n.actual_self_time_ms.is_some()) {
        let mut inclusive: std::collections::HashMap<i32, f64> = std::collections::HashMap::new();
        // Index the edges once: scanning them all for every operator is
        // quadratic in the size of the plan.
        let mut inputs_of: std::collections::HashMap<i32, Vec<i32>> =
            std::collections::HashMap::new();
        for edge in &plan.edges {
            inputs_of
                .entry(edge.from_node)
                .or_default()
                .push(edge.to_node);
        }
        for node in plan.nodes.iter_mut().rev() {
            let inputs: f64 = inputs_of
                .get(&node.id)
                .into_iter()
                .flatten()
                .filter_map(|input| inclusive.get(input))
                .sum();
            let total = node.actual_self_time_ms.unwrap_or(0.0) + inputs;
            inclusive.insert(node.id, total);
            node.actual_time_ms = Some(total);
        }
    }
    super::common::finish_plan(plan);
}

/// DuckDB 2.0's EXPLAIN ANALYZE plumbing above the query's own operators.
pub(super) const WRAPPER_OPERATORS: [&str; 3] = ["RESULT_COLLECTOR", "EXECUTE", "EXPLAIN_ANALYZE"];

/// An operator's children: `children`, or the profiling root's `operator`.
fn children(object: &serde_json::Map<String, Value>) -> &[Value] {
    object
        .get("children")
        .or_else(|| object.get("operator"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// One operator as either format describes it. `entries` are the labelled
/// lines of the operator (`Table`, `Filters`, `Projections`, …) in document
/// order; the empty label holds unlabelled lines (the JSON key `Text`).
#[derive(Debug, Default)]
pub(super) struct Operator {
    pub name: String,
    pub entries: Vec<(String, Vec<String>)>,
    pub estimated_rows: Option<f64>,
    pub actual_rows: Option<f64>,
    /// The operator's own time in seconds, summed over its threads.
    pub timing_s: Option<f64>,
    pub properties: Vec<PlanProperty>,
}

impl Operator {
    fn values(&self, label: &str) -> Option<&[String]> {
        self.entries
            .iter()
            .find(|(name, values)| name == label && !values.is_empty())
            .map(|(_, values)| values.as_slice())
    }
}

/// Build the model node for one operator.
pub(super) fn operator_node(id: i32, op: Operator) -> PlanNode {
    let name = op.name.trim().to_string();
    let first = |label: &str| op.values(label).and_then(|v| v.first()).cloned();
    let is_scan = name.contains("SCAN");
    let unlabelled = op.values("");
    // DuckDB 1.1 prints a scan's table as a bare line; 1.2 and later label
    // it `Table:`.
    let table = first("Table").or_else(|| match unlabelled {
        Some([only]) if is_scan => Some(only.clone()),
        _ => None,
    });
    let kind = first("Type");
    let physical_op = match (&table, &kind) {
        (Some(table), _) => format!("{name} {table}"),
        (None, Some(kind)) if *kind != name => format!("{name} ({kind})"),
        _ => name.clone(),
    };
    let filters = op.values("Filters").map(|v| v.join(" AND "));
    let projections: Vec<String> = match (op.values("Projections"), unlabelled) {
        (Some(values), _) => values.to_vec(),
        (None, Some(values)) if name == "PROJECTION" => values.to_vec(),
        _ => Vec::new(),
    };
    let sorts_unlabelled = name.starts_with("ORDER_BY") || name.starts_with("TOP_N");
    let sort_keys: Vec<String> = match (op.values("Order By"), unlabelled) {
        (Some(values), _) => values.to_vec(),
        (None, Some(values)) if sorts_unlabelled => values.to_vec(),
        _ => Vec::new(),
    };
    // A scan operator over a table with no pushed-down filter reads the
    // whole table; filtered scans still prune with zone maps.
    let mut warnings = Vec::new();
    if let Some(table) = &table
        && is_scan
        && !name.contains("INDEX")
        && !kind.as_deref().is_some_and(|k| k.contains("Index"))
        && filters.is_none()
    {
        warnings.push(PlanWarning::TableScan {
            table: table.clone(),
            schema: String::new(),
        });
    }
    let estimated_rows = op.estimated_rows.unwrap_or(0.0);
    let mut node = PlanNode {
        id,
        logical_op: name.clone(),
        physical_op,
        estimated_rows,
        // DuckDB publishes no cost units. An ANALYZE capture carries each
        // operator's time, which is the cost worth shading; an estimated
        // plan only has cardinalities, so the estimated row count stands in
        // as the relative cost so the heaviest operators still light up.
        estimated_cost: op.timing_s.unwrap_or(estimated_rows),
        actual_rows: op.actual_rows,
        actual_rows_total: op.actual_rows,
        actual_self_time_ms: op.timing_s.map(|s| s * 1000.0),
        output_list: projections.clone(),
        output_columns: projections,
        warnings,
        filter: filters,
        join_type: first("Join Type"),
        join_predicate: op.values("Conditions").map(|v| v.join(" AND ")),
        group_keys: op
            .values("Groups")
            .map(<[String]>::to_vec)
            .unwrap_or_default(),
        sort_keys,
        ..Default::default()
    };
    if let Some(table) = &table {
        // `database.schema.table`, `schema.table` or `table`.
        let mut parts = table.rsplitn(3, '.');
        node.relation = parts.next().map(str::to_string);
        node.schema = parts.next().map(str::to_string);
        node.database = parts.next().map(str::to_string);
    }
    let table_is_unlabelled = table.is_some() && first("Table").is_none();
    for (label, values) in &op.entries {
        if values.is_empty() {
            continue;
        }
        let typed = match label.as_str() {
            "Table" | "Filters" | "Projections" | "Join Type" | "Conditions" | "Groups"
            | "Order By" => true,
            "" => name == "PROJECTION" || sorts_unlabelled || table_is_unlabelled,
            _ => false,
        };
        if !typed {
            node.properties.push(PlanProperty {
                name: if label.is_empty() {
                    "Info".to_string()
                } else {
                    label.clone()
                },
                value: values.join(", "),
            });
        }
    }
    node.properties.extend(op.properties);
    node
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

/// Keys of an operator object that the typed fields already carry, or that
/// are structure rather than facts.
const HANDLED_KEYS: [&str; 12] = [
    "name",
    "operator_type",
    "operator_name",
    "type",
    "children",
    "operator",
    "extra_info",
    "operator_cardinality",
    "intermediate_rows",
    "operator_timing",
    "timing",
    "query_name",
];

impl Builder {
    fn visit(&mut self, node: &Value, parent: Option<i32>, depth: usize) -> PlanResult<()> {
        if depth > MAX_DEPTH {
            return Err(PlanError::InvalidStructure(
                "DuckDB EXPLAIN JSON: the plan nests too deeply".into(),
            ));
        }
        let object = node.as_object().ok_or_else(|| {
            PlanError::InvalidStructure("DuckDB EXPLAIN JSON: operator is not an object".into())
        })?;
        let name = object
            .get("name")
            .or_else(|| object.get("operator_type"))
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .map(|name| name.trim().to_string())
            .unwrap_or_else(|| "QUERY".to_string());
        if WRAPPER_OPERATORS.contains(&name.as_str()) {
            for child in children(object) {
                self.visit(child, parent, depth + 1)?;
            }
            return Ok(());
        }
        let mut op = Operator {
            name,
            actual_rows: object
                .get("operator_cardinality")
                .or_else(|| object.get("intermediate_rows"))
                .and_then(number),
            timing_s: object
                .get("operator_timing")
                .or_else(|| object.get("timing"))
                .and_then(number),
            ..Default::default()
        };
        if let Some(extra) = object.get("extra_info").and_then(Value::as_object) {
            for (key, value) in extra {
                if key == "Estimated Cardinality" {
                    op.estimated_rows = number(value);
                    continue;
                }
                let values = match value {
                    Value::String(s) => vec![s.clone()],
                    Value::Array(items) => items
                        .iter()
                        .map(|item| match item {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .collect(),
                    Value::Null => Vec::new(),
                    other => vec![other.to_string()],
                };
                // DuckDB 1.1 files unlabelled text under `Text`.
                let label = if key == "Text" {
                    String::new()
                } else {
                    key.clone()
                };
                op.entries.push((label, values));
            }
        }
        // Every other counter the profile carries for this operator, when it
        // is not zero (`operator_rows_scanned`, `result_set_size`, …).
        for (key, value) in object {
            if HANDLED_KEYS.contains(&key.as_str()) {
                continue;
            }
            if let Some(n) = value.as_f64().filter(|n| *n != 0.0) {
                op.properties.push(PlanProperty {
                    name: key.clone(),
                    value: if n.fract() == 0.0 && n.abs() < 1e15 {
                        format!("{}", n as i64)
                    } else {
                        n.to_string()
                    },
                });
            }
        }
        let id = self.nodes.len() as i32 + 1;
        self.nodes.push(operator_node(id, op));
        if let Some(parent) = parent {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        for child in children(object) {
            self.visit(child, Some(id), depth + 1)?;
        }
        Ok(())
    }
}

/// DuckDB writes numbers as numbers in ANALYZE output and as strings inside
/// `extra_info` (`"Estimated Cardinality": "200"`).
fn number(value: &Value) -> Option<f64> {
    let parsed: Option<f64> = match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    };
    parsed.filter(|n| n.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::findings_for_plan;

    const ESTIMATED: &str = r#"[{"name":"PROJECTION","children":[{"name":"HASH_JOIN","children":[{"name":"SEQ_SCAN","children":[],"extra_info":{"Table":"memory.main.ex_b","Type":"Sequential Scan","Projections":"a_id","Filters":"a_id>10","Estimated Cardinality":"1000"}},{"name":"SEQ_SCAN","children":[],"extra_info":{"Table":"memory.main.ex_a","Type":"Sequential Scan","Projections":["id","v"],"Estimated Cardinality":"200"}}],"extra_info":{"Join Type":"INNER","Conditions":"a_id = id","Estimated Cardinality":"200"}}],"extra_info":{"Projections":["v"],"Estimated Cardinality":"200"}}]"#;

    const ANALYZED: &str = r#"{"query_name":"SELECT 1","latency":0.002,"children":[{"operator_type":"PROJECTION","operator_timing":0.0001,"operator_cardinality":3,"extra_info":{"Projections":"i","Estimated Cardinality":"3"},"children":[{"operator_type":"SEQ_SCAN","operator_timing":0.0015,"operator_cardinality":3,"extra_info":{"Table":"t","Estimated Cardinality":"3"},"children":[]}]}]}"#;

    #[test]
    fn estimated_tree_becomes_nodes_edges_and_scan_warnings() {
        let plan = parse_duckdb_explain_json(ESTIMATED).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.edges.len(), 3);
        assert_eq!(plan.estimated_rows, 200.0);
        assert_eq!(plan.nodes[0].logical_op, "PROJECTION");
        assert_eq!(plan.nodes[0].output_list, vec!["v".to_string()]);
        let join = &plan.nodes[1];
        assert_eq!(join.physical_op, "HASH_JOIN");
        assert_eq!(join.estimated_rows, 200.0);
        // The filtered scan is not flagged; the unfiltered one is.
        let filtered = &plan.nodes[2];
        assert_eq!(filtered.physical_op, "SEQ_SCAN memory.main.ex_b");
        assert!(filtered.warnings.is_empty());
        let full = &plan.nodes[3];
        assert_eq!(full.output_list, vec!["id".to_string(), "v".to_string()]);
        assert!(matches!(
            full.warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "memory.main.ex_a"
        ));
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
        // No cost units exist for an estimated plan, so the cardinality
        // stands in and the pane can still shade the heavier operators.
        assert_eq!(plan.nodes[2].estimated_cost, 1000.0);
        assert_eq!(plan.nodes[3].estimated_cost, 200.0);
        assert_eq!(plan.total_subtree_cost, 1600.0);
        // The plan is too small for a finding: the rules need measured rows
        // or a meaningful share of the cost before they call out a scan.
        let findings = findings_for_plan(&plan);
        assert!(findings.is_empty(), "{findings:?}");
    }

    #[test]
    fn analyzed_tree_carries_actual_rows_and_timing_as_cost() {
        let plan = parse_duckdb_explain_json(ANALYZED).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert_eq!(plan.nodes.len(), 3);
        assert_eq!(plan.nodes[0].logical_op, "QUERY");
        assert_eq!(plan.nodes[1].logical_op, "PROJECTION");
        assert_eq!(plan.nodes[1].actual_rows, Some(3.0));
        assert_eq!(plan.nodes[2].estimated_cost, 0.0015);
        assert!((plan.total_subtree_cost - 0.0016).abs() < 1e-9);
        assert_eq!(plan.estimated_rows, 3.0);
    }

    /// DuckDB 2.0's EXPLAIN ANALYZE profile, trimmed.
    const ANALYZED_V2: &str = r#"{"io":{"total_bytes_read":0},"query":{"total_time":0.013},"operator":[{"type":"RESULT_COLLECTOR","timing":0.0,"intermediate_rows":0,"extra_info":{},"children":[{"type":"EXECUTE","timing":0.0,"intermediate_rows":0,"extra_info":{},"children":[{"type":"EXPLAIN_ANALYZE","timing":0.0,"intermediate_rows":0,"extra_info":{},"children":[{"type":"HASH_JOIN","timing":0.0002,"intermediate_rows":3,"extra_info":{"Join Type":"INNER","Estimated Cardinality":"3"},"children":[{"type":"TABLE_SCAN","timing":0.00004,"intermediate_rows":3,"extra_info":{"Table":"main.orders","Type":"Sequential Scan","Estimated Cardinality":"3"},"children":[]},{"type":"TABLE_SCAN","timing":0.00003,"intermediate_rows":2,"extra_info":{"Table":"main.customers","Type":"Sequential Scan","Filters":"id>0","Estimated Cardinality":"2"},"children":[]}]}]}]}]}]}"#;

    #[test]
    fn inclusive_time_adds_up_under_a_very_wide_operator() {
        // One union over many scans: every scan is an input of the same
        // operator, which is the shape that made the per-operator edge scan
        // quadratic.
        const SCANS: usize = 20_000;
        let scan = r#"{"operator_type":"SEQ_SCAN","operator_timing":0.001,"operator_cardinality":1,"extra_info":{"Table":"t"},"children":[]}"#;
        let scans = vec![scan; SCANS].join(",");
        let doc = format!(
            r#"{{"query_name":"q","latency":1.0,"children":[{{"operator_type":"UNION","operator_timing":0.002,"operator_cardinality":1,"extra_info":{{}},"children":[{scans}]}}]}}"#
        );
        let started = std::time::Instant::now();
        let plan = parse_duckdb_explain_json(&doc).unwrap();
        let union = plan
            .nodes
            .iter()
            .find(|node| node.logical_op == "UNION")
            .unwrap();
        assert!((union.actual_self_time_ms.unwrap() - 2.0).abs() < 1e-9);
        let expected = 2.0 + SCANS as f64;
        assert!((union.actual_time_ms.unwrap() - expected).abs() < 1e-3);
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[test]
    fn duckdb_2_profile_drops_wrappers_and_reads_new_field_names() {
        let plan = parse_duckdb_explain_json(ANALYZED_V2).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        let ops: Vec<_> = plan.nodes.iter().map(|n| n.logical_op.as_str()).collect();
        assert_eq!(ops, ["QUERY", "HASH_JOIN", "TABLE_SCAN", "TABLE_SCAN"]);
        assert_eq!(plan.edges.len(), 3);
        assert_eq!(plan.edges[0].from_node, 1);
        assert_eq!(plan.edges[0].to_node, 2);
        assert_eq!(plan.nodes[1].actual_rows, Some(3.0));
        assert_eq!(plan.nodes[3].actual_rows, Some(2.0));
        assert_eq!(plan.nodes[2].estimated_cost, 0.00004);
        assert_eq!(plan.nodes[2].physical_op, "TABLE_SCAN main.orders");
        assert!(matches!(
            plan.nodes[2].warnings.as_slice(),
            [PlanWarning::TableScan { table, .. }] if table == "main.orders"
        ));
        assert!(plan.nodes[3].warnings.is_empty());
    }

    #[test]
    fn malformed_documents_yield_typed_errors_not_panics() {
        assert!(parse_duckdb_explain_json("not json").is_err());
        assert!(parse_duckdb_explain_json("[]").is_err());
        assert!(parse_duckdb_explain_json("42").is_err());
        assert!(parse_duckdb_explain_json("[1]").is_err());
    }

    const JOIN_EXPLAIN_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-explain.json");
    const JOIN_ANALYZE_142: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb142-join-analyze.json");
    const JOIN_EXPLAIN_113: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb113-join-explain.json");
    const JOIN_ANALYZE_113: &str =
        include_str!("../../../tests/golden/plans/duckdb/duckdb113-join-analyze.json");

    fn ops(plan: &ExecutionPlan) -> Vec<&str> {
        plan.nodes.iter().map(|n| n.logical_op.as_str()).collect()
    }

    fn prop<'a>(node: &'a PlanNode, name: &str) -> Option<&'a str> {
        node.properties
            .iter()
            .find(|p| p.name == name)
            .map(|p| p.value.as_str())
    }

    #[test]
    fn cli_explain_json_under_its_title_box_fills_typed_fields() {
        // The CLI prints the `Physical Plan` title box above the JSON.
        assert!(JOIN_EXPLAIN_142.trim_start().starts_with('\u{250c}'));
        let plan = parse_duckdb_explain_json(JOIN_EXPLAIN_142).unwrap();
        assert_eq!(plan.source_format.as_deref(), Some("duckdb-json"));
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert!(!plan.has_actuals);
        assert_eq!(
            ops(&plan),
            [
                "PROJECTION",
                "ORDER_BY",
                "PROJECTION",
                "PROJECTION",
                "HASH_GROUP_BY",
                "PROJECTION",
                "PROJECTION",
                "HASH_JOIN",
                "SEQ_SCAN",
                "HASH_JOIN",
                "SEQ_SCAN",
                "SEQ_SCAN",
            ]
        );
        assert_eq!(
            plan.nodes[1].sort_keys,
            ["sum((oi.quantity * oi.unit_price)) DESC"]
        );
        assert_eq!(plan.nodes[4].group_keys, ["#0"]);
        assert_eq!(
            prop(&plan.nodes[4], "Aggregates"),
            Some("count_star(), sum(#1)")
        );
        let join = &plan.nodes[7];
        assert_eq!(join.join_type.as_deref(), Some("INNER"));
        assert_eq!(join.join_predicate.as_deref(), Some("order_id = id"));
        assert_eq!(join.estimated_rows, 96231.0);
        // `"name": "SEQ_SCAN "` carries a trailing space in the document.
        let items = &plan.nodes[8];
        assert_eq!(items.physical_op, "SEQ_SCAN order_items");
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(items.output_columns, ["order_id", "quantity", "unit_price"]);
        assert_eq!(items.estimated_rows, 1_200_000.0);
        assert_eq!(items.warnings.len(), 1);
        let orders = &plan.nodes[10];
        assert_eq!(orders.filter.as_deref(), Some("status='paid'"));
        assert!(orders.warnings.is_empty());
        assert_eq!(plan.edges.len(), 11);
    }

    #[test]
    fn analyze_json_fills_rows_and_per_operator_time() {
        let plan = parse_duckdb_explain_json(JOIN_ANALYZE_142).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN ANALYZE");
        assert!(plan.has_actuals);
        // The capture's `latency` is 0.0, so there is no total to report.
        assert_eq!(plan.execution_time_ms, None);
        assert_eq!(plan.nodes.len(), 13);
        assert_eq!(plan.nodes[0].logical_op, "QUERY");
        assert_eq!(plan.nodes[1].logical_op, "PROJECTION");
        let join = &plan.nodes[8];
        assert_eq!(join.actual_rows, Some(300_000.0));
        assert_eq!(join.estimated_rows, 96231.0);
        let items = &plan.nodes[9];
        assert_eq!(items.physical_op, "TABLE_SCAN order_items");
        assert_eq!(items.actual_rows, Some(1_199_991.0));
        assert_eq!(items.actual_rows_total, Some(1_199_991.0));
        assert!((items.actual_self_time_ms.unwrap() - 65.201174).abs() < 1e-6);
        assert!((items.estimated_cost - 0.065201174).abs() < 1e-9);
        // Inclusive time adds the inputs' own time.
        let inner = &plan.nodes[10];
        let expected = (0.000904253 + 0.001517586 + 0.000087794) * 1000.0;
        assert!((inner.actual_time_ms.unwrap() - expected).abs() < 1e-6);
        assert!(
            plan.nodes[1].actual_time_ms.unwrap() > items.actual_time_ms.unwrap(),
            "the root's inclusive time covers every operator"
        );
        assert_eq!(plan.nodes[11].actual_rows, Some(100_000.0));
        assert_eq!(plan.nodes[12].actual_rows, Some(50_000.0));
    }

    #[test]
    fn duckdb_1_1_json_names_the_table_under_text() {
        let plan = parse_duckdb_explain_json(JOIN_EXPLAIN_113).unwrap();
        assert_eq!(plan.nodes.len(), 12);
        let items = &plan.nodes[8];
        assert_eq!(items.physical_op, "SEQ_SCAN order_items");
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(items.warnings.len(), 1);
        assert_eq!(prop(&plan.nodes[7], "Build Max"), Some("400000"));
        assert_eq!(
            plan.nodes[10].filter.as_deref(),
            Some("status='paid' AND status IS NOT NULL")
        );

        let plan = parse_duckdb_explain_json(JOIN_ANALYZE_113).unwrap();
        assert!(plan.has_actuals);
        let items = &plan.nodes[9];
        assert_eq!(items.relation.as_deref(), Some("order_items"));
        assert_eq!(items.actual_rows, Some(1_199_991.0));
        assert!((items.actual_self_time_ms.unwrap() - 44.254).abs() < 1e-6);
    }

    #[test]
    fn duckdb_json_survives_mangled_input() {
        for sample in [JOIN_EXPLAIN_142, JOIN_ANALYZE_113] {
            super::super::common::assert_survives_mangling(sample, |doc| {
                let _ = parse_duckdb_explain_json(doc);
            });
        }
    }
}
