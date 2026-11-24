use super::*;
use crate::plans::analysis::analyze_plan;
use crate::plans::plan_fixtures::*;

fn ids(rows: &[OperatorTableRow]) -> Vec<i32> {
    rows.iter().map(|row| row.node_id).collect()
}

#[test]
fn the_operator_table_lists_every_operator_in_plan_order() {
    let analysis = analyze_plan(&pg_join());
    let rows = plan_operator_table(&analysis);
    assert_eq!(ids(&rows), vec![1, 2, 3, 4, 5]);
    let orders = &rows[2];
    assert_eq!(orders.operator, "Seq Scan");
    assert_eq!(orders.object.as_deref(), Some("orders o"));
    assert_eq!(orders.step, 3, "third in execution order");
    assert_eq!(orders.depth, 2);
    assert_eq!(orders.self_time_ms, Some(70.0));
    assert_eq!(orders.inclusive_time_ms, Some(70.0));
    assert_eq!(orders.self_cost, Some(300.0));
    assert_eq!(orders.rows, Some(1000.0));
    assert_eq!(orders.rows_in, Some(100_000.0));
    assert_eq!(orders.estimated_rows, Some(50.0));
    assert_eq!(orders.estimate_factor, Some(20.0));
    assert_eq!(orders.rows_removed, Some(99_000.0));
    assert!(orders.flags.bottleneck);
    assert_eq!(rows[4].index.as_deref(), Some("customers_pkey"));
    assert_eq!(rows[0].step, 5, "the root runs last");
}

#[test]
fn columns_without_data_are_left_out() {
    let measured = plan_operator_table(&analyze_plan(&pg_join()));
    let columns = operator_table_columns(&measured);
    for column in [
        OperatorTableColumn::Step,
        OperatorTableColumn::Operator,
        OperatorTableColumn::Object,
        OperatorTableColumn::SelfTime,
        OperatorTableColumn::Rows,
        OperatorTableColumn::EstimateFactor,
        OperatorTableColumn::RowsRemoved,
    ] {
        assert!(columns.contains(&column), "{column:?}");
    }
    assert!(!columns.contains(&OperatorTableColumn::Io));
    assert!(!columns.contains(&OperatorTableColumn::Memory));

    let estimated = plan_operator_table(&analyze_plan(&pg_join_estimated()));
    let columns = operator_table_columns(&estimated);
    assert!(columns.contains(&OperatorTableColumn::SelfCost));
    assert!(columns.contains(&OperatorTableColumn::EstimatedRows));
    for column in [
        OperatorTableColumn::SelfTime,
        OperatorTableColumn::TotalTime,
        OperatorTableColumn::TimeShare,
        OperatorTableColumn::Rows,
        OperatorTableColumn::EstimateFactor,
        OperatorTableColumn::Loops,
    ] {
        assert!(!columns.contains(&column), "{column:?}");
    }
    // Keys are unique: renderers persist the sort column by key.
    let mut keys: Vec<&str> = OperatorTableColumn::ALL
        .iter()
        .map(|column| column.key())
        .collect();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(keys.len(), OperatorTableColumn::ALL.len());
}

#[test]
fn sorting_is_stable_and_keeps_missing_values_last() {
    let mut plan = pg_join();
    // The hash reports no row count.
    plan.nodes[3].actual_rows = None;
    plan.nodes[3].actual_rows_total = None;
    let analysis = analyze_plan(&plan);
    let mut rows = plan_operator_table(&analysis);

    sort_operator_table(&mut rows, OperatorTableColumn::SelfTime, true);
    // 70, then the two 10s in plan order, 8, 2.
    assert_eq!(ids(&rows), vec![3, 1, 5, 2, 4]);
    sort_operator_table(&mut rows, OperatorTableColumn::SelfTime, false);
    assert_eq!(ids(&rows), vec![4, 2, 1, 5, 3]);

    sort_operator_table(&mut rows, OperatorTableColumn::Rows, true);
    assert_eq!(*ids(&rows).last().expect("rows"), 4, "no value sorts last");
    // Stable: the three 1,000-row operators keep the order the previous
    // sort left them in.
    let tied: Vec<i32> = ids(&rows)
        .into_iter()
        .filter(|id| [1, 2, 3].contains(id))
        .collect();
    sort_operator_table(&mut rows, OperatorTableColumn::Rows, false);
    let mut expected = vec![5];
    expected.extend(tied);
    expected.push(4);
    assert_eq!(ids(&rows), expected);

    sort_operator_table(&mut rows, OperatorTableColumn::Operator, false);
    let names: Vec<&str> = rows.iter().map(|row| row.operator.as_str()).collect();
    assert_eq!(
        names,
        vec!["Hash", "Hash Join", "Index Scan", "Seq Scan", "Sort"]
    );

    sort_operator_table(&mut rows, OperatorTableColumn::Step, false);
    assert_eq!(ids(&rows), vec![5, 4, 3, 2, 1]);
}

