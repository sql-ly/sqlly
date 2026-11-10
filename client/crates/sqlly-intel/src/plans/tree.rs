//! Engine-agnostic operator tree and top-down diagram layout.

use std::collections::{HashMap, HashSet};

use crate::plans::model::{ExecutionPlan, PlanNode};

/// One row of the indented operator tree, in pre-order (parent before its
/// children, siblings in edge order).
#[derive(Debug, Clone, PartialEq)]
pub struct PlanTreeRow {
    pub node_id: i32,
    pub depth: usize,
    pub has_children: bool,
    /// Physical operator (falls back to the logical one), same rule as `OperatorRow.title`.
    pub title: String,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub warning_count: usize,
}

/// A box in the top-down diagram, in abstract canvas units the paint pass
/// treats as px. Root at the top, children on the next row, subtrees packed
/// left to right without overlap.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanDiagramNode {
    pub node_id: i32,
    pub title: String,
    pub cost_percent: f64,
    pub estimated_rows: f64,
    pub actual_rows: Option<f64>,
    pub warning_count: usize,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlanDiagramEdge {
    pub from_node: i32,
    pub to_node: i32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanDiagramLayout {
    pub nodes: Vec<PlanDiagramNode>,
    pub edges: Vec<PlanDiagramEdge>,
    pub width: f32,
    pub height: f32,
}

pub const DIAGRAM_NODE_W: f32 = 168.0;
pub const DIAGRAM_NODE_H: f32 = 56.0;
pub const DIAGRAM_H_GAP: f32 = 24.0;
pub const DIAGRAM_V_GAP: f32 = 40.0;
pub const DIAGRAM_MARGIN: f32 = 24.0;

/// Pre-order rows for `plan`. Roots are nodes that are never a `to_node`;
/// when the plan has no edges every node is a root. Cycles cannot occur in a
/// parsed plan, but a visited set guards against a malformed one.
pub fn plan_tree_rows(plan: &ExecutionPlan) -> Vec<PlanTreeRow> {
    let children = children_map(plan);
    let node_by_id: HashMap<i32, &PlanNode> =
        plan.nodes.iter().map(|node| (node.id, node)).collect();
    let mut rows = Vec::new();
    let mut visited = HashSet::new();
    for root in roots(plan) {
        walk_tree(
            root,
            0,
            &children,
            &node_by_id,
            plan,
            &mut visited,
            &mut rows,
        );
    }
    rows
}

/// Rows with the subtrees of `collapsed` node ids removed (the collapsed
/// node itself stays).
pub fn visible_tree_rows(rows: &[PlanTreeRow], collapsed: &HashSet<i32>) -> Vec<PlanTreeRow> {
    let mut skip_deeper_than: Option<usize> = None;
    let mut visible = Vec::new();
    for row in rows {
        if let Some(depth) = skip_deeper_than {
            if row.depth > depth {
                continue;
            }
            skip_deeper_than = None;
        }
        visible.push(row.clone());
        if collapsed.contains(&row.node_id) && row.has_children {
            skip_deeper_than = Some(row.depth);
        }
    }
    visible
}

pub fn plan_diagram_layout(plan: &ExecutionPlan) -> PlanDiagramLayout {
    let children = children_map(plan);
    let node_by_id: HashMap<i32, &PlanNode> =
        plan.nodes.iter().map(|node| (node.id, node)).collect();
    let mut nodes = Vec::new();
    let mut visited = HashSet::new();
    let mut x = DIAGRAM_MARGIN;
    {
        let mut ctx = DiagramLayout {
            children: &children,
            node_by_id: &node_by_id,
            plan,
            visited: &mut visited,
            nodes: &mut nodes,
        };
        for root in roots(plan) {
            let width = layout_subtree(root, 0, x, &mut ctx);
            x += width + DIAGRAM_H_GAP;
        }
    }
    let placed: HashSet<i32> = nodes.iter().map(|node| node.node_id).collect();
    let edges = plan
        .edges
        .iter()
        .filter(|edge| placed.contains(&edge.from_node) && placed.contains(&edge.to_node))
        .map(|edge| PlanDiagramEdge {
            from_node: edge.from_node,
            to_node: edge.to_node,
        })
        .collect();
    let width = nodes
        .iter()
        .map(|node| node.x + DIAGRAM_NODE_W)
        .fold(DIAGRAM_MARGIN, f32::max)
        + DIAGRAM_MARGIN;
    let height = nodes
        .iter()
        .map(|node| node.y + DIAGRAM_NODE_H)
        .fold(DIAGRAM_MARGIN, f32::max)
        + DIAGRAM_MARGIN;
    PlanDiagramLayout {
        nodes,
        edges,
        width,
        height,
    }
}

fn children_map(plan: &ExecutionPlan) -> HashMap<i32, Vec<i32>> {
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for edge in &plan.edges {
        children
            .entry(edge.from_node)
            .or_default()
            .push(edge.to_node);
    }
    children
}

fn roots(plan: &ExecutionPlan) -> Vec<i32> {
    let children_ids: HashSet<i32> = plan.edges.iter().map(|edge| edge.to_node).collect();
    plan.nodes
        .iter()
        .map(|node| node.id)
        .filter(|id| !children_ids.contains(id))
        .collect()
}

fn walk_tree(
    id: i32,
    depth: usize,
    children: &HashMap<i32, Vec<i32>>,
    node_by_id: &HashMap<i32, &PlanNode>,
    plan: &ExecutionPlan,
    visited: &mut HashSet<i32>,
    rows: &mut Vec<PlanTreeRow>,
) {
    if !visited.insert(id) {
        return;
    }
    let Some(node) = node_by_id.get(&id) else {
        return;
    };
    let kids = children.get(&id).map(Vec::as_slice).unwrap_or(&[]);
    rows.push(PlanTreeRow {
        node_id: id,
        depth,
        has_children: !kids.is_empty(),
        title: display_op(node),
        cost_percent: cost_percent(plan, node),
        estimated_rows: node.estimated_rows,
        actual_rows: node.actual_rows,
        warning_count: node.warnings.len(),
    });
    for child in kids {
        walk_tree(*child, depth + 1, children, node_by_id, plan, visited, rows);
    }
}

struct DiagramLayout<'a> {
    children: &'a HashMap<i32, Vec<i32>>,
    node_by_id: &'a HashMap<i32, &'a PlanNode>,
    plan: &'a ExecutionPlan,
    visited: &'a mut HashSet<i32>,
    nodes: &'a mut Vec<PlanDiagramNode>,
}

