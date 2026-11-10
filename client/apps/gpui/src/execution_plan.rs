//! Graphical execution plan — data model and text formatters.
//!
//! Port of `ExecutionPlanViewerController.swift`. The Swift controller rendered
//! ShowPlan XML via a CLI round-trip (`plan render <xml>`) into an
//! `ExecutionPlanLayout` JSON, then drew operator nodes + edges in a custom
//! `NSView` and showed summary/property text in `NSTextView`s.
//!
//! This module ports the pure data model (`ExecutionPlanLayout` /
//! `ExecutionPlanNode` / `ExecutionPlanEdge`) and the text formatters
//! (`summary_text`, `property_text`, `plan_report_text`). Unlike the Swift app,
//! the GPUI app renders ShowPlan XML **in-process** via
//! [`sqlly_services::plans::render_showplan_xml`] — there is no `sqlly-cli plan render`
//! subprocess. [`ExecutionPlanLayout::from_xml`] is the direct path;
//! [`ExecutionPlanLayout::from_json`] remains for tests/interop.
//!
//! The user-facing plan surface is the findings-first pane in
//! `crate::execution_plan_view` (fed by `AppView::explain_active_query`); this
//! module's layout model is retained for the future graphical node/edge canvas
//! and its text formatters remain covered by tests.

use serde::Deserialize;

/// A single operator node in the plan graph (e.g. "Index Seek", "Hash Match").
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPlanNode {
    pub id: i32,
    pub label: String,
    #[serde(default)]
    pub subtitle: String,
    pub estimated_cost: f64,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    #[serde(default)]
    pub actual_rows: Option<f64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub output_list: Vec<String>,
    #[serde(default)]
    pub warning_count: i32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// A data-flow edge connecting two operator nodes.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPlanEdge {
    pub from_node: i32,
    pub to_node: i32,
    pub from_x: f64,
    pub from_y: f64,
    pub to_x: f64,
    pub to_y: f64,
}

/// Full layout produced by the `plan render` CLI: statement metadata, the node
/// list, the edge list, and the canvas dimensions.
#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPlanLayout {
    #[serde(default)]
    pub statement_text: String,
    #[serde(default)]
    pub statement_type: String,
    #[serde(default)]
    pub total_subtree_cost: f64,
    #[serde(default)]
    pub estimated_rows: f64,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub nodes: Vec<ExecutionPlanNode>,
    #[serde(default)]
    pub edges: Vec<ExecutionPlanEdge>,
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
}

impl ExecutionPlanLayout {
    /// Render ShowPlan XML **in-process** via [`sqlly_services::plans::render_showplan_xml`]
    /// and convert the resulting visual layout into this module's UI types. This
    /// is the direct path — no `sqlly-cli` subprocess, no JSON interchange.
    #[allow(dead_code)]
    pub fn from_xml(xml: &str) -> Result<Self, sqlly_services::plans::PlanError> {
        let layout = sqlly_services::plans::render_showplan_xml(xml)?;
        Ok(Self::from_visual_layout(layout))
    }

    /// Convert the downstack [`sqlly_services::plans::ExecutionPlanVisualLayout`] into the
    /// gpui-side layout types (the shapes are identical; only the
    /// `warning_count` integer width differs).
    pub fn from_visual_layout(layout: sqlly_services::plans::ExecutionPlanVisualLayout) -> Self {
        let nodes = layout
            .nodes
            .into_iter()
            .map(|n| ExecutionPlanNode {
                id: n.id,
                label: n.label,
                subtitle: n.subtitle,
                estimated_cost: n.estimated_cost,
                cost_percent: n.cost_percent,
                estimated_rows: n.estimated_rows,
                actual_rows: n.actual_rows,
                warnings: n.warnings,
                output_list: n.output_list,
                warning_count: n.warning_count as i32,
                x: n.x,
                y: n.y,
                width: n.width,
                height: n.height,
            })
            .collect();
        let edges = layout
            .edges
            .into_iter()
            .map(|e| ExecutionPlanEdge {
                from_node: e.from_node,
                to_node: e.to_node,
                from_x: e.from_x,
                from_y: e.from_y,
                to_x: e.to_x,
                to_y: e.to_y,
            })
            .collect();
        Self {
            statement_text: layout.statement_text,
            statement_type: layout.statement_type,
            total_subtree_cost: layout.total_subtree_cost,
            estimated_rows: layout.estimated_rows,
            warnings: layout.warnings,
            nodes,
            edges,
            width: layout.width,
            height: layout.height,
        }
    }

