use super::*;
use crate::plans::analysis::analyze_plan;
use crate::plans::plan_fixtures::*;

fn layout(plan: &ExecutionPlan, options: &DiagramOptions) -> PlanDiagram {
    plan_diagram(plan, &analyze_plan(plan), options)
}

fn left_to_right() -> DiagramOptions {
    DiagramOptions {
        orientation: DiagramOrientation::RootLeft,
        ..Default::default()
    }
}

/// Boxes sorted by one axis and swept, so the check stays near-linear on
/// big diagrams instead of comparing every pair.
fn assert_no_overlap(diagram: &PlanDiagram) {
    let mut boxes: Vec<&DiagramNode> = diagram.nodes.iter().collect();
    boxes.sort_by(|a, b| a.y.total_cmp(&b.y));
    for (index, a) in boxes.iter().enumerate() {
        for b in &boxes[index + 1..] {
            if b.y >= a.y + a.height {
                break;
            }
            let apart = b.x >= a.x + a.width || a.x >= b.x + b.width;
            assert!(apart, "nodes {} and {} overlap", a.node_id, b.node_id);
        }
    }
}

fn assert_inside_canvas(diagram: &PlanDiagram) {
    for node in &diagram.nodes {
        assert!(
            node.x >= 0.0 && node.y >= 0.0,
            "node {} off canvas",
            node.node_id
        );
        assert!(node.x + node.width <= diagram.width + 0.01);
        assert!(node.y + node.height <= diagram.height + 0.01);
    }
    for group in &diagram.groups {
        assert!(group.x >= 0.0 && group.y >= 0.0);
        assert!(group.x + group.width <= diagram.width + 0.01);
        assert!(group.y + group.height <= diagram.height + 0.01);
    }
}

#[test]
fn root_top_puts_inputs_below_their_consumer() {
    let diagram = layout(&pg_join(), &DiagramOptions::default());
    assert_eq!(diagram.nodes.len(), 5);
    assert_eq!(diagram.edges.len(), 4);
    assert_no_overlap(&diagram);
    assert_inside_canvas(&diagram);
    for edge in &diagram.edges {
        let from = diagram.node(edge.from_node).expect("from");
        let to = diagram.node(edge.to_node).expect("to");
        assert!(to.y >= from.y + from.height, "input sits below");
        let first = edge.points.first().expect("points");
        let last = edge.points.last().expect("points");
        assert_eq!(first.1, from.y + from.height, "leaves the bottom edge");
        assert_eq!(last.1, to.y, "arrives at the top edge");
        // Orthogonal routing: every segment is horizontal or vertical.
        for pair in edge.points.windows(2) {
            assert!(pair[0].0 == pair[1].0 || pair[0].1 == pair[1].1);
        }
    }
    // A parent is centred over its inputs.
    let join = diagram.node(2).expect("join");
    let (left, right) = (
        diagram.node(3).expect("scan"),
        diagram.node(4).expect("hash"),
    );
    let centre = (left.x + right.x + right.width) / 2.0;
    assert!((join.x + join.width / 2.0 - centre).abs() < 0.5);
    assert!(left.x < right.x, "inputs keep plan order");
}

#[test]
fn root_left_puts_inputs_to_the_right() {
    let diagram = layout(&pg_join(), &left_to_right());
    assert_eq!(diagram.orientation, DiagramOrientation::RootLeft);
    assert_no_overlap(&diagram);
    assert_inside_canvas(&diagram);
    for edge in &diagram.edges {
        let from = diagram.node(edge.from_node).expect("from");
        let to = diagram.node(edge.to_node).expect("to");
        assert!(to.x >= from.x + from.width, "input sits to the right");
    }
    let (first, second) = (
        diagram.node(3).expect("scan"),
        diagram.node(4).expect("hash"),
    );
    assert!(first.y < second.y, "inputs keep plan order, top to bottom");
}

#[test]
fn boxes_carry_the_text_a_renderer_needs() {
    let diagram = layout(&pg_join(), &DiagramOptions::default());
    let orders = diagram.node(3).expect("orders");
    assert_eq!(orders.title, "Seq Scan");
    assert_eq!(orders.subtitle.as_deref(), Some("orders o"));
    assert_eq!(
        orders
            .stats
            .as_ref()
            .map(|stats| stats.fallback().to_string())
            .as_deref(),
        Some("1,000 rows · 70 ms")
    );
    assert_eq!(orders.share_label.as_deref(), Some("70%"));
    assert!(orders.flags.bottleneck);
    assert!((orders.heat - 0.7).abs() < 1e-6);
    assert_eq!(orders.category, OperatorCategory::Scan);
    assert!(!orders.has_children);
    let sort = diagram.node(1).expect("sort");
    assert!(sort.has_children && !sort.collapsed);
    assert_eq!(sort.parent_id, None);
    assert!(
        orders.height > sort.height,
        "a subtitle makes the box taller"
    );

    let estimated = layout(&pg_join_estimated(), &DiagramOptions::default());
    let orders = estimated.node(3).expect("orders");
    assert_eq!(
        orders
            .stats
            .as_ref()
            .map(|stats| stats.fallback().to_string())
            .as_deref(),
        Some("about 50 rows")
    );
    assert_eq!(orders.share_label.as_deref(), Some("60%"));
    assert_eq!(estimated.metric, Some(PlanMetric::Cost));
}

