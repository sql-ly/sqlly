//! DuckDB — `EXPLAIN (FORMAT JSON)` and `EXPLAIN ANALYZE (FORMAT JSON)`.
//!
//! Both come back as the `explain_value` column of a two-column result. The
//! estimated form is a JSON array of operator objects (`name`, `children`,
//! `extra_info` with `Estimated Cardinality`, `Table`, `Filters`,
//! `Projections`, …). The ANALYZE form is a profiling object. DuckDB 2.0
//! keeps its operators in an `operator` array, wrapped in
//! `RESULT_COLLECTOR` / `EXECUTE` / `EXPLAIN_ANALYZE` nodes, and spells their
//! fields `type`, `intermediate_rows` (actual rows) and `timing` (seconds);
//! 1.x used `children`, `operator_type`, `operator_cardinality` and
//! `operator_timing`. The parser accepts both, drops the 2.0 wrappers, and
//! reads whatever runtime fields are present.

use serde_json::Value;

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;

/// Parse one DuckDB EXPLAIN JSON document into a plan. Actual (ANALYZE)
/// captures fill `actual_rows` and use per-operator time as the node cost so
/// the plan pane's cost shading highlights where the time went.
pub fn parse_duckdb_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("DuckDB EXPLAIN JSON: {e}")))?;
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
        builder.visit(root, None)?;
    }
    let analyzed = builder.nodes.iter().any(|n| n.actual_rows.is_some());
    let total_subtree_cost = builder.nodes.iter().map(|n| n.estimated_cost).sum();
    // The plan's own estimate is the top operator's; ANALYZE wraps the tree
    // in a query node with no estimate, so fall through to the first that
    // has one.
    let estimated_rows = builder
        .nodes
        .iter()
        .find(|n| n.estimated_rows > 0.0)
        .map(|n| n.estimated_rows)
        .unwrap_or(0.0);
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: if analyzed {
            "EXPLAIN ANALYZE".to_string()
        } else {
            "EXPLAIN".to_string()
        },
        nodes: builder.nodes,
        edges: builder.edges,
        total_subtree_cost,
        estimated_rows,
        warnings: Vec::new(),
    })
}

/// DuckDB 2.0's EXPLAIN ANALYZE plumbing above the query's own operators.
const WRAPPER_OPERATORS: [&str; 3] = ["RESULT_COLLECTOR", "EXECUTE", "EXPLAIN_ANALYZE"];

/// An operator's children: `children`, or the profiling root's `operator`.
fn children(object: &serde_json::Map<String, Value>) -> &[Value] {
    object
        .get("children")
        .or_else(|| object.get("operator"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

impl Builder {
    fn visit(&mut self, node: &Value, parent: Option<i32>) -> PlanResult<()> {
        let object = node.as_object().ok_or_else(|| {
            PlanError::InvalidStructure("DuckDB EXPLAIN JSON: operator is not an object".into())
        })?;
        let name = object
            .get("name")
            .or_else(|| object.get("operator_type"))
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "QUERY".to_string());
        if WRAPPER_OPERATORS.contains(&name.as_str()) {
            for child in children(object) {
                self.visit(child, parent)?;
            }
            return Ok(());
        }
        let extra = object.get("extra_info").and_then(Value::as_object);
        let extra_str = |key: &str| extra.and_then(|e| e.get(key)).and_then(Value::as_str);
        let estimated_rows = extra
            .and_then(|e| e.get("Estimated Cardinality"))
            .and_then(number)
            .unwrap_or(0.0);
        let actual_rows = object
            .get("operator_cardinality")
            .or_else(|| object.get("intermediate_rows"))
            .and_then(number);
        let timing = object
            .get("operator_timing")
            .or_else(|| object.get("timing"))
            .and_then(number);
        let table = extra_str("Table").map(str::to_string);
        let physical_op = match &table {
            Some(table) => format!("{name} {table}"),
            None => match extra_str("Type") {
                Some(kind) if kind != name => format!("{name} ({kind})"),
                _ => name.clone(),
            },
        };
        let output_list = match extra.and_then(|e| e.get("Projections")) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            _ => Vec::new(),
        };
        // A scan operator over a table with no pushed-down filter reads the
        // whole table; filtered scans still prune with zone maps.
        let mut warnings = Vec::new();
        if let Some(table) = &table
            && name.contains("SCAN")
            && !name.contains("INDEX")
            && extra_str("Filters").is_none()
        {
            warnings.push(PlanWarning::TableScan {
                table: table.clone(),
                schema: String::new(),
            });
        }
        let id = self.nodes.len() as i32 + 1;
        // DuckDB publishes no cost units. An ANALYZE capture carries each
        // operator's time, which is the cost worth shading; an estimated
        // plan only has cardinalities, so the estimated row count stands in
        // as the relative cost so the heaviest operators still light up.
        let estimated_cost = timing.unwrap_or(estimated_rows);
        self.nodes.push(PlanNode {
            id,
            logical_op: name,
            physical_op,
            estimated_rows,
            estimated_cost,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows,
            actual_rewinds: None,
            actual_rebinds: None,
            row_count: None,
            output_list,
            warnings,
        });
        if let Some(parent) = parent {
            self.edges.push(PlanEdge {
                from_node: parent,
                to_node: id,
            });
        }
        for child in children(object) {
            self.visit(child, Some(id))?;
        }
        Ok(())
    }
}

/// DuckDB writes numbers as numbers in ANALYZE output and as strings inside
/// `extra_info` (`"Estimated Cardinality": "200"`).
fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
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
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("ex_a")),
            "{findings:?}"
        );
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
}
