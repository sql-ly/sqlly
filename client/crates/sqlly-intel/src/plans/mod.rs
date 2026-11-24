#![forbid(unsafe_code)]
pub mod analysis;
pub mod comparison;
pub mod context;
pub mod diagram;
pub mod dialects;
pub mod error;
pub mod explain_options;
pub mod findings;
pub mod glossary;
pub mod import;
pub mod index_ddl;
pub mod learn;
pub mod model;
pub mod parser;
pub mod phrases;
pub mod predicate;
pub mod projections;
pub mod schema;
pub mod story;
pub mod tree;
pub mod warning;

#[cfg(test)]
pub(crate) mod plan_fixtures;

pub use analysis::{
    EstimateDirection, NodeAnalysis, NodeFlags, NodeHeat, NodeMetrics, PlanAnalysis, PlanGroup,
    PlanGroupKind, PlanMetric, PlanObject, PlanObjectKind, analyze_plan, analyze_plan_as,
    analyze_plan_for, plan_engine, resolve_dialect,
};
pub use comparison::{
    DEFAULT_MAX_COMPARISON_NODES, OperatorDelta, OperatorDeltaKind, PlanChange, PlanChangeEffect,
    PlanChangeKind, PlanChangeReport, PlanComparison, PlanComparisonError, StatementChanges,
    compare_plans, explain_plan_changes, explain_statement_changes,
};
pub use context::{
    AliasSource, PlanContext, ResolvedAlias, analyze_plan_with_context, dialect_prints_aliases,
    plan_with_context, resolve_batch_aliases, resolve_plan_aliases,
};
pub use diagram::{
    DiagramEdge, DiagramGroup, DiagramNode, DiagramOptions, DiagramOrientation, PlanDiagram,
    edge_thickness, plan_diagram,
};
pub use dialects::{
    PlanDialect, capture_executes_statement, estimated_capture_sql, explain_capture_sql,
    oracle_actual_capture_sql, oracle_capture_sql, oracle_plan_rows_sql, parse_captured_plan,
    parse_clickhouse_explain_json, parse_duckdb_explain_json, parse_mysql_explain_analyze_tree,
    parse_mysql_explain_json, parse_oracle_plan_lines, parse_oracle_xplan_text,
    parse_pg_explain_json, parse_sqlite_query_plan_any, parse_sqlite_query_plan_rows,
    parse_sqlite_query_plan_text,
};
pub use error::{PlanError, PlanImportError, PlanResult};
pub use explain_options::{
    ExplainOptions, ExplainPreset, MySqlExplainOptions, PgExplainOptions, ServerVersion,
    explain_sql_with_options,
};
pub use findings::{
    FindingAlternative, FindingCategory, FindingConfidence, FindingImpact, FindingKind,
    FindingSeverity, ImpactBasis, OperatorRow, PlanFinding, findings_for_plan,
    findings_for_plan_with_context, findings_for_plan_with_schema, human_count, operator_rows,
};
pub use glossary::{
    OperatorCategory, OperatorEntry, OperatorInfo, all_operator_glossaries, engine_key,
    engine_key_for, operator_entry, operator_glossary, operator_glossary_for, operator_info,
    operator_info_for, operator_lookup, operator_lookup_for,
};
pub use import::{
    ImportedPlan, PlanConfidence, PlanDetection, PlanEngine, PlanFormat, PlanSniff,
    clean_plan_text, decode_plan_bytes, detect_plan_format, import_plan_document,
    import_plan_document_as, parse_plan_document, sniff_plan_document,
};
pub use index_ddl::{IndexDdl, IndexDdlDialect};
pub use learn::{
    LEARN_LINKS_JSON, LEARN_SITE_ORIGIN, LearnEntry, LearnLink, LearnLinkType, LearnSource,
    LearnTable, engine_links_for_finding, engine_links_for_operator, engine_links_for_operator_in,
    finding_without_link, learn_engine_key, learn_engine_key_for, learn_table, learn_url,
    links_for_finding, links_for_operator, links_for_operator_in,
};
pub use model::{
    ClientStatistics, DeadlockGraph, ExecutionPlan, ExecutionPlanVisualLayout, PlanBuffers,
    PlanEdge, PlanMemoryGrant, PlanNode, PlanParameter, PlanProperty, PlanSearchMatch, PlanSpill,
    PlanStatisticsUsage, PlanTableIo, PlanTrigger, PlanVisualEdge, PlanVisualNode,
};
pub use phrases::{
    NumberStyle, PlanPhrase, all_plan_phrases, human_cost, human_duration_ms, human_factor,
    human_number, human_share, human_size_kb, number_style, rows_phrase, with_number_style,
};
pub use predicate::{ExpressionNames, ReadablePredicate, readable_expression, readable_predicate};
pub use projections::{
    BreakdownEntry, FlameCell, FlameLayout, OperatorTableColumn, OperatorTableRow, PlanBreakdown,
    operator_table_columns, plan_breakdown, plan_flame_layout, plan_operator_table,
    sort_operator_table,
};
pub use schema::{
    SchemaColumn, SchemaContext, SchemaForeignKey, SchemaIndex, SchemaIndexColumn, SchemaNote,
    SchemaNoteReason, SchemaTable, TypeFamily,
};
pub use story::{
    BatchStory, PlanStory, StatementStory, StoryBottleneck, StoryNote, StoryNoteKind, StoryStep,
    batch_story, batch_story_with_context, plan_story,
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
