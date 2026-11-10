use crate::plans::warning::PlanWarning;
use serde::{Deserialize, Serialize};

const PLAN_NODE_WIDTH: f64 = 220.0;
const PLAN_NODE_HEIGHT: f64 = 92.0;
const PLAN_NODE_X_SPACING: f64 = 280.0;
const PLAN_NODE_Y: f64 = 48.0;
const PLAN_MARGIN: f64 = 48.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub statement_text: String,
    pub statement_type: String,
    pub nodes: Vec<PlanNode>,
    pub edges: Vec<PlanEdge>,
    pub total_subtree_cost: f64,
    pub estimated_rows: f64,
    pub warnings: Vec<PlanWarning>,
}

impl ExecutionPlan {
    pub fn text_tree(&self) -> String {
        self.nodes
            .iter()
            .map(|node| {
                format!(
                    "{}: {} / {} rows={} cost={}",
                    node.id,
                    node.logical_op,
                    node.physical_op,
                    node.estimated_rows,
                    node.estimated_cost
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn search(&self, query: &str) -> Vec<PlanSearchMatch> {
        let needle = query.to_ascii_lowercase();
        self.nodes
            .iter()
            .filter(|node| {
                node.logical_op.to_ascii_lowercase().contains(&needle)
                    || node.physical_op.to_ascii_lowercase().contains(&needle)
                    || node
                        .output_list
                        .iter()
                        .any(|output| output.to_ascii_lowercase().contains(&needle))
            })
            .map(|node| PlanSearchMatch {
                node_id: node.id,
                text: format!("{} {}", node.logical_op, node.physical_op),
            })
            .collect()
    }

    pub fn visual_layout(&self) -> ExecutionPlanVisualLayout {
        let nodes = self
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                let x = PLAN_MARGIN + (index as f64 * PLAN_NODE_X_SPACING);
                PlanVisualNode {
                    id: node.id,
                    label: if node.physical_op.is_empty() {
                        node.logical_op.clone()
                    } else {
                        node.physical_op.clone()
                    },
                    subtitle: if node.logical_op == node.physical_op {
                        String::new()
                    } else {
                        node.logical_op.clone()
                    },
                    estimated_cost: node.estimated_cost,
                    cost_percent: if self.total_subtree_cost > 0.0 {
                        (node.estimated_cost / self.total_subtree_cost * 100.0).max(0.0)
                    } else {
                        0.0
                    },
                    estimated_rows: node.estimated_rows,
                    actual_rows: node.actual_rows,
                    warning_count: node.warnings.len(),
                    output_list: node.output_list.clone(),
                    warnings: node
                        .warnings
                        .iter()
                        .map(|warning| format!("{warning:?}"))
                        .collect(),
                    x,
                    y: PLAN_NODE_Y,
                    width: PLAN_NODE_WIDTH,
                    height: PLAN_NODE_HEIGHT,
                }
            })
            .collect::<Vec<_>>();

        let edges = self
            .edges
            .iter()
            .filter_map(|edge| {
                let from = nodes.iter().find(|node| node.id == edge.from_node)?;
                let to = nodes.iter().find(|node| node.id == edge.to_node)?;
                Some(PlanVisualEdge {
                    from_node: edge.from_node,
                    to_node: edge.to_node,
                    from_x: from.x + from.width,
                    from_y: from.y + from.height / 2.0,
                    to_x: to.x,
                    to_y: to.y + to.height / 2.0,
                })
            })
            .collect::<Vec<_>>();

        let width = nodes
            .last()
            .map(|node| node.x + node.width + PLAN_MARGIN)
            .unwrap_or(PLAN_MARGIN * 2.0);
        ExecutionPlanVisualLayout {
            statement_text: self.statement_text.clone(),
            statement_type: self.statement_type.clone(),
            total_subtree_cost: self.total_subtree_cost,
            estimated_rows: self.estimated_rows,
            warnings: self
                .warnings
                .iter()
                .map(|warning| format!("{warning:?}"))
                .collect(),
            nodes,
            edges,
            width,
            height: PLAN_NODE_Y + PLAN_NODE_HEIGHT + PLAN_MARGIN,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanNode {
    pub id: i32,
    pub logical_op: String,
    pub physical_op: String,
    pub estimated_rows: f64,
    pub estimated_cost: f64,
    pub estimated_cpu: f64,
    pub estimated_io: f64,
    pub actual_rows: Option<f64>,
    pub actual_rewinds: Option<i32>,
    pub actual_rebinds: Option<i32>,
    pub row_count: Option<i32>,
    pub output_list: Vec<String>,
    pub warnings: Vec<PlanWarning>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanEdge {
    pub from_node: i32,
    pub to_node: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanSearchMatch {
    pub node_id: i32,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlanVisualLayout {
    pub statement_text: String,
    pub statement_type: String,
    pub total_subtree_cost: f64,
    pub estimated_rows: f64,
    pub warnings: Vec<String>,
    pub nodes: Vec<PlanVisualNode>,
    pub edges: Vec<PlanVisualEdge>,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanVisualNode {
    pub id: i32,
    pub label: String,
    pub subtitle: String,
    pub estimated_cost: f64,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub warning_count: usize,
    pub output_list: Vec<String>,
    pub warnings: Vec<String>,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanVisualEdge {
    pub from_node: i32,
    pub to_node: i32,
    pub from_x: f64,
    pub from_y: f64,
    pub to_x: f64,
    pub to_y: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStatistics {
    pub server_round_trips: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub client_processing_ms: u128,
    pub total_execution_ms: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlockGraph {
    pub victim_process: Option<String>,
    pub processes: Vec<String>,
    pub resources: Vec<String>,
}

impl DeadlockGraph {
    pub fn parse(xml: &str) -> Self {
        Self {
            victim_process: attr(xml, "victim", "id"),
            processes: collect_attr(xml, "process", "id"),
            resources: collect_resource_names(xml),
        }
    }
}

fn attr(xml: &str, element: &str, name: &str) -> Option<String> {
    let start = xml.find(&format!("<{element}"))?;
    let rest = &xml[start..];
    let end = rest.find('>')?;
    let tag = &rest[..end];
    let prefix = format!("{name}=\"");
    let value_start = tag.find(&prefix)? + prefix.len();
    let value = &tag[value_start..];
    Some(value[..value.find('"')?].to_string())
}

fn collect_attr(xml: &str, element: &str, name: &str) -> Vec<String> {
    xml.match_indices(&format!("<{element}"))
        .filter_map(|(start, _)| {
            let rest = &xml[start..];
            let tag = &rest[..rest.find('>')?];
            let prefix = format!("{name}=\"");
            let value_start = tag.find(&prefix)? + prefix.len();
            let value = &tag[value_start..];
            Some(value[..value.find('"')?].to_string())
        })
        .collect()
}

fn collect_resource_names(xml: &str) -> Vec<String> {
    [
        "keylock",
        "pagelock",
        "objectlock",
        "ridlock",
        "exchangeEvent",
    ]
    .into_iter()
    .flat_map(|name| collect_attr(xml, name, "objectname"))
    .collect()
}
