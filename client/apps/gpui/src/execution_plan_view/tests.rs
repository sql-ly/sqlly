//! Tests for the plan pane: what a capture installs, how selection moves
//! between the views, and the pure helpers behind the table, flame chart and
//! diagram search.

use super::model::ai_explain_prompt_for;
use super::*;
use gpui::{Modifiers, MouseButton, size};
use sqlly_services::plans::parser::PlanParser;
use sqlly_services::plans::{
    DiagramOrientation, OperatorTableColumn, PlanMetric, import_plan_document,
};

const SCAN_XML: &str = r#"<ShowPlanXML><StmtSimple StatementText="SELECT *" StatementType="SELECT" StatementSubTreeCost="2"><RelOp NodeId="1" LogicalOp="Scan" PhysicalOp="Table Scan" EstimateRows="4" EstimatedTotalSubtreeCost="2"><Object Database="[db]" Schema="[dbo]" Table="[Orders]" /></RelOp></StmtSimple></ShowPlanXML>"#;

const TRIVIAL_XML: &str = r#"<ShowPlanXML><StmtSimple StatementText="SELECT 1" StatementType="SELECT" StatementSubTreeCost="0.1"><RelOp NodeId="0" LogicalOp="Select" PhysicalOp="Compute Scalar" EstimateRows="1" EstimatedTotalSubtreeCost="0.1" /></StmtSimple></ShowPlanXML>"#;

/// A measured PostgreSQL plan: a hash join over two scans, under a sort.
const JOIN_PLAN: &str = "\
Sort  (cost=450000.00..452500.00 rows=1000 width=40) (actual time=3000.000..3100.000 rows=100000 loops=1)
  Sort Key: o.total
  Sort Method: external merge  Disk: 51200kB
  ->  Hash Join  (cost=30.00..400000.00 rows=1000 width=40) (actual time=2.000..2800.000 rows=100000 loops=1)
        Hash Cond: (o.customer_id = c.id)
        ->  Seq Scan on orders o  (cost=0.00..300000.00 rows=1000 width=20) (actual time=0.010..1800.000 rows=100000 loops=1)
              Filter: (total > 100)
              Rows Removed by Filter: 7900000
        ->  Hash  (cost=20.00..20.00 rows=500 width=20) (actual time=1.500..1.500 rows=500 loops=1)
              ->  Seq Scan on customers c  (cost=0.00..20.00 rows=500 width=20) (actual time=0.005..1.000 rows=500 loops=1)
Planning Time: 0.200 ms
Execution Time: 3150.000 ms
";

fn sample_plan() -> ExecutionPlan {
    PlanParser::new().parse_xml(SCAN_XML).unwrap()
}

fn join_plans() -> Vec<ExecutionPlan> {
    import_plan_document(JOIN_PLAN)
        .expect("the sample plan imports")
        .plans
}

fn statement(plan: ExecutionPlan) -> StatementPlans {
    BatchPlans::build(vec![plan], None, None)
        .statements
        .remove(0)
}

fn join_statement() -> StatementPlans {
    statement(join_plans().remove(0))
}

fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        if cx.try_global::<gpui_component::Theme>().is_none() {
            gpui_component::init(cx);
        }
    });
}

fn install(
    view: &mut ExecutionPlanView,
    kind: PlanKind,
    sql: &str,
    plans: Vec<ExecutionPlan>,
    cx: &mut Context<ExecutionPlanView>,
) {
    view.set_plans_with_schema(kind, sql.to_string(), plans, None, cx);
}

/// The operator ids of the join sample: (sort, join, orders scan).
fn join_ids(active: &StatementPlans) -> (i32, i32, i32) {
    let root = active.analysis.roots[0];
    let join = active.analysis.node(root).unwrap().children[0];
    let orders = active
        .analysis
        .nodes
        .iter()
        .find(|node| {
            node.object
                .as_ref()
                .is_some_and(|object| object.label().contains("orders"))
        })
        .expect("the orders scan")
        .node_id;
    (root, join, orders)
}

