//! Pure layout helpers for the relationships diagram: zoom math, layered
//! placement, neighbourhood focus, and subset packing. No GPUI types.

use crate::er_diagram::{CELL_H_GAP, CELL_V_GAP, ErGraph, ErLayout, MARGIN};

pub const MIN_ZOOM: f32 = 0.25;
pub const MAX_ZOOM: f32 = 3.0;
pub const ZOOM_STEP: f32 = 0.1;

pub fn clamp_zoom(z: f32) -> f32 {
    if z.is_finite() {
        z.clamp(MIN_ZOOM, MAX_ZOOM)
    } else {
        1.0
    }
}

/// Zoom and pan that show the whole `layout` inside `view` (w, h) with
/// `MARGIN` padding, centred. Zoom is clamped; an empty layout returns (1.0, (0,0)).
pub fn fit_to_view(layout: &ErLayout, view: (f32, f32)) -> (f32, (f32, f32)) {
    if layout.width <= 0.0
        || layout.height <= 0.0
        || !layout.width.is_finite()
        || !layout.height.is_finite()
    {
        return (1.0, (0.0, 0.0));
    }
    let avail_w = (view.0 - 2.0 * MARGIN).max(1.0);
    let avail_h = (view.1 - 2.0 * MARGIN).max(1.0);
    let zoom = clamp_zoom((avail_w / layout.width).min(avail_h / layout.height));
    let pan = (
        (view.0 - layout.width * zoom) / 2.0,
        (view.1 - layout.height * zoom) / 2.0,
    );
    (zoom, pan)
}

/// Zoom about a point: keep the layout point under `anchor` (canvas-local
/// pixels) fixed while zoom changes from `old` to `new`. Returns the new pan.
pub fn zoom_about(anchor: (f32, f32), pan: (f32, f32), old: f32, new: f32) -> (f32, f32) {
    let old = if old.abs() < f32::EPSILON { 1.0 } else { old };
    let layout_x = (anchor.0 - pan.0) / old;
    let layout_y = (anchor.1 - pan.1) / old;
    (anchor.0 - layout_x * new, anchor.1 - layout_y * new)
}

/// Layered layout: BFS from the most-connected node(s) (ties by index),
/// each BFS depth is a column; nodes within a column are ordered by first
/// visit; unreached components start a new BFS at their most-connected node
/// and continue in the next free columns. Same `ErLayout` contract as
/// `layout_grid` (positions per node index, width/height bounding all boxes).
pub fn layout_layered(graph: &ErGraph, node_w: f32, node_h: f32) -> ErLayout {
    let cell_h = node_h + CELL_V_GAP;
    let cell_w = node_w + CELL_H_GAP;
    let n = graph.nodes.len();
    if n == 0 {
        return ErLayout {
            positions: Vec::new(),
            width: 0.0,
            height: 0.0,
        };
    }

    let mut degree = vec![0usize; n];
    let mut adjacency = vec![Vec::new(); n];
    for edge in &graph.edges {
        if edge.from >= n || edge.to >= n {
            continue;
        }
        if edge.from == edge.to {
            degree[edge.from] += 1;
            continue;
        }
        degree[edge.from] += 1;
        degree[edge.to] += 1;
        adjacency[edge.from].push(edge.to);
        adjacency[edge.to].push(edge.from);
    }
    for neighbors in &mut adjacency {
        neighbors.sort_unstable();
        neighbors.dedup();
    }

    let mut visited = vec![false; n];
    let mut columns: Vec<Vec<usize>> = Vec::new();

    loop {
        let seed = (0..n)
            .filter(|&i| !visited[i])
            .max_by_key(|&i| (degree[i], std::cmp::Reverse(i)));
        let Some(seed) = seed else {
            break;
        };
        let start_col = columns.len();
        visited[seed] = true;
        let mut queue = std::collections::VecDeque::from([(seed, start_col)]);
        while let Some((current, depth)) = queue.pop_front() {
            if columns.len() <= depth {
                columns.resize(depth + 1, Vec::new());
            }
            columns[depth].push(current);
            for &next in &adjacency[current] {
                if !visited[next] {
                    visited[next] = true;
                    queue.push_back((next, depth + 1));
                }
            }
        }
    }

    let mut positions = vec![(MARGIN, MARGIN); n];
    for (col, nodes) in columns.iter().enumerate() {
        for (row, &i) in nodes.iter().enumerate() {
            positions[i] = (MARGIN + col as f32 * cell_w, MARGIN + row as f32 * cell_h);
        }
    }

    let cols = columns.len().max(1);
    let rows = columns.iter().map(|c| c.len()).max().unwrap_or(0).max(1);
    ErLayout {
        positions,
        width: 2.0 * MARGIN + (cols - 1) as f32 * cell_w + node_w,
        height: 2.0 * MARGIN + (rows - 1) as f32 * cell_h + node_h,
    }
}

