//! ClickHouse — `EXPLAIN json = 1, indexes = 1`.
//!
//! The capture is one row whose `explain` column holds a JSON document
//! shaped like PostgreSQL's: `[{"Plan": {"Node Type": …, "Node Id": …,
//! "Description": …, "Plans": […], "Indexes": […]}}]`. ClickHouse plans
//! carry no cost model and no cardinality estimates; the closest thing is
//! the index analysis on `ReadFromMergeTree` steps — how many parts and
//! granules survived the primary key, partition key and skip indexes — so
//! that is what feeds the row estimate (selected granules × the default
//! granule size) and the full-scan finding. There is no `EXPLAIN ANALYZE`,
//! so every capture is estimated.

use serde_json::Value;

use crate::plans::error::{PlanError, PlanResult};
use crate::plans::model::{ExecutionPlan, PlanEdge, PlanNode};
use crate::plans::warning::PlanWarning;

/// The default `index_granularity` of a MergeTree table: rows per granule.
const GRANULE_ROWS: f64 = 8192.0;

/// Parse one ClickHouse `EXPLAIN json = 1` document into a plan.
pub fn parse_clickhouse_explain_json(doc: &str) -> PlanResult<ExecutionPlan> {
    let value: Value = serde_json::from_str(doc.trim())
        .map_err(|e| PlanError::InvalidStructure(format!("ClickHouse EXPLAIN JSON: {e}")))?;
    let root = value
        .get(0)
        .and_then(|entry| entry.get("Plan"))
        .or_else(|| value.get("Plan"))
        .ok_or_else(|| {
            PlanError::InvalidStructure("ClickHouse EXPLAIN JSON: missing [0].Plan".into())
        })?;
    let mut builder = Builder::default();
    builder.visit(root, None)?;
    let estimated_rows = builder
        .nodes
        .iter()
        .filter(|n| n.estimated_rows > 0.0)
        .map(|n| n.estimated_rows)
        .fold(0.0, f64::max);
    Ok(ExecutionPlan {
        statement_text: String::new(),
        statement_type: "EXPLAIN".to_string(),
        nodes: builder.nodes,
        edges: builder.edges,
        total_subtree_cost: 0.0,
        estimated_rows,
        warnings: Vec::new(),
    })
}

#[derive(Default)]
struct Builder {
    nodes: Vec<PlanNode>,
    edges: Vec<PlanEdge>,
}

/// What the index analysis on a read step says: parts/granules before and
/// after pruning, and whether any index condition actually filtered.
#[derive(Debug, Default, Clone, Copy)]
struct IndexAnalysis {
    initial_granules: Option<f64>,
    selected_granules: Option<f64>,
    filtered: bool,
}

fn analyse_indexes(node: &Value) -> IndexAnalysis {
    let mut analysis = IndexAnalysis::default();
    let Some(indexes) = node.get("Indexes").and_then(Value::as_array) else {
        return analysis;
    };
    for index in indexes {
        let condition = index.get("Condition").and_then(Value::as_str).unwrap_or("");
        if !matches!(condition, "" | "true" | "unknown") {
            analysis.filtered = true;
        }
        let initial = index.get("Initial Granules").and_then(number);
        let selected = index.get("Selected Granules").and_then(number);
        if analysis.initial_granules.is_none() {
            analysis.initial_granules = initial;
        }
        if selected.is_some() {
            analysis.selected_granules = selected;
        }
        if let (Some(initial), Some(selected)) = (initial, selected)
            && selected < initial
        {
            analysis.filtered = true;
        }
    }
    analysis
}