#[test]
fn ai_explain_prompt_digests_findings_and_operators() {
    assert!(ai_explain_prompt_for(&[]).is_none());
    let plans = vec![join_statement()];
    let prompt = ai_explain_prompt_for(&plans).expect("has statements");
    assert!(prompt.contains("Explain this SQL execution plan"));
    assert!(prompt.contains("Findings:"), "{prompt}");
    assert!(prompt.contains("orders"));
    assert!(prompt.contains("Most expensive operators:"));
    // The digest is prose, not the plan document.
    assert!(!prompt.contains("actual time="));
}

#[test]
fn statement_plans_precompute_every_view() {
    let plans = statement(sample_plan());
    assert_eq!(plans.statement_type, "SELECT");
    assert_eq!(plans.table.len(), 1);
    assert_eq!(plans.story.steps.len(), 1);
    assert!(!plans.used_schema, "no catalog was supplied");

    let join = join_statement();
    assert!(
        !join.findings.is_empty(),
        "a scan that discards nearly everything it reads is reported"
    );
    assert_eq!(join.table.len(), join.analysis.nodes.len());
    assert!(
        join.flame.is_some(),
        "a measured plan has a time chart to draw"
    );
    assert!(join.breakdown.is_some());
}

#[test]
fn trivial_plan_has_no_findings() {
    let plans = statement(PlanParser::new().parse_xml(TRIVIAL_XML).unwrap());
    assert!(plans.findings.is_empty());
    assert_eq!(plans.table.len(), 1);
}

#[test]
fn truncate_clips_to_one_line() {
    assert_eq!(truncate("SELECT 1", 120), "SELECT 1");
    assert_eq!(truncate("SELECT 1\nFROM t", 120), "SELECT 1 FROM t");
    assert_eq!(
        truncate("SELECT\n\t@Offset = t.Utc * 60\nFROM  Shared.Tenant t", 120),
        "SELECT @Offset = t.Utc * 60 FROM Shared.Tenant t"
    );
    let long = "x".repeat(200);
    let clipped = truncate(&long, 120);
    assert!(clipped.ends_with('…'));
    assert_eq!(clipped.chars().count(), 121);
    assert_eq!(truncate("", 120), "");
}

#[test]
fn search_matches_operator_table_and_alias_in_execution_order() {
    let active = join_statement();
    let (_, join, orders) = join_ids(&active);

    let scans = diagram::search_plan(&active, "seq scan");
    assert_eq!(scans.ordered.len(), 2, "both scans match");

    let by_table = diagram::search_plan(&active, "ORDERS");
    assert_eq!(by_table.ordered, vec![orders]);

    let by_operator = diagram::search_plan(&active, "hash join");
    assert_eq!(by_operator.ordered, vec![join]);

    assert!(diagram::search_plan(&active, "   ").ordered.is_empty());
    assert!(
        diagram::search_plan(&active, "nothing here")
            .ordered
            .is_empty()
    );
}

#[test]
fn operator_table_cells_carry_numbers_not_text() {
    let active = join_statement();
    let set = table::table_set(&active.table);
    assert_eq!(set.rows.len(), active.table.len());
    assert!(set.rows.iter().all(|row| row.len() == set.columns.len()));
    // Numbers stay numeric so the grid sorts them as numbers.
    let step = table::cell(&active.table[0], OperatorTableColumn::Step);
    assert!(
        !matches!(step, sqlly_core::CellValue::String(_)),
        "the step column is numeric, got {step:?}"
    );
    let operator = table::cell(&active.table[0], OperatorTableColumn::Operator);
    assert!(matches!(operator, sqlly_core::CellValue::String(_)));
    // A measured plan names its time column with the unit.
    assert!(
        set.columns.iter().any(|column| column.name.contains("ms")),
        "columns: {:?}",
        set.columns.iter().map(|c| &c.name).collect::<Vec<_>>()
    );
}

