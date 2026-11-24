//! Engine-agnostic operator tree and the basic top-down diagram layout.
//!
//! These work straight from an [`ExecutionPlan`]. The richer views built on
//! the plan analysis live in [`crate::plans::diagram`] (the full diagram
//! model) and [`crate::plans::projections`] (operator table, flame chart,
//! breakdowns).

use std::collections::{HashMap, HashSet};

use crate::plans::diagram::{TidyInput, tidy_breadth};
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

/// The plan as a forest of indices into `plan.nodes`: roots are nodes that
/// are never a `to_node` (every node when the plan has no edges), children
/// follow edge order. Each node appears once even if a malformed plan lists
/// it under two parents or in a cycle.
struct Forest {
    roots: Vec<usize>,
    /// Pre-order: parents before children, siblings in edge order.
    order: Vec<usize>,
    children: Vec<Vec<usize>>,
    depth: Vec<usize>,
}

fn forest(plan: &ExecutionPlan) -> Forest {
    let count = plan.nodes.len();
    let position: HashMap<i32, usize> = plan
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id, index))
        .collect();
    let mut listed: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut is_child = vec![false; count];
    for edge in &plan.edges {
        if let (Some(&from), Some(&to)) =
            (position.get(&edge.from_node), position.get(&edge.to_node))
        {
            listed[from].push(to);
            is_child[to] = true;
        }
    }
    let mut roots = Vec::new();
    let mut order = Vec::with_capacity(count);
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut depth = vec![0usize; count];
    let mut seen = vec![false; count];
    let mut stack: Vec<usize> = Vec::new();
    for start in (0..count).filter(|index| !is_child[*index]) {
        if seen[start] {
            continue;
        }
        seen[start] = true;
        roots.push(start);
        stack.push(start);
        while let Some(node) = stack.pop() {
            order.push(node);
            let kids: Vec<usize> = listed[node]
                .iter()
                .copied()
                .filter(|child| !std::mem::replace(&mut seen[*child], true))
                .collect();
            for &child in kids.iter().rev() {
                depth[child] = depth[node] + 1;
                stack.push(child);
            }
            children[node] = kids;
        }
    }
    Forest {
        roots,
        order,
        children,
        depth,
    }
}

/// Pre-order rows for `plan`. Roots are nodes that are never a `to_node`;
/// when the plan has no edges every node is a root. Cycles cannot occur in a
/// parsed plan, and a malformed one is cut where it would repeat a node.
pub fn plan_tree_rows(plan: &ExecutionPlan) -> Vec<PlanTreeRow> {
    let forest = forest(plan);
    forest
        .order
        .iter()
        .map(|&index| {
            let node = &plan.nodes[index];
            PlanTreeRow {
                node_id: node.id,
                depth: forest.depth[index],
                has_children: !forest.children[index].is_empty(),
                title: display_op(node),
                cost_percent: cost_percent(plan, node),
                estimated_rows: node.estimated_rows,
                actual_rows: node.actual_rows,
                warning_count: node.warnings.len(),
            }
        })
        .collect()
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

/// The basic top-down diagram: fixed-size boxes, root at the top, parents
/// centred over their inputs. Subtrees are packed by outline (see
/// [`crate::plans::diagram`]), so wide and deep plans stay compact, and the
/// layout is linear in the number of nodes.
///
/// [`crate::plans::diagram::plan_diagram`] is the full model, with both
/// orientations, sized boxes, connectors scaled by rows, collapsing and
/// frames.
pub fn plan_diagram_layout(plan: &ExecutionPlan) -> PlanDiagramLayout {
    let forest = forest(plan);
    let count = plan.nodes.len();
    let sizes = vec![DIAGRAM_NODE_W; count];
    let block = vec![None; count];
    let breadth = tidy_breadth(&TidyInput {
        roots: &forest.roots,
        order: &forest.order,
        children: &forest.children,
        sizes: &sizes,
        block: &block,
        gap: DIAGRAM_H_GAP,
        centered: true,
    });
    let left = forest
        .order
        .iter()
        .map(|index| breadth[*index])
        .fold(f32::INFINITY, f32::min);
    let shift = if left.is_finite() {
        DIAGRAM_MARGIN - left
    } else {
        0.0
    };
    let nodes: Vec<PlanDiagramNode> = forest
        .order
        .iter()
        .map(|&index| {
            let node = &plan.nodes[index];
            PlanDiagramNode {
                node_id: node.id,
                title: display_op(node),
                cost_percent: cost_percent(plan, node),
                estimated_rows: node.estimated_rows,
                actual_rows: node.actual_rows,
                warning_count: node.warnings.len(),
                x: breadth[index] + shift,
                y: DIAGRAM_MARGIN + forest.depth[index] as f32 * (DIAGRAM_NODE_H + DIAGRAM_V_GAP),
            }
        })
        .collect();
    let edges = forest
        .order
        .iter()
        .flat_map(|&index| {
            let from_node = plan.nodes[index].id;
            forest.children[index]
                .iter()
                .map(move |&child| PlanDiagramEdge {
                    from_node,
                    to_node: plan.nodes[child].id,
                })
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
            ..Default::default()
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
            ..Default::default()
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