#[test]
fn flame_bars_nest_inside_their_parent() {
    let analysis = analyze_plan(&pg_join());
    let flame = plan_flame_layout(&analysis, None).expect("timed plan");
    assert_eq!(flame.metric, PlanMetric::Time);
    assert_eq!(flame.total, 100.0);
    assert_eq!(flame.depth, 4);
    let cell = |id: i32| {
        flame
            .cells
            .iter()
            .find(|cell| cell.node_id == id)
            .expect("cell")
    };
    // Sort spans everything; the join is its 90 ms; orders the first 70 ms
    // of that, the hash the next 12.
    assert_eq!((cell(1).x, cell(1).width), (0.0, 1.0));
    assert!((cell(2).width - 0.9).abs() < 1e-6);
    assert!((cell(3).width - 0.7).abs() < 1e-6);
    assert!((cell(4).x - 0.7).abs() < 1e-6);
    assert!((cell(4).width - 0.12).abs() < 1e-6);
    assert!((cell(5).x - 0.7).abs() < 1e-6);
    assert!((cell(5).width - 0.1).abs() < 1e-6);
    assert_eq!(cell(2).self_value, 8.0);
    assert_eq!(cell(2).value, 90.0);
    assert_eq!(cell(3).label.fallback(), "Seq Scan on orders o");
    for cell in &flame.cells {
        let Some(parent) = analysis.node(cell.node_id).and_then(|node| node.parent_id) else {
            continue;
        };
        let parent = flame
            .cells
            .iter()
            .find(|other| other.node_id == parent)
            .expect("parent");
        assert_eq!(cell.depth, parent.depth + 1);
        assert!(cell.x >= parent.x - 1e-6);
        assert!(cell.x + cell.width <= parent.x + parent.width + 1e-6);
    }
}

#[test]
fn flame_falls_back_to_cost_and_refuses_what_the_plan_lacks() {
    let estimated = analyze_plan(&pg_join_estimated());
    let flame = plan_flame_layout(&estimated, None).expect("costed plan");
    assert_eq!(flame.metric, PlanMetric::Cost);
    assert_eq!(flame.total, 500.0);
    assert!(plan_flame_layout(&estimated, Some(PlanMetric::Time)).is_none());
    assert!(plan_flame_layout(&estimated, Some(PlanMetric::Rows)).is_none());
    let measured = analyze_plan(&pg_join());
    let by_cost = plan_flame_layout(&measured, Some(PlanMetric::Cost)).expect("cost");
    assert_eq!(by_cost.metric, PlanMetric::Cost);
    let bare = analyze_plan(&plan(
        crate::plans::PlanDialect::Sqlite,
        vec![op(1, "SCAN t")],
        &[],
    ));
    assert!(plan_flame_layout(&bare, None).is_none());
    assert!(plan_breakdown(&bare, 5).is_none());
}

#[test]
fn breakdown_ranks_operators_families_and_tables() {
    let analysis = analyze_plan(&pg_join());
    let breakdown = plan_breakdown(&analysis, 2).expect("timed plan");
    assert_eq!(breakdown.metric, PlanMetric::Time);
    assert_eq!(breakdown.total, 100.0);

    // Top two, then everything else (10 + 8 + 2).
    assert_eq!(breakdown.by_operator.len(), 3);
    assert_eq!(breakdown.by_operator[0].node_ids, vec![3]);
    assert_eq!(breakdown.by_operator[0].value, 70.0);
    assert!((breakdown.by_operator[0].share - 0.7).abs() < 1e-9);
    assert_eq!(
        breakdown.by_operator[1].node_ids,
        vec![1],
        "ties keep plan order"
    );
    let rest = &breakdown.by_operator[2];
    assert_eq!(rest.value, 20.0);
    assert_eq!(rest.category, None);
    assert_eq!(rest.node_ids.len(), 3);

    let tables: Vec<(&str, f64)> = breakdown
        .by_table
        .iter()
        .map(|entry| (entry.label.fallback(), entry.value))
        .collect();
    assert_eq!(tables, vec![("orders", 70.0), ("customers", 10.0)]);

    let scans = breakdown
        .by_category
        .iter()
        .find(|entry| entry.category == Some(OperatorCategory::Scan))
        .expect("scan family");
    assert_eq!(scans.value, 70.0);
    assert_eq!(
        breakdown.by_category[0].category,
        Some(OperatorCategory::Scan)
    );
    assert_eq!(
        breakdown.by_category.len(),
        2,
        "families are cut to the top two as well"
    );

    let all = plan_breakdown(&analysis, 50).expect("breakdown");
    let family_total: f64 = all.by_category.iter().map(|entry| entry.value).sum();
    assert!((family_total - 100.0).abs() < 1e-9);
    assert_eq!(
        all.by_operator.len(),
        5,
        "no 'everything else' line when all fit"
    );
    let estimated = plan_breakdown(&analyze_plan(&pg_join_estimated()), 3).expect("costed");
    assert_eq!(estimated.metric, PlanMetric::Cost);
    assert_eq!(estimated.by_operator[0].value, 300.0);
}