#[test]
fn flame_hit_test_finds_the_bar_under_the_pointer() {
    let active = join_statement();
    let layout = active.flame.as_ref().expect("measured plan");
    let root = layout
        .cells
        .iter()
        .find(|cell| cell.depth == 0)
        .expect("a root bar");
    let middle = root.x + root.width / 2.0;
    assert_eq!(flame::flame_cell_at(layout, middle, 0), Some(root.node_id));
    assert_eq!(flame::flame_cell_at(layout, middle, 99), None);
    assert_eq!(flame::flame_cell_at(layout, 2.0, 0), None);
    assert!(flame::metric_value(PlanMetric::Time, 12.0).contains("ms"));
}

#[gpui::test]
fn finding_selection_follows_the_operator(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        assert_eq!(view.finding_count(), view.active().unwrap().findings.len());
        view.toggle_finding(0, cx);
        assert_eq!(view.selected_finding, Some(0));
        assert!(view.expanded_findings.contains(&0));
        let node = view.selected_node.expect("the finding names its operator");
        assert!(view.finding_nodes().contains(&node));
        // Clearing the operator lets go of the finding too.
        view.select_node(None, cx);
        assert_eq!(view.selected_finding, None);
        // Collapsing keeps it selected so the diagram stays lit.
        view.toggle_finding(0, cx);
        assert!(!view.expanded_findings.contains(&0));
        assert_eq!(view.selected_finding, Some(0));
    });
}

/// The Re-capture button and the empty state follow the engine gate the app
/// pushes down.
#[gpui::test]
fn capture_gate_drives_recapture_and_empty_state(cx: &mut gpui::TestAppContext) {
    use crate::capabilities::PlanCaptureGate;
    use sqlly_lang::sqlmodel::DatabaseType;
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        assert_eq!(view.recapture_unavailable_reason(), None);

        view.set_capture_gate(PlanCaptureGate::for_engine(Some(DatabaseType::Redis)), cx);
        assert!(
            view.empty_state_text()
                .starts_with("Query plans are disabled for this database type (Redis).")
        );
        install(view, PlanKind::Estimated, "", vec![sample_plan()], cx);
        assert!(
            view.recapture_unavailable_reason()
                .is_some_and(|r| r.contains("(Redis)"))
        );

        view.set_capture_gate(PlanCaptureGate::for_engine(Some(DatabaseType::Sqlite)), cx);
        assert_eq!(
            view.recapture_unavailable_reason(),
            None,
            "an estimated plan can be re-captured on SQLite"
        );
        assert!(view.empty_state_text().contains("Estimated"));
        install(view, PlanKind::Actual, "", vec![sample_plan()], cx);
        assert!(
            view.recapture_unavailable_reason()
                .is_some_and(|r| r.contains("(SQLite)"))
        );
    });
}

#[gpui::test]
fn capture_history_is_bounded_and_defaults_to_latest_pair(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        for sequence in 0..(MAX_CAPTURE_HISTORY + 2) {
            let mut plan = sample_plan();
            plan.nodes[0].estimated_cost = sequence as f64 + 1.0;
            install(
                view,
                PlanKind::Estimated,
                "SELECT * FROM Orders",
                vec![plan],
                cx,
            );
        }
        assert_eq!(view.captures.len(), MAX_CAPTURE_HISTORY);
        assert_eq!(view.captures.first().unwrap().sequence, 3);
        assert_eq!(view.captures.last().unwrap().sequence, 10);
        assert_eq!(view.baseline_capture, MAX_CAPTURE_HISTORY - 2);
        assert_eq!(view.comparison_capture, MAX_CAPTURE_HISTORY - 1);
    });
}