/// Node indices to show when focusing on `center`: the node itself plus every
/// node sharing an edge with it (either direction). Sorted ascending.
pub fn neighbourhood(graph: &ErGraph, center: usize) -> Vec<usize> {
    let mut out = vec![center];
    for edge in &graph.edges {
        if edge.from == center {
            out.push(edge.to);
        } else if edge.to == center {
            out.push(edge.from);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Given a full layout and a subset of visible nodes, repack the visible ones
/// with `layout_grid`/`layout_layered` semantics: returns a layout whose
/// `positions` still has one entry per graph node (hidden nodes keep their
/// old positions) so `node_at`/paint indexing stays index-aligned.
pub fn layout_subset(full: &ErLayout, visible: &[usize], packed: &ErLayout) -> ErLayout {
    let mut positions = full.positions.clone();
    for (k, &i) in visible.iter().enumerate() {
        if let (Some(slot), Some(&pos)) = (positions.get_mut(i), packed.positions.get(k)) {
            *slot = pos;
        }
    }
    let (width, height) = if visible.is_empty() {
        (full.width, full.height)
    } else {
        (packed.width, packed.height)
    };
    ErLayout {
        positions,
        width,
        height,
    }
}

/// Restrict `graph` to `visible` node indices (order preserved, duplicates
/// dropped). Edges whose endpoints both survive are remapped. The returned
/// `Vec` maps each new index back to the original graph index.
pub fn subgraph(graph: &ErGraph, visible: &[usize]) -> (ErGraph, Vec<usize>) {
    use std::collections::HashMap;

    let mut new_to_old = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for &i in visible {
        if i < graph.nodes.len() && seen.insert(i) {
            new_to_old.push(i);
        }
    }
    let remap: HashMap<usize, usize> = new_to_old
        .iter()
        .enumerate()
        .map(|(new, &old)| (old, new))
        .collect();
    let nodes = new_to_old.iter().map(|&i| graph.nodes[i].clone()).collect();
    let edges = graph
        .edges
        .iter()
        .filter_map(|edge| {
            let from = *remap.get(&edge.from)?;
            let to = *remap.get(&edge.to)?;
            Some(crate::er_diagram::ErEdge {
                from,
                to,
                label: edge.label.clone(),
                pairs: edge.pairs.clone(),
            })
        })
        .collect();
    (
        ErGraph {
            nodes,
            edges,
            truncated: graph.truncated,
            total_tables: graph.total_tables,
            total_relationships: graph.total_relationships,
        },
        new_to_old,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::er_diagram::NODE_W;
    use crate::er_diagram::{build_graph, layout_grid};
    use sqlly_services::ForeignKeyRow;

    fn fk(
        constraint: &str,
        from_schema: &str,
        from_table: &str,
        from_column: &str,
        to_schema: &str,
        to_table: &str,
        to_column: &str,
    ) -> ForeignKeyRow {
        ForeignKeyRow {
            constraint_name: constraint.to_string(),
            from_schema: from_schema.to_string(),
            from_table: from_table.to_string(),
            from_column: from_column.to_string(),
            to_schema: to_schema.to_string(),
            to_table: to_table.to_string(),
            to_column: to_column.to_string(),
        }
    }

    fn col_of(x: f32) -> i32 {
        ((x - MARGIN) / crate::er_diagram::CELL_W).round() as i32
    }

    #[test]
    fn clamp_zoom_bounds_and_nan() {
        assert!((clamp_zoom(0.1) - MIN_ZOOM).abs() < 1e-4);
        assert!((clamp_zoom(9.0) - MAX_ZOOM).abs() < 1e-4);
        assert!((clamp_zoom(1.25) - 1.25).abs() < 1e-4);
        assert!((clamp_zoom(f32::NAN) - 1.0).abs() < 1e-4);
        assert!((clamp_zoom(f32::INFINITY) - 1.0).abs() < 1e-4);
    }

    #[test]
    fn fit_to_view_centres_and_fits() {
        let layout = layout_grid(6, NODE_W, 58.0);
        let (zoom, pan) = fit_to_view(&layout, (800.0, 480.0));
        assert!(
            zoom < 1.0,
            "3×2 grid into 800×480 should zoom out, got {zoom}"
        );
        assert!(zoom >= MIN_ZOOM);
        assert!(
            pan.0 >= 0.0,
            "pan.x should centre (non-negative), got {}",
            pan.0
        );
        assert!(
            pan.1 >= 0.0,
            "pan.y should centre (non-negative), got {}",
            pan.1
        );
        assert!(layout.width * zoom <= 800.0 + 1e-3);
        assert!(layout.height * zoom <= 480.0 + 1e-3);
    }

    #[test]
    fn fit_to_view_empty() {
        let layout = layout_grid(0, NODE_W, 58.0);
        assert_eq!(fit_to_view(&layout, (800.0, 480.0)), (1.0, (0.0, 0.0)));
    }

    #[test]
    fn zoom_about_keeps_anchor_fixed() {
        let anchor = (120.0, 80.0);
        let pan = (-40.0, 10.0);
        let old = 1.0;
        let new = 2.0;
        let new_pan = zoom_about(anchor, pan, old, new);
        let before = ((anchor.0 - pan.0) / old, (anchor.1 - pan.1) / old);
        let after = ((anchor.0 - new_pan.0) / new, (anchor.1 - new_pan.1) / new);
        assert!((before.0 - after.0).abs() < 1e-3);
        assert!((before.1 - after.1).abs() < 1e-3);
    }

    #[test]
    fn layered_hub_is_first_column() {
        let rows: Vec<ForeignKeyRow> = (0..4)
            .map(|i| {
                fk(
                    &format!("fk_{i}"),
                    "s",
                    &format!("leaf_{i}"),
                    "hub_id",
                    "s",
                    "hub",
                    "id",
                )
            })
            .collect();
        let graph = build_graph(&rows);
        let layout = layout_layered(&graph, NODE_W, 58.0);
        let hub = graph
            .nodes
            .iter()
            .position(|n| n.table == "hub")
            .expect("hub");
        assert_eq!(col_of(layout.positions[hub].0), 0);
        for (i, node) in graph.nodes.iter().enumerate() {
            if node.table != "hub" {
                assert_eq!(
                    col_of(layout.positions[i].0),
                    1,
                    "leaf {} not in column 1",
                    node.table
                );
            }
        }
    }

    #[test]
    fn layered_two_components_do_not_overlap() {
        let rows = vec![
            fk("a1", "s", "a_leaf1", "x", "s", "a_hub", "id"),
            fk("a2", "s", "a_leaf2", "x", "s", "a_hub", "id"),
            fk("b1", "s", "b_leaf1", "x", "s", "b_hub", "id"),
            fk("b2", "s", "b_leaf2", "x", "s", "b_hub", "id"),
        ];
        let graph = build_graph(&rows);
        let layout = layout_layered(&graph, NODE_W, 58.0);
        assert_eq!(layout.positions.len(), graph.nodes.len());
        let mut seen: Vec<(i64, i64)> = layout
            .positions
            .iter()
            .map(|&(x, y)| {
                assert!(x.is_finite() && y.is_finite());
                (x as i64, y as i64)
            })
            .collect();
        let n = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), n, "layered positions must not overlap");
    }

    #[test]
    fn layered_is_deterministic() {
        let rows = vec![
            fk("fk_a", "s", "b", "x", "s", "a", "id"),
            fk("fk_b", "s", "c", "x", "s", "a", "id"),
            fk("fk_c", "s", "c", "y", "s", "b", "id"),
        ];
        let graph = build_graph(&rows);
        let a = layout_layered(&graph, NODE_W, 58.0);
        let b = layout_layered(&graph, NODE_W, 58.0);
        assert_eq!(a, b);
    }

    #[test]
    fn neighbourhood_includes_both_directions_and_self() {
        let graph = build_graph(&[
            fk("ab", "s", "b", "a_id", "s", "a", "id"),
            fk("bc", "s", "c", "b_id", "s", "b", "id"),
        ]);
        let b = graph.nodes.iter().position(|n| n.table == "b").expect("b");
        let a = graph.nodes.iter().position(|n| n.table == "a").expect("a");
        let c = graph.nodes.iter().position(|n| n.table == "c").expect("c");
        let around_b = neighbourhood(&graph, b);
        assert_eq!(around_b, {
            let mut v = vec![a, b, c];
            v.sort_unstable();
            v
        });
        let around_a = neighbourhood(&graph, a);
        assert!(around_a.contains(&a));
        assert!(around_a.contains(&b));
        assert!(!around_a.contains(&c));
    }

    #[test]
    fn layout_subset_keeps_index_alignment() {
        let full = layout_grid(4, NODE_W, 58.0);
        let packed = layout_grid(2, NODE_W, 58.0);
        let visible = [0usize, 2];
        let out = layout_subset(&full, &visible, &packed);
        assert_eq!(out.positions.len(), 4);
        assert_eq!(out.positions[0], packed.positions[0]);
        assert_eq!(out.positions[2], packed.positions[1]);
        assert_eq!(out.positions[1], full.positions[1]);
        assert_eq!(out.positions[3], full.positions[3]);
        assert_eq!(out.width, packed.width);
        assert_eq!(out.height, packed.height);
    }

    #[test]
    fn subgraph_reindexes_edges() {
        let graph = build_graph(&[
            fk("ab", "s", "b", "a_id", "s", "a", "id"),
            fk("bc", "s", "c", "b_id", "s", "b", "id"),
        ]);
        let a = graph.nodes.iter().position(|n| n.table == "a").unwrap();
        let b = graph.nodes.iter().position(|n| n.table == "b").unwrap();
        let visible = neighbourhood(&graph, a);
        let (sub, map) = subgraph(&graph, &visible);
        assert_eq!(sub.nodes.len(), 2);
        assert_eq!(sub.edges.len(), 1);
        assert_eq!(map.len(), 2);
        assert!(map.contains(&a) && map.contains(&b));
        assert!(sub.edges[0].from < 2 && sub.edges[0].to < 2);
        assert_ne!(sub.edges[0].from, sub.edges[0].to);
    }
}
