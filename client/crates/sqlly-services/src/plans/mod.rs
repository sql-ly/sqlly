#![forbid(unsafe_code)]
pub mod comparison;
pub mod dialects;
pub mod error;
pub mod explain_options;
pub mod findings;
pub mod index_ddl;
pub mod model;
pub mod parser;
pub mod tree;
pub mod warning;

pub use comparison::{
    DEFAULT_MAX_COMPARISON_NODES, OperatorDelta, OperatorDeltaKind, PlanComparison,
    PlanComparisonError, compare_plans,
};
pub use dialects::{
    PlanDialect, capture_executes_statement, estimated_capture_sql, explain_capture_sql,
    oracle_actual_capture_sql, oracle_capture_sql, oracle_plan_rows_sql, parse_captured_plan,
    parse_clickhouse_explain_json, parse_duckdb_explain_json, parse_mysql_explain_analyze_tree,
    parse_mysql_explain_json, parse_oracle_plan_lines, parse_pg_explain_json,
    parse_sqlite_query_plan_rows, parse_sqlite_query_plan_text,
};
pub use error::{PlanError, PlanResult};
pub use explain_options::{
    ExplainOptions, ExplainPreset, MySqlExplainOptions, PgExplainOptions, ServerVersion,
    explain_sql_with_options,
};
pub use findings::{
    FindingSeverity, OperatorRow, PlanFinding, findings_for_plan, human_count, operator_rows,
};
pub use index_ddl::{IndexDdl, IndexDdlDialect};
pub use model::{
    ClientStatistics, DeadlockGraph, ExecutionPlan, ExecutionPlanVisualLayout, PlanEdge, PlanNode,
    PlanSearchMatch, PlanVisualEdge, PlanVisualNode,
};
pub use tree::{
    DIAGRAM_NODE_H, DIAGRAM_NODE_W, PlanDiagramEdge, PlanDiagramLayout, PlanDiagramNode,
    PlanTreeRow, plan_diagram_layout, plan_tree_rows, visible_tree_rows,
};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlanSummary {
    pub statement_count: usize,
    pub warnings: Vec<String>,
    pub highest_cost_operator: Option<String>,
    pub total_subtree_cost: f64,
}

pub fn summarize_showplan_xml(xml: &str) -> ExecutionPlanSummary {
    let plan = parser::PlanParser::new().parse_xml(xml).ok();
    ExecutionPlanSummary {
        statement_count: xml.matches("<StmtSimple").count() + xml.matches("<StmtCond").count(),
        warnings: plan
            .as_ref()
            .map(|plan| {
                plan.warnings
                    .iter()
                    .map(|warning| format!("{warning:?}"))
                    .collect()
            })
            .unwrap_or_default(),
        highest_cost_operator: plan.as_ref().and_then(|plan| {
            plan.nodes
                .iter()
                .max_by(|a, b| a.estimated_cost.total_cmp(&b.estimated_cost))
                .map(|node| node.physical_op.clone())
        }),
        total_subtree_cost: plan.map(|plan| plan.total_subtree_cost).unwrap_or_default(),
    }
}

pub fn render_showplan_xml(xml: &str) -> PlanResult<ExecutionPlanVisualLayout> {
    Ok(parser::PlanParser::new().parse_xml(xml)?.visual_layout())
}

/// Parse every statement in a ShowPlan document into its own plan. Batches
/// produce one plan per `<StmtSimple>`; single-statement documents produce a
/// one-element vector.
pub fn parse_showplan_xml_multi(xml: &str) -> PlanResult<Vec<ExecutionPlan>> {
    parser::PlanParser::new().parse_xml_multi(xml)
}

pub fn include_actual_plan_sql(sql: &str) -> String {
    format!("SET STATISTICS XML ON;\n{sql}\nSET STATISTICS XML OFF;")
}

/// SQL Server estimated-plan capture. `SET SHOWPLAN_XML` must be the ONLY
/// statement in its batch, so the setup and teardown are isolated behind
/// `GO` separators — one execute() runs three batches on one connection,
/// and the user SQL in the middle is compiled (returning plan XML), never
/// executed. The old single-batch form errored on every capture.
pub fn estimated_plan_sql(sql: &str) -> String {
    format!("SET SHOWPLAN_XML ON;\nGO\n{sql}\nGO\nSET SHOWPLAN_XML OFF;")
}