#[gpui::test]
fn comparison_preserves_capture_kinds_and_checks_query_context(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        let baseline = sample_plan();
        let mut comparison = sample_plan();
        comparison.nodes[0].estimated_cost = 4.0;
        install(
            view,
            PlanKind::Actual,
            "SELECT  *  FROM Orders",
            vec![baseline],
            cx,
        );
        install(
            view,
            PlanKind::Estimated,
            "SELECT * FROM Orders",
            vec![comparison],
            cx,
        );
        view.toggle_comparison(cx);
        assert!(view.comparison_mode);
        assert!(
            view.comparison_result.as_ref().unwrap().is_ok(),
            "whitespace differences do not make it a different query"
        );
        assert!(view.capture_label(0).contains("Actual plan"));
        assert!(view.capture_label(1).contains("Estimated plan"));

        install(
            view,
            PlanKind::Estimated,
            "SELECT * FROM Customers",
            vec![sample_plan()],
            cx,
        );
        assert!(!view.comparison_mode, "a new capture leaves comparison");
        view.toggle_comparison(cx);
        assert_eq!(
            view.comparison_result.as_ref().unwrap().as_ref().err(),
            Some(&CompareIssue::DifferentQuery)
        );
        assert!(
            CompareIssue::DifferentQuery
                .text()
                .contains("different query")
        );
        // Stepping the baseline back to the capture of the same query
        // cannot help: capture 1 is the other half of the pair.
        view.cycle_baseline(cx);
        assert_eq!(view.baseline_capture, 0);
        assert_eq!(view.comparison_capture, 2);
    });
}

#[gpui::test]
fn oversized_capture_is_not_retained_for_comparison(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let mut plan = sample_plan();
    let template = plan.nodes[0].clone();
    plan.nodes = (0..=DEFAULT_MAX_COMPARISON_NODES)
        .map(|id| {
            let mut node = template.clone();
            node.id = id as i32;
            node
        })
        .collect();
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(
            view,
            PlanKind::Estimated,
            "SELECT * FROM LargePlan",
            vec![plan],
            cx,
        );
    });
    // A plan this large is analysed in the background.
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert!(view.captures.is_empty());
        assert_eq!(
            view.oversized_capture,
            Some(DEFAULT_MAX_COMPARISON_NODES + 1)
        );
        assert!(
            view.capture_history_notice()
                .unwrap()
                .contains("not retained")
        );
        // The plan itself is still shown.
        assert!(view.active().is_some());
    });
}

#[gpui::test]
fn new_capture_resets_view_mode_and_selection(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        let (_, join, orders) = join_ids(view.active().unwrap());
        view.set_view_mode(PlanViewMode::Diagram, cx);
        view.select_node(Some(orders), cx);
        view.toggle_fold(join, cx);
        view.set_diagram_search("orders".into(), cx);
        view.flip_diagram(cx);
        let flipped = view.diagram.orientation;

        install(view, PlanKind::Actual, "", join_plans(), cx);
        assert_eq!(view.view_mode, PlanViewMode::Summary);
        assert_eq!(view.selected_node, None);
        assert_eq!(view.selected_finding, None);
        assert!(view.diagram.collapsed.is_empty());
        assert_eq!(
            view.diagram.search, "orders",
            "the search text is the reader's and carries over"
        );
        assert_eq!(
            view.diagram.orientation, flipped,
            "the chosen orientation is a preference, not plan state"
        );
    });
}

#[gpui::test]
fn folding_moves_a_hidden_selection_up_to_the_fold(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        let (root, join, orders) = join_ids(view.active().unwrap());
        view.select_node(Some(orders), cx);
        view.toggle_fold(join, cx);
        assert!(view.diagram.collapsed.contains(&join));
        assert_eq!(view.selected_node, Some(join));
        // Selecting through search or a finding opens the fold again.
        view.show_in_diagram(orders, cx);
        assert!(!view.diagram.collapsed.contains(&join));
        assert_eq!(view.view_mode, PlanViewMode::Diagram);
        assert_eq!(view.selected_node, Some(orders));
        // Folding the root keeps a selection on the root itself.
        view.select_node(Some(root), cx);
        view.toggle_fold(root, cx);
        assert_eq!(view.selected_node, Some(root));
    });
}