fn layout_subtree(id: i32, depth: usize, x_left: f32, ctx: &mut DiagramLayout<'_>) -> f32 {
    if !ctx.visited.insert(id) {
        return DIAGRAM_NODE_W;
    }
    let Some(node) = ctx.node_by_id.get(&id).copied() else {
        return DIAGRAM_NODE_W;
    };
    let y = DIAGRAM_MARGIN + depth as f32 * (DIAGRAM_NODE_H + DIAGRAM_V_GAP);
    let kids = ctx.children.get(&id).cloned().unwrap_or_default();
    let span = if kids.is_empty() {
        DIAGRAM_NODE_W
    } else {
        let mut cursor = x_left;
        for child in kids {
            let width = layout_subtree(child, depth + 1, cursor, ctx);
            cursor += width + DIAGRAM_H_GAP;
        }
        (cursor - x_left - DIAGRAM_H_GAP).max(DIAGRAM_NODE_W)
    };
    let x = x_left + (span - DIAGRAM_NODE_W).max(0.0) / 2.0;
    ctx.nodes.push(PlanDiagramNode {
        node_id: id,
        title: display_op(node),
        cost_percent: cost_percent(ctx.plan, node),
        estimated_rows: node.estimated_rows,
        actual_rows: node.actual_rows,
        warning_count: node.warnings.len(),
        x,
        y,
    });
    span
}

fn display_op(node: &PlanNode) -> String {
    if node.physical_op.is_empty() {
        node.logical_op.clone()
    } else {
        node.physical_op.clone()
    }
}