#[test]
fn uniform_height_and_custom_width() {
    let options = DiagramOptions {
        uniform_height: true,
        node_width: 220.0,
        ..Default::default()
    };
    let diagram = layout(&pg_join(), &options);
    for node in &diagram.nodes {
        assert_eq!(node.height, DIAGRAM_BOX_UNIFORM_H);
        assert_eq!(node.width, 220.0);
    }
    assert_no_overlap(&diagram);
}

#[test]
fn edges_scale_with_rows_and_mark_the_critical_path() {
    let diagram = layout(&pg_join(), &DiagramOptions::default());
    let edge = |to: i32| {
        diagram
            .edges
            .iter()
            .find(|edge| edge.to_node == to)
            .expect("edge")
    };
    assert_eq!(edge(3).rows, Some(1000.0));
    assert_eq!(edge(3).rows_label.as_deref(), Some("1,000"));
    assert!(!edge(3).rows_estimated);
    assert_eq!(edge(3).thickness, DIAGRAM_EDGE_MAX);
    assert!(edge(5).thickness < edge(3).thickness);
    assert!(edge(5).thickness > DIAGRAM_EDGE_MIN);
    assert!(edge(2).on_critical_path && edge(3).on_critical_path);
    assert!(!edge(4).on_critical_path && !edge(5).on_critical_path);

    let estimated = layout(&pg_join_estimated(), &DiagramOptions::default());
    assert!(estimated.edges.iter().all(|edge| edge.rows_estimated));
}

#[test]
fn edge_thickness_is_logarithmic_and_bounded() {
    assert_eq!(edge_thickness(None, 1000.0), DIAGRAM_EDGE_MIN);
    assert_eq!(edge_thickness(Some(0.0), 1000.0), DIAGRAM_EDGE_MIN);
    assert_eq!(edge_thickness(Some(1000.0), 1000.0), DIAGRAM_EDGE_MAX);
    assert_eq!(edge_thickness(Some(5000.0), 1000.0), DIAGRAM_EDGE_MAX);
    assert_eq!(edge_thickness(Some(10.0), 0.0), DIAGRAM_EDGE_MIN);
    let mut last = 0.0;
    for rows in [1.0, 10.0, 100.0, 1_000.0, 10_000.0, 1_000_000.0] {
        let thickness = edge_thickness(Some(rows), 1_000_000.0);
        assert!(thickness >= last, "{rows}");
        assert!((DIAGRAM_EDGE_MIN..=DIAGRAM_EDGE_MAX).contains(&thickness));
        last = thickness;
    }
    // A thousand times fewer rows is still clearly visible, not hairline.
    assert!(edge_thickness(Some(1_000.0), 1_000_000.0) > 3.0);
}

#[test]
fn collapsing_hides_a_subtree_and_summarises_it() {
    let plan = pg_join();
    let open = layout(&plan, &DiagramOptions::default());
    let options = DiagramOptions {
        collapsed: [2].into_iter().collect(),
        ..Default::default()
    };
    let diagram = layout(&plan, &options);
    let ids: Vec<i32> = diagram.nodes.iter().map(|node| node.node_id).collect();
    assert_eq!(ids, vec![1, 2]);
    assert_eq!(diagram.edges.len(), 1);
    let join = diagram.node(2).expect("join");
    assert!(join.collapsed && join.has_children);
    assert_eq!(join.hidden_count, 3);
    let summary = join
        .summary
        .as_ref()
        .expect("summary")
        .fallback()
        .to_string();
    assert!(summary.contains('3'), "{summary}");
    assert!(diagram.height < open.height);
    assert_no_overlap(&diagram);

    // Collapsing a leaf or an unknown id changes nothing.
    let options = DiagramOptions {
        collapsed: [5, 999].into_iter().collect(),
        ..Default::default()
    };
    let same = layout(&plan, &options);
    assert_eq!(same.nodes.len(), 5);
    assert!(!same.node(5).expect("leaf").collapsed);
    for node in &open.nodes {
        let other = same.node(node.node_id).expect("node");
        assert_eq!((node.x, node.y), (other.x, other.y));
    }
}

#[test]
fn collapsing_one_branch_keeps_the_other_branch_in_order() {
    let plan = pg_join();
    let options = DiagramOptions {
        collapsed: [4].into_iter().collect(),
        ..Default::default()
    };
    let diagram = layout(&plan, &options);
    let ids: Vec<i32> = diagram.nodes.iter().map(|node| node.node_id).collect();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    assert!(diagram.node(3).expect("scan").x < diagram.node(4).expect("hash").x);
    assert_eq!(diagram.node(4).expect("hash").hidden_count, 1);
}