#[gpui::test]
fn diagram_keys_walk_the_tree_the_way_it_is_drawn(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        let (root, join, _) = join_ids(view.active().unwrap());
        let inputs = view
            .active()
            .unwrap()
            .analysis
            .node(join)
            .unwrap()
            .children
            .clone();
        assert_eq!(inputs.len(), 2);
        view.set_view_mode(PlanViewMode::Diagram, cx);
        assert_eq!(view.diagram.orientation, DiagramOrientation::RootTop);

        // The first arrow lands on the root.
        assert!(view.diagram_key("down", cx));
        assert_eq!(view.selected_node, Some(root));
        assert!(view.diagram_key("down", cx));
        assert_eq!(view.selected_node, Some(join));
        assert!(view.diagram_key("down", cx));
        assert_eq!(view.selected_node, Some(inputs[0]));
        assert!(view.diagram_key("right", cx));
        assert_eq!(view.selected_node, Some(inputs[1]));
        // At the edge the key is still handled, and the selection stays.
        assert!(view.diagram_key("right", cx));
        assert_eq!(view.selected_node, Some(inputs[1]));
        assert!(view.diagram_key("left", cx));
        assert!(view.diagram_key("up", cx));
        assert_eq!(view.selected_node, Some(join));

        // Enter folds; a folded operator has no way down.
        assert!(view.diagram_key("enter", cx));
        assert!(view.diagram.collapsed.contains(&join));
        assert!(view.diagram_key("down", cx));
        assert_eq!(view.selected_node, Some(join));
        assert!(view.diagram_key("space", cx));
        assert!(!view.diagram.collapsed.contains(&join));

        // Root on the left turns the arrows with the drawing.
        view.flip_diagram(cx);
        assert_eq!(view.diagram.orientation, DiagramOrientation::RootLeft);
        assert!(view.diagram_key("right", cx));
        assert_eq!(view.selected_node, Some(inputs[0]));
        assert!(view.diagram_key("down", cx));
        assert_eq!(view.selected_node, Some(inputs[1]));
        assert!(view.diagram_key("left", cx));
        assert_eq!(view.selected_node, Some(join));

        let zoom = view.diagram.zoom();
        assert!(view.diagram_key("+", cx));
        assert!(view.diagram.zoom() > zoom);
        assert!(view.diagram_key("escape", cx));
        assert_eq!(view.selected_node, None);
        assert!(!view.diagram_key("escape", cx), "nothing left to clear");
        assert!(!view.diagram_key("x", cx));
    });
}

#[gpui::test]
fn summary_keys_step_through_the_story(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        assert!(view.summary_key("down", cx));
        let first = view.selected_node.expect("the first step");
        assert!(view.summary_key("down", cx));
        let second = view.selected_node.expect("the second step");
        assert_ne!(first, second);
        assert!(view.summary_key("up", cx));
        assert_eq!(view.selected_node, Some(first));
        assert!(view.summary_key("enter", cx));
        assert_eq!(view.view_mode, PlanViewMode::Diagram);
        assert_eq!(view.selected_node, Some(first));
    });
}

#[gpui::test]
fn search_steps_through_matches_and_wraps(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    cx.simulate_resize(size(px(1200.0), px(800.0)));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        view.set_view_mode(PlanViewMode::Diagram, cx);
        view.set_diagram_search("seq scan".into(), cx);
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        let matches = view
            .diagram
            .matches
            .clone()
            .expect("painting resolves the search");
        assert_eq!(matches.ordered.len(), 2);
        view.step_search(true, cx);
        assert_eq!(view.selected_node, Some(matches.ordered[0]));
        view.step_search(true, cx);
        assert_eq!(view.selected_node, Some(matches.ordered[1]));
        view.step_search(true, cx);
        assert_eq!(view.selected_node, Some(matches.ordered[0]), "wraps");
        view.step_search(false, cx);
        assert_eq!(view.selected_node, Some(matches.ordered[1]));
    });
}

