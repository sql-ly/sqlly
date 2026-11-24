use super::*;
use crate::plans::model::ExecutionPlan;
use crate::plans::plan_fixtures::*;

fn report(before: &ExecutionPlan, after: &ExecutionPlan) -> PlanChangeReport {
    explain_plan_changes(
        std::slice::from_ref(before),
        std::slice::from_ref(after),
        DEFAULT_MAX_COMPARISON_NODES,
    )
    .expect("comparable")
}

fn text(report: &PlanChangeReport) -> String {
    let mut out = format!("{}\n", report.headline.fallback());
    for statement in &report.statements {
        if report.statements.len() > 1 {
            out.push_str(&format!(
                "[{}] {}\n",
                statement.statement_index + 1,
                statement.headline.fallback()
            ));
        }
        for change in &statement.changes {
            out.push_str(&format!(
                "- {}/{}: {}\n",
                change.kind.key(),
                change.effect.key(),
                change.message.fallback()
            ));
        }
    }
    out
}

/// [`pg_join`] after adding an index on `orders(status)`: the sequential
/// scan becomes an index scan that takes 4 ms instead of 70.
pub(super) fn pg_join_indexed() -> ExecutionPlan {
    let mut plan = pg_join();
    let orders = &mut plan.nodes[2];
    orders.physical_op = "Index Scan".into();
    orders.logical_op = "Index Scan".into();
    orders.index_name = Some("orders_status_idx".into());
    orders.index_cond = orders.filter.take();
    orders.rows_removed_by_filter = None;
    orders.estimated_rows = 950.0;
    orders.estimated_cost = 40.0;
    orders.actual_time_ms = Some(4.0);
    plan.nodes[1].actual_time_ms = Some(24.0);
    plan.nodes[1].estimated_cost = 140.0;
    plan.nodes[0].actual_time_ms = Some(34.0);
    plan.nodes[0].estimated_cost = 240.0;
    plan.total_subtree_cost = 240.0;
    plan.execution_time_ms = Some(34.4);
    plan
}

/// [`pg_join`] with the hash join replaced by a nested loop that probes
/// `customers` once per order.
fn pg_join_as_nested_loop() -> ExecutionPlan {
    let mut plan = pg_join();
    plan.nodes[1].physical_op = "Nested Loop".into();
    plan.nodes[1].logical_op = "Nested Loop".into();
    // Drop the Hash node: customers hangs directly off the join.
    plan.nodes.remove(3);
    plan.edges
        .retain(|edge| edge.from_node != 4 && edge.to_node != 4);
    plan.edges.push(crate::plans::PlanEdge {
        from_node: 2,
        to_node: 5,
    });
    let customers = plan
        .nodes
        .iter_mut()
        .find(|node| node.id == 5)
        .expect("customers");
    customers.loops = Some(1000.0);
    customers.actual_rows = Some(1.0);
    customers.estimated_rows = 1.0;
    customers.actual_time_ms = Some(150.0);
    plan.nodes[1].actual_time_ms = Some(230.0);
    plan.nodes[0].actual_time_ms = Some(240.0);
    plan.execution_time_ms = Some(240.6);
    plan
}

#[test]
#[ignore = "prints the comparison text for reading; not an assertion"]
fn print_change_reports() {
    let base = pg_join();
    let mut swapped = pg_join();
    // Probe and build sides trade places.
    swapped.edges = vec![(1, 2), (2, 5), (2, 4), (4, 3)]
        .into_iter()
        .map(|(from_node, to_node)| crate::plans::PlanEdge { from_node, to_node })
        .collect();
    let mut spilled = pg_join();
    spilled.nodes[0].sort_space_type = Some("Disk".into());
    spilled.nodes[0].sort_space_kb = Some(4096.0);
    spilled.nodes[0].actual_time_ms = Some(160.0);
    spilled.execution_time_ms = Some(160.2);
    let mut fixed = pg_join();
    fixed.nodes[2].estimated_rows = 1000.0;
    let mut extra = pg_join();
    extra.nodes.push(ran(
        est(scan(6, "Seq Scan", "regions"), 5.0, 1.0),
        5.0,
        1.0,
        0.1,
    ));
    extra.edges.push(crate::plans::PlanEdge {
        from_node: 4,
        to_node: 6,
    });
    let mut index_swap = pg_join_indexed();
    index_swap.nodes[2].index_name = Some("orders_status_created_idx".into());
    let cases: Vec<(&str, ExecutionPlan, ExecutionPlan)> = vec![
        ("identical", base.clone(), base.clone()),
        ("index added", base.clone(), pg_join_indexed()),
        ("index removed", pg_join_indexed(), base.clone()),
        ("index swapped", pg_join_indexed(), index_swap),
        (
            "hash -> nested loop",
            base.clone(),
            pg_join_as_nested_loop(),
        ),
        ("join order", base.clone(), swapped),
        ("spill", base.clone(), spilled),
        ("estimate fixed", base.clone(), fixed),
        ("table added", base.clone(), extra),
        ("estimated only", pg_join_estimated(), {
            let mut plan = pg_join_indexed();
            for node in &mut plan.nodes {
                node.actual_rows = None;
                node.loops = None;
                node.actual_time_ms = None;
            }
            plan.has_actuals = false;
            plan.execution_time_ms = None;
            plan
        }),
        ("estimated vs actual", pg_join_estimated(), base.clone()),
    ];
    for (name, before, after) in &cases {
        println!("===== {name} =====\n{}", text(&report(before, after)));
    }
    let batch = explain_plan_changes(
        &[base.clone(), pg_nested_loop()],
        &[pg_join_indexed(), pg_nested_loop()],
        DEFAULT_MAX_COMPARISON_NODES,
    )
    .expect("batch");
    println!("===== batch =====\n{}", text(&batch));
}