#[test]
fn parallel_zones_get_a_frame_around_their_nodes() {
    let mut gather = ran(est(op(1, "Gather"), 300.0, 100.0), 300.0, 1.0, 30.0);
    gather.workers_launched = Some(2);
    let sort = ran(est(op(2, "Sort"), 100.0, 90.0), 100.0, 3.0, 26.0);
    let scan = ran(
        est(scan(3, "Parallel Seq Scan", "events"), 125.0, 80.0),
        100.0,
        3.0,
        24.0,
    );
    let top = ran(est(op(4, "Limit"), 10.0, 101.0), 10.0, 1.0, 31.0);
    let plan = plan(
        crate::plans::PlanDialect::Postgres,
        vec![top, gather, sort, scan],
        &[(4, 1), (1, 2), (2, 3)],
    );
    for options in [DiagramOptions::default(), left_to_right()] {
        let diagram = layout(&plan, &options);
        assert_eq!(diagram.groups.len(), 1);
        let group = &diagram.groups[0];
        assert_eq!(group.kind, PlanGroupKind::Parallel);
        assert!(!group.label.fallback().is_empty());
        for id in &group.node_ids {
            let node = diagram.node(*id).expect("member");
            assert!(node.x >= group.x && node.x + node.width <= group.x + group.width);
            assert!(node.y >= group.y && node.y + node.height <= group.y + group.height);
            assert_eq!(node.group, Some(0));
        }
        // The node above the zone stays outside the frame.
        let outside = diagram.node(4).expect("limit");
        assert_eq!(outside.group, None);
        let apart = outside.y + outside.height <= group.y
            || outside.x + outside.width <= group.x
            || outside.y >= group.y + group.height
            || outside.x >= group.x + group.width;
        assert!(apart, "frame does not cover a node outside the zone");
        assert_inside_canvas(&diagram);
    }
    let off = DiagramOptions {
        groups: false,
        ..Default::default()
    };
    assert!(layout(&plan, &off).groups.is_empty());
}

#[test]
fn the_metric_option_switches_share_and_heat() {
    let plan = pg_join();
    let options = DiagramOptions {
        metric: Some(PlanMetric::Cost),
        ..Default::default()
    };
    let diagram = layout(&plan, &options);
    let orders = diagram.node(3).expect("orders");
    assert_eq!(orders.share_label.as_deref(), Some("60%"));
    assert!((orders.heat - 0.6).abs() < 1e-6);
    assert!((orders.heat_by_metric.time - 0.7).abs() < 1e-6);
}

#[test]
fn wide_and_bushy_plans_do_not_overlap() {
    let plan = large(400);
    for options in [DiagramOptions::default(), left_to_right()] {
        let diagram = layout(&plan, &options);
        assert_eq!(diagram.nodes.len(), plan.nodes.len());
        assert_eq!(diagram.edges.len(), plan.edges.len());
        assert_no_overlap(&diagram);
        assert_inside_canvas(&diagram);
    }
}

#[test]
fn a_very_deep_plan_does_not_overflow_the_stack() {
    let plan = chain(5000);
    for options in [DiagramOptions::default(), left_to_right()] {
        let diagram = layout(&plan, &options);
        assert_eq!(diagram.nodes.len(), 5000);
        assert_eq!(diagram.nodes.last().expect("leaf").depth, 4999);
    }
}

#[test]
fn an_empty_plan_gives_an_empty_diagram() {
    let diagram = layout(&ExecutionPlan::default(), &DiagramOptions::default());
    assert!(diagram.nodes.is_empty() && diagram.edges.is_empty());
}

#[test]
fn orientation_keys_round_trip() {
    for orientation in [DiagramOrientation::RootTop, DiagramOrientation::RootLeft] {
        assert_eq!(
            DiagramOrientation::from_key(orientation.key()),
            Some(orientation)
        );
    }
    assert_eq!(DiagramOrientation::from_key("sideways"), None);
}

/// A connector carries the figure its input's box shows. An optimizer guess
/// under one row is "about 1 row" in the box, so the line must not say 0;
/// and large counts use the box's rounding (`50K`, not `50.0K`).
#[test]
fn edge_labels_use_the_same_row_figure_as_the_input_box() {
    let root = est(op(1, "Nested Loop"), 1.0, 20.0);
    let tiny = est(scan(2, "Index Seek", "orders"), 0.3, 5.0);
    let big = est(scan(3, "Seq Scan", "events"), 50_000.0, 10.0);
    let plan = plan(
        crate::plans::PlanDialect::MySql,
        vec![root, tiny, big],
        &[(1, 2), (1, 3)],
    );
    let diagram = layout(&plan, &DiagramOptions::default());
    let box_text = |id: i32| {
        diagram
            .node(id)
            .and_then(|node| node.stats.as_ref())
            .map(|stats| stats.fallback().to_string())
    };
    let edge_label = |id: i32| {
        diagram
            .edges
            .iter()
            .find(|edge| edge.to_node == id)
            .and_then(|edge| edge.rows_label.clone())
    };
    assert_eq!(box_text(2).as_deref(), Some("about 1 row"));
    assert_eq!(edge_label(2).as_deref(), Some("1"));
    assert_eq!(box_text(3).as_deref(), Some("about 50K rows"));
    assert_eq!(edge_label(3).as_deref(), Some("50K"));
}