#[gpui::test]
fn diagram_click_selects_and_fold_toggle_folds(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        view.set_view_mode(PlanViewMode::Diagram, cx);
    });
    cx.run_until_parked();
    let (join, at) = view.update(cx, |view, _| {
        let (_, join, _) = join_ids(view.active().unwrap());
        (join, view.diagram_node_center(join))
    });
    let at = at.expect("the diagram was painted");
    cx.simulate_click(at, Modifiers::none());
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert_eq!(view.selected_node, Some(join));
        assert!(view.diagram.collapsed.is_empty());
    });

    let fold = view
        .update(cx, |view, _| view.diagram_fold_center(join))
        .expect("the join has a fold toggle");
    cx.simulate_mouse_down(fold, MouseButton::Left, Modifiers::none());
    cx.simulate_mouse_up(fold, MouseButton::Left, Modifiers::none());
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert!(view.diagram.collapsed.contains(&join));
    });
}

#[gpui::test]
fn operator_table_selection_follows_the_shared_selection(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    cx.simulate_resize(size(px(1400.0), px(900.0)));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Actual, "", join_plans(), cx);
        let (_, join, _) = join_ids(view.active().unwrap());
        view.select_node(Some(join), cx);
        view.set_view_mode(PlanViewMode::Operators, cx);
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        let (_, join, _) = join_ids(view.active().unwrap());
        let position = view
            .active()
            .unwrap()
            .table
            .iter()
            .position(|row| row.node_id == join)
            .unwrap();
        let grid = view.table.grid().expect("the grid was built").clone();
        assert_eq!(
            crate::tool_dialog::selected_source_row(&grid, cx),
            Some(position),
            "the grid opens on the operator selected elsewhere"
        );
    });
}

#[gpui::test]
fn every_view_renders_for_measured_and_estimated_plans(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    for (width, plans) in [
        (1400.0, join_plans()),
        (520.0, join_plans()),
        (1400.0, vec![sample_plan()]),
        (1400.0, vec![sample_plan(), sample_plan()]),
    ] {
        cx.simulate_resize(size(px(width), px(700.0)));
        for mode in [
            PlanViewMode::Summary,
            PlanViewMode::Diagram,
            PlanViewMode::Operators,
            PlanViewMode::Breakdown,
        ] {
            let plans = plans.clone();
            view.update(cx, |view, cx| {
                install(view, PlanKind::Estimated, "", plans, cx);
                let root = view.active().unwrap().analysis.roots[0];
                view.select_node(Some(root), cx);
                view.set_view_mode(mode, cx);
            });
            cx.run_until_parked();
        }
    }
    // Comparison draws over whichever view is open.
    view.update(cx, |view, cx| {
        view.toggle_comparison(cx);
        assert!(view.comparison_mode);
    });
    cx.run_until_parked();
}

#[gpui::test]
fn import_failure_replaces_the_plan_and_a_later_import_recovers(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    cx.simulate_resize(size(px(900.0), px(600.0)));
    view.update(cx, |view, cx| {
        install(view, PlanKind::Estimated, "", vec![sample_plan()], cx);
        let error = match import_plan_document("<ShowPlanXML><StmtSimple>") {
            Err(sqlly_services::plans::PlanError::Import(error)) => *error,
            other => panic!("expected an import error, got {other:?}"),
        };
        view.set_import_failed(error, cx);
        assert!(view.active().is_none());
        assert_eq!(view.finding_count(), 0);
        assert!(matches!(view.status, PlanPaneStatus::ImportFailed(_)));
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "plan.txt".into(), join_plans(), cx);
        assert!(matches!(view.status, PlanPaneStatus::Ready));
        assert_eq!(view.statements.len(), 1);
        assert_eq!(view.captures.len(), 2, "imports join the capture history");
    });
}

/// A measured PostgreSQL plan with more operators than are analysed in place.
fn wide_plans() -> Vec<ExecutionPlan> {
    let mut text = String::from(
        "Append  (cost=0.00..6000.00 rows=600 width=4) (actual time=0.010..60.000 rows=600 loops=1)\n",
    );
    for table in 0..BACKGROUND_ANALYSIS_NODES + 100 {
        text.push_str(&format!(
            "  ->  Seq Scan on t{table}  (cost=0.00..10.00 rows=1 width=4) (actual time=0.001..0.100 rows=1 loops=1)\n"
        ));
    }
    text.push_str("Execution Time: 60.000 ms\n");
    import_plan_document(&text)
        .expect("the wide plan imports")
        .plans
}