impl Builder {
    fn visit(&mut self, node: &Value, parent: Option<i32>) -> PlanResult<()> {
        let object = node.as_object().ok_or_else(|| {
            PlanError::InvalidStructure("ClickHouse EXPLAIN JSON: step is not an object".into())
        })?;
        let name = object
            .get("Node Type")
            .and_then(Value::as_str)
            .unwrap_or("Step")
            .to_string();
        let description = object
            .get("Description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let is_read = name.starts_with("ReadFrom");
        let physical_op = match &description {
            Some(description) => format!("{name} {description}"),
            None => name.clone(),
        };
        let analysis = analyse_indexes(node);
        let estimated_rows = analysis
            .selected_granules
            .map(|granules| granules * GRANULE_ROWS)
            .unwrap_or(0.0);
        let mut warnings = Vec::new();
        if name == "ReadFromMergeTree"
            && let Some(table) = &description
            && !analysis.filtered
        {
            let (schema, table) = match table.split_once('.') {
                Some((schema, table)) => (schema.to_string(), table.to_string()),
                None => (String::new(), table.clone()),
            };
            warnings.push(PlanWarning::TableScan { table, schema });
        }
        let mut output_list = Vec::new();
        if is_read && let Some(indexes) = object.get("Indexes").and_then(Value::as_array) {
            for index in indexes {
                let kind = index.get("Type").and_then(Value::as_str).unwrap_or("Index");
                let condition = index.get("Condition").and_then(Value::as_str).unwrap_or("");
                let parts = match (
                    index.get("Selected Parts").and_then(number),
                    index.get("Initial Parts").and_then(number),
                ) {
                    (Some(selected), Some(initial)) => {
                        format!(" parts {selected}/{initial}")
                    }
                    _ => String::new(),
                };
                let granules = match (
                    index.get("Selected Granules").and_then(number),
                    index.get("Initial Granules").and_then(number),
                ) {
                    (Some(selected), Some(initial)) => {
                        format!(" granules {selected}/{initial}")
                    }
                    _ => String::new(),
                };
                output_list.push(
                    format!("{kind}: {condition}{parts}{granules}")
                        .trim()
                        .to_string(),
                );
            }
        }
        let id = self.nodes.len() as i32 + 1;
        self.nodes.push(PlanNode {
            id,
            logical_op: name,
            physical_op,
            estimated_rows,
            estimated_cost: 0.0,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows: None,
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
        if let Some(children) = object.get("Plans").and_then(Value::as_array) {
            for child in children {
                self.visit(child, Some(id))?;
            }
        }
        Ok(())
    }
}

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

    const FILTERED: &str = r#"[{"Plan":{"Node Type":"Expression","Node Id":"Expression_9","Description":"Project names","Plans":[{"Node Type":"Limit","Node Id":"Limit_8","Description":"preliminary LIMIT","Plans":[{"Node Type":"Aggregating","Node Id":"Aggregating_4","Plans":[{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"default.ex_a","Indexes":[{"Type":"PrimaryKey","Keys":["id"],"Condition":"(id in [11, +Inf))","Search Algorithm":"binary search","Initial Parts":1,"Selected Parts":1,"Initial Granules":4,"Selected Granules":2}]}]}]}]}}]"#;

    const FULL_SCAN: &str = r#"[{"Plan":{"Node Type":"Expression","Node Id":"Expression_2","Plans":[{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"shop.orders","Indexes":[{"Type":"PrimaryKey","Condition":"true","Initial Parts":3,"Selected Parts":3,"Initial Granules":10,"Selected Granules":10}]}]}}]"#;

    #[test]
    fn filtered_read_becomes_nodes_edges_and_granule_estimates() {
        let plan = parse_clickhouse_explain_json(FILTERED).unwrap();
        assert_eq!(plan.statement_type, "EXPLAIN");
        assert_eq!(plan.nodes.len(), 4);
        assert_eq!(plan.edges.len(), 3);
        assert_eq!(plan.nodes[0].physical_op, "Expression Project names");
        let read = &plan.nodes[3];
        assert_eq!(read.logical_op, "ReadFromMergeTree");
        assert_eq!(read.physical_op, "ReadFromMergeTree default.ex_a");
        assert_eq!(read.estimated_rows, 2.0 * GRANULE_ROWS);
        assert_eq!(plan.estimated_rows, 2.0 * GRANULE_ROWS);
        assert_eq!(
            read.output_list,
            vec!["PrimaryKey: (id in [11, +Inf)) parts 1/1 granules 2/4".to_string()]
        );
        assert!(read.warnings.is_empty(), "a pruned read is not a full scan");
        assert!(plan.nodes.iter().all(|n| n.actual_rows.is_none()));
    }

    #[test]
    fn unpruned_read_is_flagged_as_a_table_scan() {
        let plan = parse_clickhouse_explain_json(FULL_SCAN).unwrap();
        let read = &plan.nodes[1];
        assert!(matches!(
            read.warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "orders" && schema == "shop"
        ));
        let findings = findings_for_plan(&plan);
        assert!(
            findings.iter().any(|f| f.summary.contains("orders")),
            "{findings:?}"
        );
    }

    #[test]
    fn reads_without_index_analysis_are_full_scans_and_a_bare_plan_object_parses() {
        let plan = parse_clickhouse_explain_json(
            r#"{"Plan":{"Node Type":"ReadFromMergeTree","Node Id":"ReadFromMergeTree_0","Description":"t"}}"#,
        )
        .unwrap();
        assert_eq!(plan.nodes.len(), 1);
        assert!(matches!(
            plan.nodes[0].warnings.as_slice(),
            [PlanWarning::TableScan { table, schema }] if table == "t" && schema.is_empty()
        ));
        assert_eq!(plan.estimated_rows, 0.0);
    }

    #[test]
    fn malformed_documents_yield_typed_errors_not_panics() {
        assert!(parse_clickhouse_explain_json("not json").is_err());
        assert!(parse_clickhouse_explain_json("[]").is_err());
        assert!(parse_clickhouse_explain_json("[{}]").is_err());
        assert!(parse_clickhouse_explain_json("[{\"Plan\": 1}]").is_err());
    }
}