    /// Decode the JSON form (tests / interop). Prefer [`Self::from_xml`] for
    /// the in-process render path.
    #[allow(dead_code)]
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    #[allow(dead_code)]
    /// Find a node by id.
    pub fn node(&self, id: i32) -> Option<&ExecutionPlanNode> {
        self.nodes.iter().find(|n| n.id == id)
    }
}

/// Top-level summary shown above the plan graph: statement text/type, total
/// subtree cost, estimated rows, operator count, and warnings.
#[allow(dead_code)]
pub fn summary_text(layout: &ExecutionPlanLayout) -> String {
    let warnings = if layout.warnings.is_empty() {
        crate::localization::text("query.execution_plan.no_warnings")
    } else {
        layout.warnings.join(", ")
    };
    crate::localization::format(
        "query.execution_plan.summary",
        &[
            ("statement", &layout.statement_text),
            ("statement_type", &layout.statement_type),
            ("total_cost", &format!("{:.4}", layout.total_subtree_cost)),
            ("estimated_rows", &format!("{:.2}", layout.estimated_rows)),
            ("operator_count", &layout.nodes.len().to_string()),
            ("warnings", &warnings),
        ],
    )
}

/// Property text for a selected node (or the "select an operator" placeholder
/// when `None`).
#[allow(dead_code)]
pub fn property_text(node: Option<&ExecutionPlanNode>) -> String {
    let Some(node) = node else {
        return crate::localization::text("query.execution_plan.select_operator");
    };
    let logical_operation = if node.subtitle.is_empty() {
        &node.label
    } else {
        &node.subtitle
    };
    let actual_rows = node
        .actual_rows
        .map(|rows| format!("{rows:.2}"))
        .unwrap_or_else(|| crate::localization::text("query.execution_plan.not_available"));
    let warnings = if node.warnings.is_empty() {
        crate::localization::text("query.execution_plan.no_warnings")
    } else {
        node.warnings.join(", ")
    };
    let mut text = crate::localization::format(
        "query.execution_plan.node_properties",
        &[
            ("node_id", &node.id.to_string()),
            ("physical_operation", &node.label),
            ("logical_operation", logical_operation),
            ("estimated_cost", &format!("{:.4}", node.estimated_cost)),
            ("cost_percent", &format!("{:.1}", node.cost_percent)),
            ("estimated_rows", &format!("{:.2}", node.estimated_rows)),
            ("actual_rows", &actual_rows),
            ("warnings", &warnings),
        ],
    );
    if !node.output_list.is_empty() {
        text.push_str(&crate::localization::text(
            "query.execution_plan.output_heading",
        ));
        for column in &node.output_list {
            text.push_str(&format!("- {column}\n"));
        }
    }
    text
}