#[gpui::test]
fn a_large_plan_is_analysed_in_the_background(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "wide.txt".into(), wide_plans(), cx);
        assert!(
            view.statements.is_empty(),
            "the analysis has not landed yet"
        );
    });
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert!(matches!(view.status, PlanPaneStatus::Ready));
        assert_eq!(view.statements.len(), 1);
        assert!(view.statements[0].analysis.nodes.len() > BACKGROUND_ANALYSIS_NODES);
    });
}

#[gpui::test]
fn a_newer_plan_wins_over_a_slow_analysis(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "wide.txt".into(), wide_plans(), cx);
        view.set_imported_plans(PlanKind::Actual, "plan.txt".into(), join_plans(), cx);
    });
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert_eq!(view.statements.len(), 1);
        assert!(
            view.statements[0].analysis.nodes.len() < 10,
            "the late wide analysis must not replace the newer plan"
        );
    });
}

/// A `query_context` that makes the background analysis panic, so the
/// failure path can be tested.
pub(super) const PANIC_IN_ANALYSIS: &str = "test: panic in analysis";

fn collect_events(
    view: &Entity<ExecutionPlanView>,
    cx: &mut gpui::VisualTestContext,
) -> std::rc::Rc<std::cell::RefCell<Vec<ExecutionPlanViewEvent>>> {
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let sink = events.clone();
    cx.update(|_window, cx| {
        cx.subscribe(view, move |_, event: &ExecutionPlanViewEvent, _| {
            sink.borrow_mut().push(event.clone());
        })
        .detach();
    });
    events
}

/// The status bar used to show the previous plan's finding count after a
/// large paste, because the count was read before the background analysis
/// landed. While it runs, the old plan's count and digest are gone and the
/// pane says it is analysing; when it lands, an event tells every reader of
/// the count to read it again.
#[gpui::test]
fn the_finding_count_waits_for_a_background_analysis(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    let events = collect_events(&view, cx);
    let expected = BatchPlans::build(wide_plans(), None, None)
        .statements
        .iter()
        .map(|statement| statement.findings.len())
        .sum::<usize>();
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "plan.txt".into(), join_plans(), cx);
        assert!(view.finding_count() > 0, "the first plan has findings");
    });
    cx.run_until_parked();
    events.borrow_mut().clear();
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "wide.txt".into(), wide_plans(), cx);
        assert!(view.is_analysing());
        assert_eq!(view.finding_count(), 0, "no stale count while analysing");
        assert!(view.ai_explain_prompt().is_none(), "no stale digest");
    });
    assert!(
        events.borrow().is_empty(),
        "nothing is announced before the analysis lands"
    );
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert!(matches!(view.status, PlanPaneStatus::Ready));
        assert_eq!(view.finding_count(), expected);
    });
    assert_eq!(
        events.borrow().as_slice(),
        &[ExecutionPlanViewEvent::AnalysisFinished { failed: false }]
    );
}

#[gpui::test]
fn a_failed_background_analysis_says_so_instead_of_spinning(cx: &mut gpui::TestAppContext) {
    ensure_components_initialized(cx);
    let (view, cx) = cx.add_window_view(|_window, cx| ExecutionPlanView::new(cx));
    let events = collect_events(&view, cx);
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, PANIC_IN_ANALYSIS.into(), wide_plans(), cx);
        assert!(view.is_analysing());
    });
    cx.run_until_parked();
    view.update(cx, |view, _| {
        assert_eq!(view.status, PlanPaneStatus::AnalysisFailed);
        assert!(!view.is_analysing());
        assert_eq!(view.finding_count(), 0);
    });
    assert_eq!(
        events.borrow().as_slice(),
        &[ExecutionPlanViewEvent::AnalysisFinished { failed: true }]
    );
    // A later plan still installs.
    view.update(cx, |view, cx| {
        view.set_imported_plans(PlanKind::Actual, "plan.txt".into(), join_plans(), cx);
        assert!(matches!(view.status, PlanPaneStatus::Ready));
    });
}

