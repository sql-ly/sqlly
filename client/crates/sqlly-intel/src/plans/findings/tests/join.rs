//! Joins and subqueries.

use super::*;
use crate::plans::PlanProperty;
use crate::plans::warning::PlanWarning;

fn both(node: PlanNode, estimated: f64, actual: f64, loops: f64, time_ms: f64) -> PlanNode {
    ran(est(node, estimated, 100.0), actual, loops, time_ms)
}

/// SQL Server nested loops with the engine's NoJoinPredicate warning:
/// `left` outer rows, `right` inner rows per execution.
fn mssql_cross(left: f64, right: f64, time_ms: f64) -> ExecutionPlan {
    let mut join = node(1, "Nested Loops", left * right, 1.0, time_ms);
    join.logical_op = "Inner Join".into();
    join.warnings.push(PlanWarning::NoJoinPredicate);
    let customers = read(2, "Clustered Index Scan", "Customers", left, 1.0, 5.0);
    // SQL Server reports the inner side's rows summed over its executions.
    let regions = read(
        3,
        "Clustered Index Scan",
        "Regions",
        right * left,
        left,
        50.0,
    );
    timed(
        plan(
            PlanDialect::SqlServer,
            vec![join, customers, regions],
            &[(1, 2), (1, 3)],
        ),
        time_ms,
    )
}

#[test]
fn no_join_fires_when_the_join_multiplies_its_inputs() {
    let findings = findings_for_plan(&mssql_cross(2_000.0, 500.0, 3_000.0));
    let finding = get(&findings, K::NoJoin);
    assert_eq!(variant(finding), "rows");
    assert_eq!(finding.severity, S::Critical);
    assert_eq!(finding.node_ids, vec![1, 2, 3]);
    assert!(
        finding.summary.fallback().contains("1.00M"),
        "{:?}",
        finding.summary
    );

    // The same plan on a statement that took 200 ms is not critical.
    let quick = findings_for_plan(&mssql_cross(2_000.0, 500.0, 200.0));
    assert_eq!(get(&quick, K::NoJoin).severity, S::Warning);
}

#[test]
fn no_join_is_silent_for_a_one_row_side() {
    // CROSS JOIN with a one-row settings table is deliberate and free.
    assert_none(
        &findings_for_plan(&mssql_cross(200_000.0, 1.0, 3_000.0)),
        K::NoJoin,
    );
    assert_none(
        &findings_for_plan(&mssql_cross(20.0, 10.0, 3_000.0)),
        K::NoJoin,
    );
}

#[test]
fn an_estimated_cartesian_join_is_low_confidence() {
    let join = est(op(1, "MERGE JOIN CARTESIAN"), 4_000_000.0, 9_000.0);
    let customers = est(scan(2, "TABLE ACCESS FULL", "CUSTOMERS"), 2_000.0, 20.0);
    let buffer = est(op(3, "BUFFER SORT"), 2_000.0, 8_000.0);
    let regions = est(scan(4, "TABLE ACCESS FULL", "REGIONS"), 2_000.0, 4.0);
    let plan = plan(
        PlanDialect::Oracle,
        vec![join, customers, buffer, regions],
        &[(1, 2), (1, 3), (3, 4)],
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NoJoin);
    assert_eq!((finding.severity, finding.confidence), (S::Info, C::Low));
    assert!(has_note(finding, "common.note.estimated_only"));
}

/// PostgreSQL nested loop that scans all of `orders` once per customer.
fn pg_repeated_scan(loops: f64) -> ExecutionPlan {
    let mut join = both(op(1, "Nested Loop"), loops * 2.0, loops * 2.0, 1.0, 4_000.0);
    join.join_type = Some("Inner".into());
    let mut customers = both(scan(2, "Seq Scan", "customers"), loops, loops, 1.0, 5.0);
    customers.alias = Some("c".into());
    let mut orders = both(scan(3, "Seq Scan", "orders"), 2.0, 2.0, loops, 3_900.0);
    orders.alias = Some("o".into());
    orders.filter = Some("(o.customer_id = c.id)".into());
    // Totals over all loops, as the parser stores them.
    orders.rows_removed_by_filter = Some(4_998.0 * loops);
    timed(
        plan(
            PlanDialect::Postgres,
            vec![join, customers, orders],
            &[(1, 2), (1, 3)],
        ),
        4_000.0,
    )
}

