//! PostgreSQL — `EXPLAIN (FORMAT JSON)` / `EXPLAIN (ANALYZE, FORMAT JSON)`.

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;
use serde_json::Value;

/// Parse PostgreSQL `EXPLAIN (FORMAT JSON)` / `EXPLAIN (ANALYZE, FORMAT
/// JSON)` output into the shared model.
pub fn parse_pg_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("EXPLAIN JSON: {e}")))?;
    let root = value
        .get(0)
        .and_then(|entry| entry.get("Plan"))
        .ok_or_else(|| PlanError::InvalidStructure("EXPLAIN JSON: missing [0].Plan".into()))?;

    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    walk_pg_node(root, None, &mut nodes, &mut edges);

    let total_subtree_cost = nodes.first().map(|n| n.estimated_cost).unwrap_or(0.0);
    let estimated_rows = nodes.first().map(|n| n.estimated_rows).unwrap_or(0.0);
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: "EXPLAIN".to_string(),
        nodes,
        edges,
        total_subtree_cost,
        estimated_rows,
        warnings: Vec::new(),
    })
}

fn walk_pg_node(
    node: &Value,
    parent: Option<i32>,
    nodes: &mut Vec<PlanNode>,
    edges: &mut Vec<PlanEdge>,
) {
    let id = nodes.len() as i32 + 1;
    let node_type = node
        .get("Node Type")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_string();
    let relation = node.get("Relation Name").and_then(Value::as_str);
    let mut warnings = Vec::new();
    if node_type == "Seq Scan"
        && let Some(table) = relation
    {
        warnings.push(PlanWarning::TableScan {
            table: table.to_string(),
            schema: node
                .get("Schema")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        });
    }
    let mut output_list = Vec::new();
    if let Some(relation) = relation {
        output_list.push(relation.to_string());
    }
    if let Some(filter) = node.get("Filter").and_then(Value::as_str) {
        output_list.push(format!("Filter: {filter}"));
    }
    if let Some(cond) = node.get("Index Cond").and_then(Value::as_str) {
        output_list.push(format!("Index Cond: {cond}"));
    }
    if let Some(index) = node.get("Index Name").and_then(Value::as_str) {
        output_list.push(format!("Index: {index}"));
    }
    nodes.push(PlanNode {
        id,
        logical_op: node_type.clone(),
        physical_op: node_type,
        estimated_rows: node.get("Plan Rows").and_then(Value::as_f64).unwrap_or(0.0),
        estimated_cost: node
            .get("Total Cost")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        estimated_cpu: 0.0,
        estimated_io: 0.0,
        actual_rows: node.get("Actual Rows").and_then(Value::as_f64),
        actual_rewinds: None,
        actual_rebinds: None,
        row_count: None,
        output_list,
        warnings,
    });
    if let Some(parent) = parent {
        edges.push(PlanEdge {
            from_node: parent,
            to_node: id,
        });
    }
    if let Some(children) = node.get("Plans").and_then(Value::as_array) {
        for child in children {
            walk_pg_node(child, Some(id), nodes, edges);
        }
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
    fn pg_seq_scan_parses_and_reports_a_table_scan_finding() {
        let plan = parse_pg_explain_json(PG_SEQ_SCAN).unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert_eq!(plan.nodes[0].physical_op, "Seq Scan");
        assert_eq!(plan.total_subtree_cost, 25.88);
        assert_eq!(plan.nodes[0].actual_rows, Some(0.0));
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("plan_demo")),
            "{findings:?}"
        );
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