/// The analysis marks the largest step even when nothing dominates; every
/// view labels that mark "Bottleneck", so it must follow the story's rule.
#[test]
fn only_a_step_the_story_calls_the_bottleneck_is_marked_as_one() {
    let even = statement(wide_plans().remove(0));
    assert!(even.story.bottleneck.is_some(), "the slowest step is named");
    assert_eq!(even.story.named_bottleneck(), None);
    assert!(
        even.analysis
            .nodes
            .iter()
            .all(|node| !node.flags.bottleneck)
    );
    assert!(even.story.steps.iter().all(|step| !step.is_bottleneck));
    assert!(even.table.iter().all(|row| !row.flags.bottleneck));
    if let Some(flame) = &even.flame {
        assert!(flame.cells.iter().all(|cell| !cell.flags.bottleneck));
    }

    let join = join_statement();
    let named = join.story.named_bottleneck().expect("one step dominates");
    let marked: Vec<i32> = join
        .analysis
        .nodes
        .iter()
        .filter(|node| node.flags.bottleneck)
        .map(|node| node.node_id)
        .collect();
    assert_eq!(marked, vec![named]);
    assert!(
        join.story
            .steps
            .iter()
            .any(|step| step.is_bottleneck && step.node_id == named)
    );
}

/// A card's mark comes from the findings only. The parsers attach scan
/// warnings to every sequential scan; a mark for those would contradict a
/// Findings list that says there is nothing to fix.
#[test]
fn diagram_marks_follow_the_findings_not_parser_warnings() {
    let wide = statement(wide_plans().remove(0));
    assert!(
        wide.plan.nodes.iter().any(|node| !node.warnings.is_empty()),
        "the parser attached scan warnings"
    );
    let scene = diagram::DiagramState::default().ensure_scene(&wide);
    for node in &scene.diagram.nodes {
        assert_eq!(
            scene.card_severity(node.node_id),
            wide.node_severity(node.node_id),
            "node {}",
            node.node_id
        );
    }
    if wide.findings.is_empty() {
        assert!(
            scene
                .diagram
                .nodes
                .iter()
                .all(|node| scene.card_severity(node.node_id).is_none())
        );
    }
}

/// German plan text writes German numbers; the English text is unchanged.
#[test]
fn plan_numbers_follow_the_interface_language() {
    let english = join_statement();
    let headline =
        |statement: &StatementPlans| crate::localization::message(&statement.story.headline);
    assert!(
        headline(&english).contains("3.15 s"),
        "{}",
        headline(&english)
    );
    let _german = crate::localization::test_language(sqlly_i18n::Language::German);
    let german = join_statement();
    let text = headline(&german);
    assert!(text.contains("3,15 s"), "{text}");
    let removed: Vec<String> = german
        .story
        .steps
        .iter()
        .flat_map(|step| step.notes.iter())
        .map(|note| crate::localization::message(&note.message))
        .collect();
    assert!(
        removed.iter().any(|note| note.contains("7,9M")),
        "{removed:?}"
    );
}

/// The Learn table is English. Outside English the link reads as a
/// translated kind of page plus its address, never an English title.
#[test]
fn learn_links_are_not_shown_in_english_in_other_languages() {
    let link = sqlly_services::plans::engine_links_for_finding(
        "table_scan",
        sqlly_services::plans::PlanEngine::SqlServer,
    )
    .into_iter()
    .next()
    .expect("table scans have an article");
    let (title, learn) = summary::learn_link_text(link);
    assert_eq!(title.as_ref(), link.title);
    assert_eq!(learn.as_ref(), link.learn);
    let _german = crate::localization::test_language(sqlly_i18n::Language::German);
    let (title, learn) = summary::learn_link_text(link);
    assert_ne!(title.as_ref(), link.title);
    assert!(title.contains("sqlly.app"), "{title}");
    assert_eq!(learn.as_ref(), format!("sqlly.app{}", link.path));
}