fn kinds(report: &PlanChangeReport) -> Vec<PlanChangeKind> {
    report.changes().map(|change| change.kind).collect()
}

fn find(report: &PlanChangeReport, kind: PlanChangeKind) -> &PlanChange {
    report
        .changes()
        .find(|change| change.kind == kind)
        .unwrap_or_else(|| panic!("no {kind:?} change in:\n{}", text(report)))
}

#[test]
fn identical_plans_report_no_changes() {
    let report = report(&pg_join(), &pg_join());
    assert_eq!(
        report.headline.fallback(),
        "Both plans ran in about the same time: 100 ms for the baseline, 100 ms for the new plan."
    );
    assert_eq!(report.statements.len(), 1);
    assert!(report.statements[0].same_shape);
    assert_eq!(
        report.statements[0].headline_effect,
        PlanChangeEffect::Neutral
    );
    assert_eq!(report.changes().count(), 0);
}

#[test]
fn an_added_index_is_explained_in_words() {
    let report = report(&pg_join(), &pg_join_indexed());
    assert_eq!(
        report.headline.fallback(),
        "The new plan ran in 34.4 ms, 2.9 times faster than the baseline's 100 ms."
    );
    assert_eq!(
        report.statements[0].headline_effect,
        PlanChangeEffect::Better
    );
    assert!(!report.statements[0].same_shape);
    assert_eq!(
        kinds(&report),
        vec![
            PlanChangeKind::Cost,
            PlanChangeKind::Bottleneck,
            PlanChangeKind::AccessPath,
            PlanChangeKind::Estimate,
        ]
    );
    let access = find(&report, PlanChangeKind::AccessPath);
    assert_eq!(
        access.message.fallback(),
        "orders o is now searched through the index orders_status_idx. The baseline scanned it."
    );
    assert_eq!(access.baseline_node_id, Some(3));
    assert_eq!(access.comparison_node_id, Some(3));
    assert_eq!(access.statement_index, 0);
    let cost = find(&report, PlanChangeKind::Cost);
    assert_eq!(cost.effect, PlanChangeEffect::Better);
    assert_eq!(
        cost.message.fallback(),
        "The optimizer's cost estimate went from 500 to 240, 2.1 times lower."
    );
    let estimate = find(&report, PlanChangeKind::Estimate);
    assert_eq!(estimate.effect, PlanChangeEffect::Better);
    assert_eq!(
        estimate.message.fallback(),
        "The row estimate for orders o was off by a factor of 20 in the baseline and is close now."
    );
}

#[test]
fn the_reverse_direction_reads_as_a_regression() {
    let report = report(&pg_join_indexed(), &pg_join());
    assert_eq!(
        report.headline.fallback(),
        "The new plan ran in 100 ms, 2.9 times slower than the baseline's 34.4 ms."
    );
    assert_eq!(
        report.statements[0].headline_effect,
        PlanChangeEffect::Worse
    );
    assert_eq!(
        find(&report, PlanChangeKind::Cost).effect,
        PlanChangeEffect::Worse
    );
    assert_eq!(
        find(&report, PlanChangeKind::AccessPath).message.fallback(),
        "orders o is now scanned. The baseline searched it through the index orders_status_idx."
    );
    assert_eq!(
        find(&report, PlanChangeKind::Estimate).effect,
        PlanChangeEffect::Worse
    );
}