#[test]
fn nested_loop_volume_fires_for_a_full_scan_repeated_per_outer_row() {
    let findings = findings_for_plan(&pg_repeated_scan(500.0));
    let finding = get(&findings, K::NestedLoopVolume);
    assert_eq!(variant(finding), "inner_scan");
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_customer_id\" ON \"orders\" (\"customer_id\");"
    );
    assert_eq!(finding.node_ids, vec![1, 3]);
    assert!(
        evidence(finding).contains("Executions: 500"),
        "{}",
        evidence(finding)
    );
    // The scan is reported once, as the loop.
    assert_none(&findings, K::TableScan);

    // Three executions of the same scan is a small plan.
    assert_none(
        &findings_for_plan(&pg_repeated_scan(3.0)),
        K::NestedLoopVolume,
    );
}

/// Nested loop whose index lookup ran `actual` times where `planned` were
/// expected.
fn pg_loop_underestimate(planned: f64, actual: f64) -> ExecutionPlan {
    let mut join = both(op(1, "Nested Loop"), planned, actual, 1.0, 2_000.0);
    join.join_type = Some("Inner".into());
    let mut customers = both(scan(2, "Seq Scan", "customers"), planned, actual, 1.0, 50.0);
    customers.filter = Some("(tier = 'gold'::text)".into());
    customers.rows_removed_by_filter = Some(10.0);
    let mut orders = both(scan(3, "Index Scan", "orders"), 1.0, 1.0, actual, 1_900.0);
    orders.index_name = Some("orders_customer_id_idx".into());
    orders.index_cond = Some("(customer_id = customers.id)".into());
    timed(
        plan(
            PlanDialect::Postgres,
            vec![join, customers, orders],
            &[(1, 2), (1, 3)],
        ),
        2_000.0,
    )
}

#[test]
fn nested_loop_volume_fires_when_the_loop_ran_far_more_often_than_planned() {
    let findings = findings_for_plan(&pg_loop_underestimate(20.0, 60_000.0));
    let finding = get(&findings, K::NestedLoopVolume);
    assert_eq!(variant(finding), "underestimate");
    assert_eq!(sql(finding), "ANALYZE \"customers\";");
    assert_eq!(finding.node_ids, vec![1, 2, 3]);

    // 60,000 planned and 60,000 run: the loop is what the optimizer chose.
    assert_none(
        &findings_for_plan(&pg_loop_underestimate(60_000.0, 60_000.0)),
        K::NestedLoopVolume,
    );
}

