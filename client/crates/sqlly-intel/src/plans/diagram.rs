//! The plan diagram as pure data: boxes, connectors and frames in abstract
//! canvas units a renderer treats as pixels.
//!
//! The layout is a tidy tree. Subtrees are packed against each other by
//! their outlines, so a deep narrow branch can sit under a wide shallow
//! neighbour, and no two boxes or frames overlap. It runs in time linear in
//! the number of nodes and never recurses, so plans with thousands of
//! operators and plans that are thousands of levels deep both lay out.
//!
//! Two orientations are offered. [`DiagramOrientation::RootTop`] puts the
//! root at the top with parents centred over their inputs (the pev2 look).
//! [`DiagramOrientation::RootLeft`] puts the root at the left with inputs to
//! its right and each parent level with its first input, so data flows right
//! to left the way SQL Server Management Studio draws it.

use std::collections::HashSet;

use sqlly_core::DisplayMessage;

use crate::plans::analysis::{
    NodeAnalysis, NodeFlags, NodeHeat, PlanAnalysis, PlanGroupKind, PlanMetric,
};
use crate::plans::glossary::OperatorCategory;
use crate::plans::model::ExecutionPlan;
use crate::plans::phrases::{
    PlanPhrase, human_duration_ms, human_number, human_share, phrase, rows_phrase, say, say0,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DiagramOrientation {
    /// Root at the top, inputs below.
    #[default]
    RootTop,
    /// Root at the left, inputs to the right.
    RootLeft,
}

impl DiagramOrientation {
    pub fn key(self) -> &'static str {
        match self {
            Self::RootTop => "root_top",
            Self::RootLeft => "root_left",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        match key {
            "root_top" => Some(Self::RootTop),
            "root_left" => Some(Self::RootLeft),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DiagramOptions {
    pub orientation: DiagramOrientation,
    /// Nodes whose inputs are folded away. A collapsed node stays and
    /// carries a summary of what it hides.
    pub collapsed: HashSet<i32>,
    /// Metric `DiagramNode::heat` reports. `None` uses the plan's primary
    /// metric (time when timed, else cost).
    pub metric: Option<PlanMetric>,
    /// Box width.
    pub node_width: f32,
    /// Give every box the same height instead of sizing it to its lines.
    pub uniform_height: bool,
    /// Reserve room for, and return, frames around subplans, CTEs and
    /// parallel zones.
    pub groups: bool,
}

impl Default for DiagramOptions {
    fn default() -> Self {
        Self {
            orientation: DiagramOrientation::default(),
            collapsed: HashSet::new(),
            metric: None,
            node_width: DIAGRAM_BOX_W,
            uniform_height: false,
            groups: true,
        }
    }
}

/// Default box width.
pub const DIAGRAM_BOX_W: f32 = 184.0;
/// Height of a box with only a title line.
pub const DIAGRAM_BOX_BASE_H: f32 = 40.0;
/// Height each further line (object, numbers, hidden-steps summary) adds.
pub const DIAGRAM_BOX_LINE_H: f32 = 16.0;
/// Height of every box when `uniform_height` is set.
pub const DIAGRAM_BOX_UNIFORM_H: f32 = 72.0;
/// Space around the whole drawing.
pub const DIAGRAM_CANVAS_MARGIN: f32 = 24.0;
/// Thinnest and thickest connector.
pub const DIAGRAM_EDGE_MIN: f32 = 1.0;
pub const DIAGRAM_EDGE_MAX: f32 = 10.0;
/// In the root-left orientation connectors meet a box this far below its
/// top edge (the middle of the title line), so the connector to a first
/// input is a straight line whatever the two heights are.
pub const DIAGRAM_SIDE_ANCHOR: f32 = 20.0;

const TOP_SIBLING_GAP: f32 = 24.0;
const TOP_LAYER_GAP: f32 = 48.0;
const LEFT_SIBLING_GAP: f32 = 16.0;
const LEFT_LAYER_GAP: f32 = 72.0;
const GROUP_PAD: f32 = 10.0;
/// Room for a frame's label, on the side the label sits.
const GROUP_LABEL_PAD: f32 = 24.0;

/// One operator box.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagramNode {
    pub node_id: i32,
    pub parent_id: Option<i32>,
    pub depth: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// Operator name as the engine prints it.
    pub title: String,
    /// Table and index the operator touches.
    pub subtitle: Option<String>,
    /// Rows and own time, ready to print: `8,210 rows · 84 ms`.
    pub stats: Option<DisplayMessage>,
    /// Own share of the plan's time (cost on untimed plans), 0..1.
    pub share: Option<f64>,
    /// `share` formatted: `46%`.
    pub share_label: Option<String>,
    pub category: OperatorCategory,
    /// 0..1 intensity for the metric the options asked for.
    pub heat: f32,
    /// Intensity for every metric, so a renderer can switch without a new
    /// layout.
    pub heat_by_metric: NodeHeat,
    pub flags: NodeFlags,
    pub warning_count: usize,
    pub has_children: bool,
    /// The node's inputs are folded away.
    pub collapsed: bool,
    /// Operators hidden below a collapsed node.
    pub hidden_count: usize,
    /// What a collapsed node hides: `12 steps hidden, 31% of the time`.
    pub summary: Option<DisplayMessage>,
    /// Index into [`PlanDiagram::groups`] of the innermost frame around
    /// this node.
    pub group: Option<usize>,
}

/// The connector from an operator to one of its inputs.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagramEdge {
    /// The consuming operator (nearer the root).
    pub from_node: i32,
    /// The input that produces the rows.
    pub to_node: i32,
    /// Rows that flow along the connector: actual when the plan was
    /// executed, else the estimate.
    pub rows: Option<f64>,
    /// `rows` is an estimate.
    pub rows_estimated: bool,
    /// `rows` formatted the way the input's own box states them: `8,210`,
    /// `1.2M`, and never below 1 for an estimate (the box says "about 1
    /// row" for an optimizer guess under one).
    pub rows_label: Option<String>,
    /// Stroke width, log-scaled by rows between [`DIAGRAM_EDGE_MIN`] and
    /// [`DIAGRAM_EDGE_MAX`].
    pub thickness: f32,
    /// Right-angled polyline from the consumer to the input.
    pub points: Vec<(f32, f32)>,
    /// Where the row label goes: the middle of the segment that enters the
    /// input.
    pub label_x: f32,
    pub label_y: f32,
    /// Both ends are on the heaviest root-to-leaf chain.
    pub on_critical_path: bool,
    /// The input never ran.
    pub never_executed: bool,
}

/// A frame around a subplan, a CTE definition or a parallel zone.
#[derive(Debug, Clone, PartialEq)]
pub struct DiagramGroup {
    pub kind: PlanGroupKind,
    pub label: DisplayMessage,
    pub root_id: i32,
    /// Visible members.
    pub node_ids: Vec<i32>,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlanDiagram {
    pub orientation: DiagramOrientation,
    /// Visible boxes, in plan order (parents before inputs). The order does
    /// not change when nodes are collapsed or expanded.
    pub nodes: Vec<DiagramNode>,
    pub edges: Vec<DiagramEdge>,
    pub groups: Vec<DiagramGroup>,
    pub width: f32,
    pub height: f32,
    /// Metric the `heat` values are for.
    pub metric: Option<PlanMetric>,
}

impl PlanDiagram {
    pub fn node(&self, node_id: i32) -> Option<&DiagramNode> {
        self.nodes.iter().find(|node| node.node_id == node_id)
    }
}

/// Lay out `plan` using the facts in `analysis`.
pub fn plan_diagram(
    plan: &ExecutionPlan,
    analysis: &PlanAnalysis,
    options: &DiagramOptions,
) -> PlanDiagram {
    let count = analysis.nodes.len();
    let metric = options.metric.unwrap_or(analysis.primary_metric);
    let top = options.orientation == DiagramOrientation::RootTop;
    let width = if options.node_width > 0.0 {
        options.node_width
    } else {
        DIAGRAM_BOX_W
    };

    // ---- Visible forest ----------------------------------------------------
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut visible: Vec<usize> = Vec::with_capacity(count);
    let roots: Vec<usize> = analysis
        .roots
        .iter()
        .filter_map(|id| analysis.position(*id))
        .collect();
    let mut seen = vec![false; count];
    let mut stack: Vec<usize> = roots.iter().rev().copied().collect();
    while let Some(position) = stack.pop() {
        if std::mem::replace(&mut seen[position], true) {
            continue;
        }
        visible.push(position);
        let node = &analysis.nodes[position];
        if options.collapsed.contains(&node.node_id) {
            continue;
        }
        let kids: Vec<usize> = node
            .children
            .iter()
            .filter_map(|id| analysis.position(*id))
            .filter(|child| !seen[*child])
            .collect();
        for child in kids.iter().rev() {
            stack.push(*child);
        }
        children[position] = kids;
    }

    // ---- Box contents and sizes ----------------------------------------------
    let mut boxes: Vec<Option<DiagramNode>> = vec![None; count];
    for &position in &visible {
        let node = &analysis.nodes[position];
        let collapsed = options.collapsed.contains(&node.node_id) && !node.children.is_empty();
        let hidden_count = if collapsed {
            node.subtree_size.saturating_sub(1)
        } else {
            0
        };
        // The share follows the metric the diagram is coloured by, where
        // that metric has one; rows and estimate error do not.
        let share = match metric {
            PlanMetric::Time => node.metrics.time_share,
            PlanMetric::Cost => node.metrics.cost_share,
            PlanMetric::Io => node.metrics.io_share,
            PlanMetric::Rows | PlanMetric::EstimateError => primary_share(analysis, node).0,
        };
        let subtitle = subtitle(node);
        let stats = stats(analysis, node);
        let summary = collapsed.then(|| collapsed_summary(analysis, node, hidden_count));
        let lines = 1
            + usize::from(subtitle.is_some())
            + usize::from(stats.is_some())
            + usize::from(summary.is_some());
        let height = if options.uniform_height {
            DIAGRAM_BOX_UNIFORM_H
        } else {
            DIAGRAM_BOX_BASE_H + DIAGRAM_BOX_LINE_H * (lines as f32 - 1.0)
        };
        boxes[position] = Some(DiagramNode {
            node_id: node.node_id,
            parent_id: node.parent_id,
            depth: node.depth,
            x: 0.0,
            y: 0.0,
            width,
            height,
            title: node.operator.name.clone(),
            subtitle,
            stats,
            share,
            share_label: share.map(human_share),
            category: node.category(),
            heat: node.metrics.heat.get(metric),
            heat_by_metric: node.metrics.heat,
            flags: node.flags,
            warning_count: plan
                .nodes
                .get(position)
                .map(|raw| raw.warnings.len())
                .unwrap_or(0),
            has_children: !node.children.is_empty(),
            collapsed,
            hidden_count,
            summary,
            group: None,
        });
    }

    // ---- Groups ----------------------------------------------------------------
    // A frame needs clear space around its whole subtree, so the top of each
    // group is laid out as a solid block.
    let mut block: Vec<Option<BlockPad>> = vec![None; count];
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    if options.groups {
        for (index, group) in analysis.groups.iter().enumerate() {
            let Some(root) = analysis.position(group.root_id) else {
                continue;
            };
            if !seen[root] {
                continue;
            }
            let members: Vec<usize> = group
                .node_ids
                .iter()
                .filter_map(|id| analysis.position(*id))
                .filter(|position| seen[*position])
                .collect();
            if members.len() < 2 {
                continue;
            }
            block[root] = Some(if top {
                BlockPad {
                    before: GROUP_PAD,
                    after: GROUP_PAD,
                }
            } else {
                BlockPad {
                    before: GROUP_LABEL_PAD,
                    after: GROUP_PAD,
                }
            });
            groups.push((index, members));
        }
    }

    // ---- Breadth axis: tidy tree --------------------------------------------------
    let sizes: Vec<f32> = boxes
        .iter()
        .map(|node| match node {
            Some(node) if top => node.width,
            Some(node) => node.height,
            None => 0.0,
        })
        .collect();
    let breadth = tidy_breadth(&TidyInput {
        roots: &roots,
        order: &visible,
        children: &children,
        sizes: &sizes,
        block: &block,
        gap: if top {
            TOP_SIBLING_GAP
        } else {
            LEFT_SIBLING_GAP
        },
        centered: top,
    });

    // ---- Depth axis: one layer per depth ---------------------------------------------
    let max_depth = visible
        .iter()
        .map(|position| analysis.nodes[*position].depth)
        .max()
        .unwrap_or(0);
    let mut layer_size = vec![0.0_f32; max_depth + 1];
    for &position in &visible {
        let Some(node) = &boxes[position] else {
            continue;
        };
        let size = if top { node.height } else { node.width };
        let depth = analysis.nodes[position].depth;
        layer_size[depth] = layer_size[depth].max(size);
    }
    let layer_gap = if top { TOP_LAYER_GAP } else { LEFT_LAYER_GAP };
    let mut layer_start = Vec::with_capacity(max_depth + 1);
    let mut cursor = DIAGRAM_CANVAS_MARGIN;
    if options.groups && !groups.is_empty() && top {
        // Room for a frame label above a group that starts at a root.
        cursor += GROUP_LABEL_PAD;
    }
    for size in &layer_size {
        layer_start.push(cursor);
        cursor += size + layer_gap;
    }

    let min_breadth = visible
        .iter()
        .map(|position| breadth[*position])
        .fold(f32::INFINITY, f32::min);
    let group_room = if options.groups && !groups.is_empty() {
        if top { GROUP_PAD } else { GROUP_LABEL_PAD }
    } else {
        0.0
    };
    let breadth_shift = if min_breadth.is_finite() {
        DIAGRAM_CANVAS_MARGIN + group_room - min_breadth
    } else {
        0.0
    };
    for &position in &visible {
        let depth = analysis.nodes[position].depth;
        if let Some(node) = boxes[position].as_mut() {
            let along = breadth[position] + breadth_shift;
            let across = layer_start[depth];
            if top {
                node.x = along;
                node.y = across;
            } else {
                node.x = across;
                node.y = along;
            }
        }
    }

    // ---- Frames -------------------------------------------------------------------------
    let mut frames = Vec::with_capacity(groups.len());
    for (index, members) in &groups {
        let group = &analysis.groups[*index];
        let mut left = f32::INFINITY;
        let mut right = f32::NEG_INFINITY;
        let mut top_edge = f32::INFINITY;
        let mut bottom = f32::NEG_INFINITY;
        for position in members {
            let Some(node) = &boxes[*position] else {
                continue;
            };
            left = left.min(node.x);
            right = right.max(node.x + node.width);
            top_edge = top_edge.min(node.y);
            bottom = bottom.max(node.y + node.height);
        }
        if !left.is_finite() {
            continue;
        }
        let frame_index = frames.len();
        for position in members {
            // Groups are listed outermost first, so the last writer is the
            // innermost frame.
            if let Some(node) = boxes[*position].as_mut() {
                node.group = Some(frame_index);
            }
        }
        frames.push(DiagramGroup {
            kind: group.kind,
            label: group.label.clone(),
            root_id: group.root_id,
            node_ids: members
                .iter()
                .map(|position| analysis.nodes[*position].node_id)
                .collect(),
            x: left - GROUP_PAD,
            y: top_edge - GROUP_LABEL_PAD,
            width: right - left + GROUP_PAD * 2.0,
            height: bottom - top_edge + GROUP_LABEL_PAD + GROUP_PAD,
        });
    }

    // ---- Connectors ------------------------------------------------------------------------
    let max_rows = analysis.max_rows.max(0.0);
    let mut edges = Vec::with_capacity(visible.len().saturating_sub(roots.len()));
    for &position in &visible {
        let Some(parent) = &boxes[position] else {
            continue;
        };
        for &child_position in &children[position] {
            let Some(child) = &boxes[child_position] else {
                continue;
            };
            let child_analysis = &analysis.nodes[child_position];
            let metrics = &child_analysis.metrics;
            let (rows, rows_estimated) = match metrics.rows_out {
                Some(rows) => (Some(rows), false),
                None => (
                    metrics
                        .estimated_rows_total
                        .filter(|_| analysis.has_estimates),
                    true,
                ),
            };
            let rows_estimated = rows.is_some() && rows_estimated;
            let (points, label_x, label_y) = if top {
                let start = (parent.x + parent.width / 2.0, parent.y + parent.height);
                let end = (child.x + child.width / 2.0, child.y);
                let bend = child.y - layer_gap / 2.0;
                (
                    vec![start, (start.0, bend), (end.0, bend), end],
                    end.0,
                    (bend + end.1) / 2.0,
                )
            } else {
                let start = (parent.x + parent.width, parent.y + DIAGRAM_SIDE_ANCHOR);
                let end = (child.x, child.y + DIAGRAM_SIDE_ANCHOR);
                let bend = child.x - layer_gap / 2.0;
                (
                    vec![start, (bend, start.1), (bend, end.1), end],
                    (bend + end.0) / 2.0,
                    end.1,
                )
            };
            edges.push(DiagramEdge {
                from_node: parent.node_id,
                to_node: child.node_id,
                rows,
                rows_estimated,
                rows_label: rows.map(|rows| edge_rows_label(rows, rows_estimated)),
                thickness: edge_thickness(rows, max_rows),
                points,
                label_x,
                label_y,
                on_critical_path: parent.flags.on_critical_path && child.flags.on_critical_path,
                never_executed: child.flags.never_executed,
            });
        }
    }

    let nodes: Vec<DiagramNode> = {
        // Plan order, which `visible` (a pre-order walk) already is for a
        // well-formed plan; sorting by position keeps it stable regardless.
        let mut order = visible.clone();
        order.sort_unstable();
        order
            .into_iter()
            .filter_map(|position| boxes[position].take())
            .collect()
    };
    let mut canvas_width = DIAGRAM_CANVAS_MARGIN;
    let mut canvas_height = DIAGRAM_CANVAS_MARGIN;
    for node in &nodes {
        canvas_width = canvas_width.max(node.x + node.width);
        canvas_height = canvas_height.max(node.y + node.height);
    }
    for frame in &frames {
        canvas_width = canvas_width.max(frame.x + frame.width);
        canvas_height = canvas_height.max(frame.y + frame.height);
    }
    PlanDiagram {
        orientation: options.orientation,
        nodes,
        edges,
        groups: frames,
        width: canvas_width + DIAGRAM_CANVAS_MARGIN,
        height: canvas_height + DIAGRAM_CANVAS_MARGIN,
        metric: Some(metric),
    }
}

/// Stroke width for a connector carrying `rows`, log-scaled against the
/// busiest connector so one huge scan does not flatten every other line.
pub fn edge_thickness(rows: Option<f64>, max_rows: f64) -> f32 {
    let Some(rows) = rows.filter(|rows| *rows > 0.0) else {
        return DIAGRAM_EDGE_MIN;
    };
    if max_rows <= 0.0 {
        return DIAGRAM_EDGE_MIN;
    }
    let scale = ((1.0 + rows).ln() / (1.0 + max_rows.max(rows)).ln()).clamp(0.0, 1.0);
    DIAGRAM_EDGE_MIN + (DIAGRAM_EDGE_MAX - DIAGRAM_EDGE_MIN) * scale as f32
}

/// A node's own share of the plan by the plan's primary metric.
pub(crate) fn primary_share(
    analysis: &PlanAnalysis,
    node: &NodeAnalysis,
) -> (Option<f64>, PlanMetric) {
    match analysis.primary_metric {
        PlanMetric::Time => (node.metrics.time_share, PlanMetric::Time),
        _ => (node.metrics.cost_share, PlanMetric::Cost),
    }
}

fn subtitle(node: &NodeAnalysis) -> Option<String> {
    match (&node.object, &node.index) {
        (Some(object), Some(index)) => Some(format!("{} · {index}", object.label())),
        (Some(object), None) => Some(object.label()),
        (None, Some(index)) => Some(index.clone()),
        (None, None) => None,
    }
}

/// The number on a connector. It is the figure the input's box shows
/// ([`stats`]): the same rounding, and an estimate is never shown below one
/// row, so a box that says "about 1 row" never feeds a line labelled 0.
fn edge_rows_label(rows: f64, estimated: bool) -> String {
    human_number(if estimated { rows.max(1.0) } else { rows })
}

fn stats(analysis: &PlanAnalysis, node: &NodeAnalysis) -> Option<DisplayMessage> {
    if node.flags.never_executed {
        return Some(say0("diagram.stats.never_ran"));
    }
    let metrics = &node.metrics;
    match (metrics.rows_out, metrics.self_time_ms) {
        (Some(rows), Some(time)) => Some(say(
            "diagram.stats.rows_time",
            vec![
                ("rows", rows_phrase(rows).into()),
                ("time", human_duration_ms(time).into()),
            ],
        )),
        (Some(rows), None) => Some(rows_phrase(rows)),
        (None, Some(time)) => Some(DisplayMessage::raw(human_duration_ms(time))),
        (None, None) => metrics
            .estimated_rows_total
            .filter(|_| analysis.has_estimates)
            .map(|rows| {
                say(
                    "diagram.stats.expected",
                    vec![("rows", rows_phrase(rows.max(1.0)).into())],
                )
            }),
    }
}

fn collapsed_summary(
    analysis: &PlanAnalysis,
    node: &NodeAnalysis,
    hidden: usize,
) -> DisplayMessage {
    let metrics = &node.metrics;
    let below = |inclusive: Option<f64>, own: Option<f64>| -> Option<f64> {
        Some((inclusive? - own.unwrap_or(0.0)).clamp(0.0, 1.0))
    };
    match analysis.primary_metric {
        PlanMetric::Time => match below(metrics.inclusive_time_share, metrics.time_share) {
            Some(share) => say(
                "diagram.collapsed.time",
                vec![
                    ("count", hidden.into()),
                    ("share", human_share(share).into()),
                ],
            ),
            None => say("diagram.collapsed.plain", vec![("count", hidden.into())]),
        },
        PlanMetric::Cost => match below(metrics.inclusive_cost_share, metrics.cost_share) {
            Some(share) => say(
                "diagram.collapsed.cost",
                vec![
                    ("count", hidden.into()),
                    ("share", human_share(share).into()),
                ],
            ),
            None => say("diagram.collapsed.plain", vec![("count", hidden.into())]),
        },
        _ => say("diagram.collapsed.plain", vec![("count", hidden.into())]),
    }
}

// ---------------------------------------------------------------------------
// Tidy tree
// ---------------------------------------------------------------------------

/// Extra clear space around a subtree laid out as a solid block.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BlockPad {
    pub before: f32,
    pub after: f32,
}

pub(crate) struct TidyInput<'a> {
    /// Top-level nodes, in order.
    pub roots: &'a [usize],
    /// Every node to place, parents before children.
    pub order: &'a [usize],
    /// Children per node index.
    pub children: &'a [Vec<usize>],
    /// Extent of each node along the breadth axis.
    pub sizes: &'a [f32],
    /// Nodes whose whole subtree must keep a clear rectangle to itself.
    pub block: &'a [Option<BlockPad>],
    /// Space between neighbours.
    pub gap: f32,
    /// Centre a parent over its children; otherwise level it with the first.
    pub centered: bool,
}

/// One side of a subtree's outline: the extreme coordinate at each level,
/// deepest level first so adding the subtree's root is a push. `offset` is
/// added to every stored value, which makes moving a whole subtree O(1).
struct Outline {
    values: Vec<f32>,
    offset: f32,
}

impl Outline {
    fn len(&self) -> usize {
        self.values.len()
    }

    /// Coordinate at `level` (0 is the subtree's root).
    fn at(&self, level: usize) -> f32 {
        self.values[self.values.len() - 1 - level] + self.offset
    }

    fn set(&mut self, level: usize, value: f32) {
        let index = self.values.len() - 1 - level;
        self.values[index] = value - self.offset;
    }

    fn push_root(&mut self, value: f32) {
        self.values.push(value - self.offset);
    }
}

struct Contour {
    left: Outline,
    right: Outline,
}

/// Place `next` to the right of `placed`, as close as the outlines allow.
/// Returns the merged contour and how far `next` moved. The work is
/// proportional to the shallower of the two subtrees, which is what keeps
/// the whole layout linear.
fn place_beside(placed: Contour, mut next: Contour, gap: f32) -> (Contour, f32) {
    let shared = placed.left.len().min(next.left.len());
    let mut shift = f32::NEG_INFINITY;
    for level in 0..shared {
        shift = shift.max(placed.right.at(level) - next.left.at(level) + gap);
    }
    if !shift.is_finite() {
        shift = 0.0;
    }
    next.left.offset += shift;
    next.right.offset += shift;
    let Contour {
        left: placed_left,
        right: placed_right,
    } = placed;
    let Contour {
        left: next_left,
        right: next_right,
    } = next;
    let left = if placed_left.len() >= next_left.len() {
        placed_left
    } else {
        let mut left = next_left;
        for level in 0..shared {
            left.set(level, placed_left.at(level));
        }
        left
    };
    let right = if next_right.len() >= placed_right.len() {
        next_right
    } else {
        let mut right = placed_right;
        for level in 0..shared {
            right.set(level, next_right.at(level));
        }
        right
    };
    (Contour { left, right }, shift)
}

/// Breadth coordinate of every node in `input.order`; other indices are 0.
pub(crate) fn tidy_breadth(input: &TidyInput<'_>) -> Vec<f32> {
    let count = input.sizes.len();
    // Offset of each node from its parent (from the forest origin for a root).
    let mut relative = vec![0.0_f32; count];
    let mut contours: Vec<Option<Contour>> = Vec::with_capacity(count);
    contours.resize_with(count, || None);

    // Children come after their parent in `order`, so walking it backwards
    // finishes every subtree before the node above it.
    for &node in input.order.iter().rev() {
        let size = input.sizes[node];
        let mut merged: Option<Contour> = None;
        let mut last_shift = 0.0_f32;
        let mut last_size = 0.0_f32;
        let mut placed_any = false;
        for &child in &input.children[node] {
            let Some(contour) = contours[child].take() else {
                continue;
            };
            let (contour, shift) = match merged.take() {
                None => (contour, 0.0),
                Some(placed) => place_beside(placed, contour, input.gap),
            };
            relative[child] = shift;
            last_shift = shift;
            last_size = input.sizes[child];
            placed_any = true;
            merged = Some(contour);
        }
        let mut contour = match merged {
            Some(mut contour) if placed_any => {
                let start = if input.centered {
                    (last_shift + last_size - size) / 2.0
                } else {
                    0.0
                };
                // Re-base the frame so this node starts at 0.
                contour.left.offset -= start;
                contour.right.offset -= start;
                for &child in &input.children[node] {
                    relative[child] -= start;
                }
                contour.left.push_root(0.0);
                contour.right.push_root(size);
                contour
            }
            _ => Contour {
                left: Outline {
                    values: vec![0.0],
                    offset: 0.0,
                },
                right: Outline {
                    values: vec![size],
                    offset: 0.0,
                },
            },
        };
        if let Some(pad) = input.block[node] {
            let levels = contour.left.len();
            let mut low = f32::INFINITY;
            let mut high = f32::NEG_INFINITY;
            for level in 0..levels {
                low = low.min(contour.left.at(level));
                high = high.max(contour.right.at(level));
            }
            for level in 0..levels {
                contour.left.set(level, low - pad.before);
                contour.right.set(level, high + pad.after);
            }
        }
        contours[node] = Some(contour);
    }

    let mut merged: Option<Contour> = None;
    for &root in input.roots {
        let Some(contour) = contours[root].take() else {
            continue;
        };
        let (contour, shift) = match merged.take() {
            None => (contour, 0.0),
            Some(placed) => place_beside(placed, contour, input.gap * 2.0),
        };
        relative[root] = shift;
        merged = Some(contour);
    }

    // Parents come first in `order`, so one forward pass turns offsets into
    // coordinates.
    let mut absolute = vec![0.0_f32; count];
    let mut is_root = vec![false; count];
    for &root in input.roots {
        is_root[root] = true;
        absolute[root] = relative[root];
    }
    for &node in input.order {
        let base = absolute[node];
        for &child in &input.children[node] {
            absolute[child] = base + relative[child];
        }
    }
    absolute
}

pub(crate) static PHRASES: &[PlanPhrase] = &[
    phrase!(
        "diagram.stats.rows_time",
        "{rows} · {time}",
        "Execution Plan → Diagram: the numbers line inside an operator box. {rows} is a row-count phrase such as '8,210 rows'; {time} is the operator's own time such as 84 ms."
    ),
    phrase!(
        "diagram.stats.expected",
        "about {rows}",
        "Execution Plan → Diagram: the numbers line inside an operator box on an estimated plan. {rows} is a row-count phrase such as '8,210 rows'."
    ),
    phrase!(
        "diagram.stats.never_ran",
        "never ran",
        "Execution Plan → Diagram: the numbers line inside the box of an operator the executed plan reports as never executed."
    ),
    phrase!(
        "diagram.collapsed.time",
        "{count} steps hidden, {share} of the time",
        "Execution Plan → Diagram: summary line inside a collapsed operator box. {count} is the number of operators folded away; {share} is their percentage of the plan's time."
    ),
    phrase!(
        "diagram.collapsed.cost",
        "{count} steps hidden, {share} of the cost",
        "Execution Plan → Diagram: summary line inside a collapsed operator box on a plan without timings. {count} is the number of operators folded away; {share} is their percentage of the estimated cost."
    ),
    phrase!(
        "diagram.collapsed.plain",
        "{count} steps hidden",
        "Execution Plan → Diagram: summary line inside a collapsed operator box when the plan has no timings or costs. {count} is the number of operators folded away."
    ),
];

#[cfg(test)]
mod tests;