#[test]
fn a_different_index_is_an_index_choice_change() {
    let mut other = pg_join_indexed();
    other.nodes[2].index_name = Some("orders_status_created_idx".into());
    let report = report(&pg_join_indexed(), &other);
    assert_eq!(kinds(&report), vec![PlanChangeKind::IndexChoice]);
    assert_eq!(
        find(&report, PlanChangeKind::IndexChoice)
            .message
            .fallback(),
        "orders o is now read through the index orders_status_created_idx instead of orders_status_idx."
    );
}

#[test]
fn a_join_algorithm_swap_names_both_operators_and_the_tables() {
    let report = report(&pg_join(), &pg_join_as_nested_loop());
    let join = find(&report, PlanChangeKind::JoinAlgorithm);
    assert_eq!(
        join.message.fallback(),
        "The join of orders o and customers c changed from Hash Join to Nested Loop."
    );
    assert_eq!(join.baseline_node_id, Some(2));
    assert_eq!(join.comparison_node_id, Some(2));
    assert!(join.kind.is_structural());
    assert_eq!(
        report.statements[0].headline_effect,
        PlanChangeEffect::Worse
    );
    // The inner side now runs 1,000 times and returns 1,000 rows in all.
    assert_eq!(
        find(&report, PlanChangeKind::Rows).message.fallback(),
        "customers c returned 200 rows in the baseline and 1,000 rows in the new plan."
    );
}

#[test]
fn a_join_order_change_lists_both_orders() {
    let mut swapped = pg_join();
    swapped.edges = [(1, 2), (2, 5), (2, 4), (4, 3)]
        .into_iter()
        .map(|(from_node, to_node)| crate::plans::PlanEdge { from_node, to_node })
        .collect();
    let report = report(&pg_join(), &swapped);
    assert_eq!(kinds(&report), vec![PlanChangeKind::JoinOrder]);
    assert_eq!(
        find(&report, PlanChangeKind::JoinOrder).message.fallback(),
        "Tables are now read in the order orders o, customers c. The baseline read them in the order customers c, orders o."
    );
}

#[test]
fn spills_and_added_tables_are_reported() {
    let mut spilled = pg_join();
    spilled.nodes[0].sort_space_type = Some("Disk".into());
    spilled.nodes[0].sort_space_kb = Some(4096.0);
    let report_spill = report(&pg_join(), &spilled);
    let spill = find(&report_spill, PlanChangeKind::Spill);
    assert_eq!(spill.effect, PlanChangeEffect::Worse);
    assert_eq!(
        find(&report(&spilled, &pg_join()), PlanChangeKind::Spill).effect,
        PlanChangeEffect::Better
    );

    let mut extra = pg_join();
    extra.nodes.push(ran(
        est(scan(6, "Seq Scan", "regions"), 5.0, 1.0),
        5.0,
        1.0,
        0.1,
    ));
    extra.edges.push(crate::plans::PlanEdge {
        from_node: 4,
        to_node: 6,
    });
    let added = report(&pg_join(), &extra);
    assert_eq!(
        find(&added, PlanChangeKind::Table).message.fallback(),
        "The new plan also reads regions."
    );
    let removed = report(&extra, &pg_join());
    assert!(
        find(&removed, PlanChangeKind::Table)
            .message
            .fallback()
            .contains("regions")
    );
}

#[test]
fn estimated_plans_are_compared_by_cost() {
    let mut indexed = pg_join_indexed();
    for node in &mut indexed.nodes {
        node.actual_rows = None;
        node.loops = None;
        node.actual_time_ms = None;
    }
    indexed.has_actuals = false;
    indexed.execution_time_ms = None;
    let report = report(&pg_join_estimated(), &indexed);
    assert_eq!(
        report.headline.fallback(),
        "The optimizer's cost estimate went from 500 to 240, 2.1 times lower."
    );
    assert!(
        !kinds(&report).contains(&PlanChangeKind::Cost),
        "not said twice"
    );
    assert_eq!(
        find(&report, PlanChangeKind::Rows).message.fallback(),
        "The optimizer expected 50 rows from orders o in the baseline and expects 950 rows in the new plan."
    );
}

#[test]
fn batches_get_a_headline_and_one_entry_per_statement() {
    let report = explain_plan_changes(
        &[pg_join(), pg_nested_loop()],
        &[pg_join_indexed(), pg_nested_loop()],
        DEFAULT_MAX_COMPARISON_NODES,
    )
    .expect("comparable");
    assert_eq!(
        report.headline.fallback(),
        "The new batch ran in 94.6 ms, 41% less than the baseline's 161 ms."
    );
    assert_eq!(report.statements.len(), 2);
    assert_eq!(report.statements[1].statement_index, 1);
    assert!(report.statements[1].same_shape);
    assert!(report.statements[1].changes.is_empty());
    assert!(report.changes().all(|change| change.statement_index == 0));
}