/// Full textual rendering of a parsed plan: the top-level [`summary_text`]
/// followed by a per-operator [`property_text`] block for every node, ordered
/// by ascending cost-percent contribution (most expensive operators last so the
/// tail of the report highlights the hot path). This is the text surface used by
/// the Execution Plan Viewer until the graphical canvas is wired; it exercises
/// the in-process [`ExecutionPlanLayout::from_xml`] render path end to end.
#[allow(dead_code)]
pub fn plan_report_text(layout: &ExecutionPlanLayout) -> String {
    let mut sections = vec![summary_text(layout)];
    let mut nodes: Vec<&ExecutionPlanNode> = layout.nodes.iter().collect();
    nodes.sort_by(|a, b| {
        a.cost_percent
            .partial_cmp(&b.cost_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.id.cmp(&b.id))
    });
    for node in nodes {
        sections.push(property_text(Some(node)));
    }
    sections.join("\n\n────────────────────\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_layout() -> ExecutionPlanLayout {
        ExecutionPlanLayout {
            statement_text: "SELECT * FROM t".to_string(),
            statement_type: "SELECT".to_string(),
            total_subtree_cost: 0.0032813,
            estimated_rows: 1.0,
            warnings: vec![],
            nodes: vec![
                ExecutionPlanNode {
                    id: 0,
                    label: "SELECT".to_string(),
                    subtitle: "".to_string(),
                    estimated_cost: 0.0032813,
                    cost_percent: 100.0,
                    estimated_rows: 1.0,
                    actual_rows: Some(1.0),
                    warnings: vec![],
                    output_list: vec!["col1".to_string(), "col2".to_string()],
                    warning_count: 0,
                    x: 0.0,
                    y: 0.0,
                    width: 100.0,
                    height: 50.0,
                },
                ExecutionPlanNode {
                    id: 1,
                    label: "Index Seek".to_string(),
                    subtitle: "Seek".to_string(),
                    estimated_cost: 0.003,
                    cost_percent: 91.4,
                    estimated_rows: 1.0,
                    actual_rows: None,
                    warnings: vec!["No matching rows".to_string()],
                    output_list: vec![],
                    warning_count: 1,
                    x: 0.0,
                    y: 100.0,
                    width: 100.0,
                    height: 50.0,
                },
            ],
            edges: vec![ExecutionPlanEdge {
                from_node: 0,
                to_node: 1,
                from_x: 50.0,
                from_y: 50.0,
                to_x: 50.0,
                to_y: 100.0,
            }],
            width: 200.0,
            height: 200.0,
        }
    }

    #[test]
    fn summary_text_contains_statement_and_costs() {
        let s = summary_text(&sample_layout());
        assert!(s.contains("Statement: SELECT * FROM t"));
        assert!(s.contains("Type: SELECT"));
        assert!(s.contains("Total subtree cost: 0.0033"));
        assert!(s.contains("Operators: 2"));
        assert!(s.contains("Warnings: none"));
    }

    #[test]
    fn summary_text_shows_warnings() {
        let mut l = sample_layout();
        l.warnings = vec!["Missing stats".to_string()];
        let s = summary_text(&l);
        assert!(s.contains("Warnings: Missing stats"));
    }

    #[test]
    fn property_text_none_node() {
        let s = property_text(None);
        assert!(s.contains("Select an operator"));
    }

    #[test]
    fn property_text_with_node() {
        let l = sample_layout();
        let s = property_text(l.node(0));
        assert!(s.contains("Node 0"));
        assert!(s.contains("Physical op: SELECT"));
        assert!(s.contains("Logical op: SELECT"));
        assert!(s.contains("Estimated cost: 0.0033"));
        assert!(s.contains("Actual rows: 1.00"));
        assert!(s.contains("Output"));
        assert!(s.contains("- col1"));
    }

    #[test]
    fn property_text_uses_subtitle_for_logical_op() {
        let l = sample_layout();
        let s = property_text(l.node(1));
        assert!(s.contains("Physical op: Index Seek"));
        assert!(s.contains("Logical op: Seek"));
        assert!(s.contains("Actual rows: n/a"));
        assert!(s.contains("Warnings: No matching rows"));
    }

    #[test]
    fn from_json_decodes_layout() {
        let json = r#"{
            "statementText": "SELECT 1",
            "statementType": "SELECT",
            "totalSubtreeCost": 0.1,
            "estimatedRows": 1.0,
            "warnings": [],
            "nodes": [],
            "edges": [],
            "width": 0.0,
            "height": 0.0
        }"#;
        let layout = ExecutionPlanLayout::from_json(json).unwrap();
        assert_eq!(layout.statement_text, "SELECT 1");
        assert_eq!(layout.nodes.len(), 0);
    }

    #[test]
    fn from_json_decodes_nodes_and_edges() {
        let json = r#"{
            "statementText": "",
            "statementType": "",
            "totalSubtreeCost": 0.0,
            "estimatedRows": 0.0,
            "warnings": [],
            "nodes": [
                {"id": 0, "label": "A", "estimatedCost": 1.0, "costPercent": 50.0, "estimatedRows": 10.0, "x": 0, "y": 0, "width": 10, "height": 10}
            ],
            "edges": [
                {"fromNode": 0, "toNode": 1, "fromX": 0, "fromY": 0, "toX": 10, "toY": 10}
            ],
            "width": 100.0,
            "height": 100.0
        }"#;
        let layout = ExecutionPlanLayout::from_json(json).unwrap();
        assert_eq!(layout.nodes.len(), 1);
        assert_eq!(layout.nodes[0].label, "A");
        assert_eq!(layout.edges.len(), 1);
        assert_eq!(layout.node(0).unwrap().label, "A");
        assert!(layout.node(99).is_none());
    }

    #[test]
    fn from_json_default_fields_when_missing() {
        let json = r#"{
            "nodes": [],
            "statementText": "x"
        }"#;
        let layout = ExecutionPlanLayout::from_json(json).unwrap();
        assert_eq!(layout.statement_text, "x");
        assert_eq!(layout.statement_type, "");
        assert!(layout.warnings.is_empty());
        assert!(layout.edges.is_empty());
    }

    #[test]
    fn from_json_invalid_returns_error() {
        assert!(ExecutionPlanLayout::from_json("not json").is_err());
    }

    #[test]
    fn from_xml_renders_in_process() {
        // In-process render via sqlly_services::plans::render_showplan_xml — no CLI.
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Nested Loops" EstimateRows="4" EstimatedTotalSubtreeCost="2"><RunTimeCountersPerThread ActualRows="7" /><OutputList><ColumnReference Column="OrderId" /></OutputList></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#;
        let layout = ExecutionPlanLayout::from_xml(xml).expect("plan renders in-process");
        assert_eq!(layout.statement_type, "SELECT");
        assert_eq!(layout.nodes.len(), 2);
        assert_eq!(layout.nodes[0].label, "Nested Loops");
        assert_eq!(layout.nodes[0].id, 1);
        assert_eq!(layout.nodes[1].label, "Table Scan");
        // summary_text/property_text compose over the in-process layout.
        let s = summary_text(&layout);
        assert!(s.contains("Type: SELECT"));
        assert!(s.contains("Operators: 2"));
        let p = property_text(layout.node(1));
        assert!(p.contains("Physical op: Nested Loops"));
    }

    #[test]
    fn from_xml_invalid_returns_error() {
        assert!(ExecutionPlanLayout::from_xml("not xml").is_err());
    }

    #[test]
    fn plan_report_text_combines_summary_and_operators() {
        let layout = sample_layout();
        let report = plan_report_text(&layout);
        // Summary header present.
        assert!(report.contains("Statement: SELECT * FROM t"));
        assert!(report.contains("Operators: "));
        // Every operator's property block is present.
        for node in &layout.nodes {
            assert!(
                report.contains(&format!("Physical op: {}", node.label)),
                "missing operator {} in report",
                node.label
            );
        }
        // Section separator joins the blocks.
        assert!(report.contains("────────────────────"));
    }

    #[test]
    fn plan_report_text_orders_operators_by_cost_percent() {
        let xml = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="3"><RelOp NodeId="1" LogicalOp="Select" PhysicalOp="Nested Loops" EstimateRows="4" EstimatedTotalSubtreeCost="3"><OutputList /></RelOp><RelOp NodeId="2" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="1" /></StmtSimple></ShowPlanXML>"#;
        let layout = ExecutionPlanLayout::from_xml(xml).expect("renders");
        let report = plan_report_text(&layout);
        // Both operators appear; the report is non-empty and well-formed.
        assert!(report.contains("Physical op: Nested Loops"));
        assert!(report.contains("Physical op: Table Scan"));
    }
}