fn cost_percent(plan: &ExecutionPlan, node: &PlanNode) -> f64 {
    if plan.total_subtree_cost > 0.0 {
        (node.estimated_cost / plan.total_subtree_cost * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::model::{PlanEdge, PlanNode};
    use crate::plans::{parse_pg_explain_json, parse_sqlite_query_plan_rows};
    use std::collections::HashSet;

    fn node(id: i32, cost: f64) -> PlanNode {
        PlanNode {
            id,
            logical_op: format!("Op{id}"),
            physical_op: format!("Op{id}"),
            estimated_rows: 1.0,
            estimated_cost: cost,
            estimated_cpu: 0.0,
            estimated_io: 0.0,
            actual_rows: None,
            actual_rewinds: None,
            actual_rebinds: None,
            row_count: None,
            output_list: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn plan_with(nodes: Vec<PlanNode>, edges: Vec<(i32, i32)>) -> ExecutionPlan {
        let total_subtree_cost = nodes
            .iter()
            .map(|n| n.estimated_cost)
            .fold(0.0_f64, f64::max);
        ExecutionPlan {
            statement_text: String::new(),
            statement_type: "SELECT".into(),
            nodes,
            edges: edges
                .into_iter()
                .map(|(from_node, to_node)| PlanEdge { from_node, to_node })
                .collect(),
            total_subtree_cost,
            estimated_rows: 1.0,
            warnings: Vec::new(),
        }
    }

    /// Root 1 → {2 → {4}, 3}.
    fn branched_plan() -> ExecutionPlan {
        plan_with(
            vec![node(1, 10.0), node(2, 4.0), node(3, 3.0), node(4, 1.0)],
            vec![(1, 2), (1, 3), (2, 4)],
        )
    }

    #[test]
    fn pre_order_rows_follow_edges_and_carry_depth() {
        let rows = plan_tree_rows(&branched_plan());
        assert_eq!(
            rows.iter().map(|r| r.node_id).collect::<Vec<_>>(),
            vec![1, 2, 4, 3]
        );
        assert_eq!(
            rows.iter().map(|r| r.depth).collect::<Vec<_>>(),
            vec![0, 1, 2, 1]
        );
        assert_eq!(
            rows.iter().map(|r| r.has_children).collect::<Vec<_>>(),
            vec![true, true, false, false]
        );
    }

    #[test]
    fn plan_without_edges_is_a_flat_list_of_roots() {
        let plan = plan_with(vec![node(1, 1.0), node(2, 1.0), node(3, 1.0)], vec![]);
        let rows = plan_tree_rows(&plan);
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.depth == 0 && !r.has_children));
        assert_eq!(
            rows.iter().map(|r| r.node_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn collapsing_a_node_hides_its_subtree_only() {
        let rows = plan_tree_rows(&branched_plan());
        let collapsed = HashSet::from([2]);
        let visible = visible_tree_rows(&rows, &collapsed);
        assert_eq!(
            visible.iter().map(|r| r.node_id).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn diagram_layout_has_no_overlapping_siblings_and_root_on_top() {
        let layout = plan_diagram_layout(&branched_plan());
        let by_id: HashMap<i32, &PlanDiagramNode> =
            layout.nodes.iter().map(|n| (n.node_id, n)).collect();
        let root = by_id.get(&1).expect("root");
        assert_eq!(root.y, DIAGRAM_MARGIN);
        let child_y = root.y + DIAGRAM_NODE_H + DIAGRAM_V_GAP;
        let two = by_id.get(&2).expect("node 2");
        let three = by_id.get(&3).expect("node 3");
        assert_eq!(two.y, child_y);
        assert_eq!(three.y, child_y);
        for (a, b) in layout.nodes.iter().enumerate().flat_map(|(i, a)| {
            layout.nodes[i + 1..]
                .iter()
                .filter(|b| (a.y - b.y).abs() < f32::EPSILON)
                .map(move |b| (a, b))
        }) {
            assert!(
                (a.x - b.x).abs() >= DIAGRAM_NODE_W + DIAGRAM_H_GAP,
                "siblings overlap: {} @ {} and {} @ {}",
                a.node_id,
                a.x,
                b.node_id,
                b.x
            );
        }
    }

    #[test]
    fn pg_json_fixture_builds_a_tree() {
        let doc = r#"[{"Plan": {"Node Type": "Nested Loop", "Total Cost": 100.0, "Plan Rows": 10,
            "Plans": [
              {"Node Type": "Seq Scan", "Relation Name": "a", "Total Cost": 30.0, "Plan Rows": 5},
              {"Node Type": "Index Scan", "Relation Name": "b", "Index Name": "b_pk", "Total Cost": 40.0, "Plan Rows": 5}
            ]}}]"#;
        let plan = parse_pg_explain_json(doc).unwrap();
        let rows = plan_tree_rows(&plan);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].title, "Nested Loop");
        assert_eq!(rows[0].depth, 0);
        assert_eq!(rows[1].depth, 1);
        assert_eq!(rows[2].depth, 1);
        assert!(rows[1].title.contains("Scan"));
        assert!(rows[2].title.contains("Scan"));
    }

    #[test]
    fn sqlite_forest_yields_multiple_roots() {
        let plan = parse_sqlite_query_plan_rows(&[
            (2, 0, "SCAN a".into()),
            (3, 0, "SEARCH b USING INDEX ix_b".into()),
        ])
        .unwrap();
        let rows = plan_tree_rows(&plan);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.depth == 0));
        assert_eq!(
            rows.iter().map(|r| r.node_id).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }
}