#[test]
fn batches_that_cannot_be_compared_are_refused() {
    let one = [pg_join()];
    let two = [pg_join(), pg_nested_loop()];
    assert_eq!(
        explain_plan_changes(&[], &one, DEFAULT_MAX_COMPARISON_NODES),
        Err(PlanComparisonError::Empty)
    );
    assert_eq!(
        explain_plan_changes(&one, &two, DEFAULT_MAX_COMPARISON_NODES),
        Err(PlanComparisonError::StatementCount {
            baseline: 1,
            comparison: 2
        })
    );
    assert!(matches!(
        explain_plan_changes(&one, &one, 3),
        Err(PlanComparisonError::TooLarge { .. })
    ));
}

#[test]
fn kind_and_effect_keys_are_unique() {
    let kinds = [
        PlanChangeKind::Time,
        PlanChangeKind::Cost,
        PlanChangeKind::Bottleneck,
        PlanChangeKind::JoinOrder,
        PlanChangeKind::JoinAlgorithm,
        PlanChangeKind::AccessPath,
        PlanChangeKind::IndexChoice,
        PlanChangeKind::Lookup,
        PlanChangeKind::Table,
        PlanChangeKind::Sort,
        PlanChangeKind::Parallelism,
        PlanChangeKind::Spill,
        PlanChangeKind::Estimate,
        PlanChangeKind::Rows,
    ];
    let mut keys: Vec<&str> = kinds.iter().map(|kind| kind.key()).collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), kinds.len());
    assert_ne!(
        PlanChangeEffect::Better.key(),
        PlanChangeEffect::Worse.key()
    );
}

fn sample_plan(name: &str) -> ExecutionPlan {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/plan_samples")
        .join(name);
    let text = std::fs::read_to_string(path).expect("sample reads");
    crate::plans::import::import_plan_document(&text)
        .expect("sample parses")
        .plans
        .remove(0)
}

#[test]
fn a_table_reached_through_a_bitmap_is_still_read() {
    // Before: Seq Scan on orders o. After: Bitmap Heap Scan on orders o over
    // a Bitmap Index Scan. The table is read by both plans; what changed is
    // how.
    let report = report(
        &sample_plan("postgresql-lookup.json"),
        &sample_plan("postgresql-lookup-indexed.json"),
    );
    let text = text(&report);
    assert!(!kinds(&report).contains(&PlanChangeKind::Table), "{text}");
    assert!(!kinds(&report).contains(&PlanChangeKind::Lookup), "{text}");
    assert!(
        report.statements[0].changes.iter().any(|change| {
            change.kind == PlanChangeKind::AccessPath
                && change.message.fallback()
                    == "orders o is now searched through the index orders_customer_id_idx. The baseline scanned it."
        }),
        "{text}"
    );
}

#[test]
fn an_estimate_against_an_executed_plan_says_what_is_compared() {
    let estimated = pg_join_estimated();
    let executed = pg_join_indexed();
    let forward = report(&estimated, &executed);
    let first = &forward.statements[0].changes[0];
    assert_eq!(first.kind, PlanChangeKind::Capture);
    assert_eq!(first.effect, PlanChangeEffect::Neutral);
    assert!(
        first
            .message
            .fallback()
            .starts_with("The baseline is an estimated plan and the new plan was executed"),
        "{}",
        text(&forward)
    );
    // Nothing measured is set against an estimate.
    assert!(!kinds(&forward).contains(&PlanChangeKind::Time));
    assert!(!kinds(&forward).contains(&PlanChangeKind::Estimate));
    assert!(!kinds(&forward).contains(&PlanChangeKind::Rows));
    assert!(forward.headline.fallback().contains("cost estimate"));

    let backward = report(&executed, &estimated);
    assert!(
        backward.statements[0].changes[0]
            .message
            .fallback()
            .starts_with("The baseline was executed and the new plan is an estimated plan"),
        "{}",
        text(&backward)
    );
    // Two executed plans, or two estimates, need no such line.
    assert!(!kinds(&report(&pg_join(), &executed)).contains(&PlanChangeKind::Capture));
    assert!(!kinds(&report(&estimated, &estimated)).contains(&PlanChangeKind::Capture));
}