fn pg_hash_build(planned: f64, built: f64, probe: f64) -> ExecutionPlan {
    let mut join = both(op(1, "Hash Join"), probe, probe, 1.0, 1_500.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(c.id = o.customer_id)".into());
    let mut customers = both(scan(2, "Seq Scan", "customers"), probe, probe, 1.0, 5.0);
    customers.alias = Some("c".into());
    let hash = both(op(3, "Hash"), planned, built, 1.0, 900.0);
    let mut orders = both(scan(4, "Seq Scan", "orders"), planned, built, 1.0, 600.0);
    orders.alias = Some("o".into());
    orders.filter = Some("(o.status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(100.0);
    timed(
        plan(
            PlanDialect::Postgres,
            vec![join, customers, hash, orders],
            &[(1, 2), (1, 3), (3, 4)],
        ),
        1_500.0,
    )
}

#[test]
fn hash_build_side_fires_when_the_larger_input_was_hashed_by_mistake() {
    let findings = findings_for_plan(&pg_hash_build(300.0, 900_000.0, 2_000.0));
    let finding = get(&findings, K::HashBuildSide);
    assert_eq!(variant(finding), "larger");
    assert_eq!(sql(finding), "ANALYZE \"orders\";");
    assert_eq!(finding.node_ids, vec![1, 3, 2]);

    // The optimizer knew the size and still built this side.
    assert_none(
        &findings_for_plan(&pg_hash_build(900_000.0, 900_000.0, 2_000.0)),
        K::HashBuildSide,
    );
    // The build side is the smaller one.
    assert_none(
        &findings_for_plan(&pg_hash_build(300.0, 900_000.0, 5_000_000.0)),
        K::HashBuildSide,
    );
}

fn pg_fanout(joined: f64, grouped: Option<f64>) -> ExecutionPlan {
    let mut join = node(2, "Hash Join", joined, 1.0, 800.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = i.customer_id)".into());
    let orders = read(3, "Seq Scan", "orders", 50_000.0, 1.0, 40.0);
    let hash = node(4, "Hash", 40_000.0, 1.0, 30.0);
    let items = read(5, "Seq Scan", "items", 40_000.0, 1.0, 20.0);
    match grouped {
        Some(rows) => {
            let distinct = node(1, "HashAggregate", rows, 1.0, 1_400.0);
            timed(
                plan(
                    PlanDialect::Postgres,
                    vec![distinct, join, orders, hash, items],
                    &[(1, 2), (2, 3), (2, 4), (4, 5)],
                ),
                1_400.0,
            )
        }
        None => timed(
            plan(
                PlanDialect::Postgres,
                vec![join, orders, hash, items],
                &[(2, 3), (2, 4), (4, 5)],
            ),
            800.0,
        ),
    }
}

#[test]
fn join_fanout_fires_when_rows_are_multiplied_then_collapsed() {
    let findings = findings_for_plan(&pg_fanout(3_000_000.0, Some(900.0)));
    let finding = get(&findings, K::JoinFanout);
    assert_eq!(variant(finding), "distinct");
    assert_eq!(finding.node_ids, vec![2, 1]);
    assert!(evidence(finding).contains("o.customer_id = i.customer_id"));

    // Nothing collapses the rows: the query asked for them.
    assert_none(
        &findings_for_plan(&pg_fanout(3_000_000.0, None)),
        K::JoinFanout,
    );
    // A join that returns about its input is not a fan-out.
    assert_none(
        &findings_for_plan(&pg_fanout(60_000.0, Some(900.0))),
        K::JoinFanout,
    );
}

fn pg_subplan(loops: f64) -> ExecutionPlan {
    let mut orders = read(1, "Seq Scan", "orders", loops, 1.0, 5_000.0);
    orders.alias = Some("o".into());
    let mut items = read(2, "Seq Scan", "items", 1.0, loops, 4_900.0);
    items.alias = Some("i".into());
    items.parent_relationship = Some("SubPlan".into());
    items.subplan_name = Some("SubPlan 1".into());
    items.filter = Some("(i.order_id = o.id)".into());
    items.rows_removed_by_filter = Some(300.0 * loops);
    timed(
        plan(PlanDialect::Postgres, vec![orders, items], &[(1, 2)]),
        5_000.0,
    )
}

#[test]
fn dependent_subquery_fires_when_it_runs_thousands_of_times() {
    let findings = findings_for_plan(&pg_subplan(8_000.0));
    let finding = get(&findings, K::DependentSubquery);
    assert_eq!(variant(finding), "loops");
    assert_eq!(finding.severity, S::Critical);
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_items_order_id\" ON \"items\" (\"order_id\");"
    );
    assert!(finding.summary.fallback().contains("SubPlan 1"));

    assert_none(&findings_for_plan(&pg_subplan(12.0)), K::DependentSubquery);
}

#[test]
fn mysql_estimated_dependent_subquery_is_a_low_confidence_note() {
    let outer = |rows: f64| {
        let mut orders = est(scan(1, "Table scan on o", "orders"), rows, 9_000.0);
        orders.alias = Some("o".into());
        let mut items = est(scan(2, "Index lookup on i", "items"), 3.0, 1.0);
        items.alias = Some("i".into());
        items.subplan_name = Some("Select #2 (subquery in projection; dependent)".into());
        items.properties.push(PlanProperty {
            name: "select_type".into(),
            value: "DEPENDENT SUBQUERY".into(),
        });
        plan(PlanDialect::MySql, vec![orders, items], &[(1, 2)])
    };
    let findings = findings_for_plan(&outer(80_000.0));
    let finding = get(&findings, K::DependentSubquery);
    assert_eq!(variant(finding), "estimated");
    assert_eq!((finding.severity, finding.confidence), (S::Info, C::Low));

    assert_none(&findings_for_plan(&outer(400.0)), K::DependentSubquery);
}

#[test]
fn sqlite_correlated_subquery_with_a_scan_inside_is_reported() {
    let text = "QUERY PLAN\n|--SCAN o\n`--CORRELATED SCALAR SUBQUERY 1\n   `--SCAN items\n";
    let plans = import_plan_document(text).unwrap().plans;
    let findings = findings_for_plan(&plans[0]);
    let finding = get(&findings, K::DependentSubquery);
    assert_eq!(variant(finding), "sqlite_scan");
    assert!(finding.summary.fallback().contains("items"));

    let text = "QUERY PLAN\n|--SCAN o\n`--CORRELATED SCALAR SUBQUERY 1\n   `--SEARCH items USING INDEX ix_items_order (order_id=?)\n";
    let plans = import_plan_document(text).unwrap().plans;
    assert_none(&findings_for_plan(&plans[0]), K::DependentSubquery);
    // A subquery that runs once is not correlated.
    let text = "QUERY PLAN\n|--SCAN o\n`--SCALAR SUBQUERY 1\n   `--SCAN items\n";
    let plans = import_plan_document(text).unwrap().plans;
    assert_none(&findings_for_plan(&plans[0]), K::DependentSubquery);
}

fn mysql_join_buffer(outer_rows: f64, inner_rows: f64, extra: &str) -> ExecutionPlan {
    let join = est(op(1, "nested_loop"), outer_rows, 9_000.0);
    let mut orders = est(scan(2, "table", "orders"), outer_rows, 500.0);
    orders.alias = Some("o".into());
    orders.logical_op = "access:ALL".into();
    let mut items = est(scan(3, "table", "items"), inner_rows, 8_000.0);
    items.alias = Some("i".into());
    items.logical_op = "access:ALL".into();
    items.estimated_rows_read = Some(inner_rows);
    items.filter = Some("(`shop`.`i`.`order_id` = `shop`.`o`.`id`)".into());
    items.extra.push(extra.into());
    plan(
        PlanDialect::MySql,
        vec![join, orders, items],
        &[(1, 2), (1, 3)],
    )
}

#[test]
fn join_buffer_fires_for_a_block_nested_loop_over_many_row_pairs() {
    let extra = "Using join buffer (Block Nested Loop)";
    let findings = findings_for_plan(&mysql_join_buffer(5_000.0, 40_000.0, extra));
    let finding = get(&findings, K::JoinBuffer);
    assert_eq!(variant(finding), "block");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(
        sql(finding),
        "CREATE INDEX `ix_items_order_id` ON `items` (`order_id`);"
    );

    assert_none(
        &findings_for_plan(&mysql_join_buffer(50.0, 400.0, extra)),
        K::JoinBuffer,
    );
    // MySQL 8.0.18+ hash join uses the same buffer and needs no index.
    assert_none(
        &findings_for_plan(&mysql_join_buffer(
            5_000.0,
            40_000.0,
            "Using join buffer (hash join)",
        )),
        K::JoinBuffer,
    );
}
