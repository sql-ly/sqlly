//! Known-answer tests: every plan here is small enough that the expected
//! numbers are worked out by hand in the comments.

use super::*;
use crate::plans::import::import_plan_document;
use crate::plans::model::{PlanBuffers, PlanNode};
use crate::plans::plan_fixtures::*;

fn close(actual: Option<f64>, expected: f64) -> bool {
    actual.is_some_and(|actual| (actual - expected).abs() < 1e-9)
}

fn close_to(actual: Option<f64>, expected: f64) -> bool {
    actual.is_some_and(|actual| (actual - expected).abs() < 1e-6)
}

fn metrics(analysis: &PlanAnalysis, id: i32) -> &NodeMetrics {
    &analysis.node(id).expect("node exists").metrics
}

fn flags(analysis: &PlanAnalysis, id: i32) -> NodeFlags {
    analysis.node(id).expect("node exists").flags
}

fn sample(name: &str) -> Vec<crate::plans::ExecutionPlan> {
    // The online viewer's samples, or this crate's own copy of a plan the
    // viewer no longer ships.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let own = root.join("tests/plan_samples").join(name);
    let path = if own.exists() {
        own
    } else {
        root.join("../../apps/planview-wasm/samples").join(name)
    };
    let text = std::fs::read_to_string(path).expect("sample reads");
    import_plan_document(&text).expect("sample parses").plans
}

#[test]
fn self_time_is_inclusive_minus_inputs() {
    let analysis = analyze_plan(&pg_join());
    // sort 100 - 90, join 90 - 70 - 12, hash 12 - 10; leaves keep theirs.
    for (id, own, inclusive) in [
        (1, 10.0, 100.0),
        (2, 8.0, 90.0),
        (3, 70.0, 70.0),
        (4, 2.0, 12.0),
        (5, 10.0, 10.0),
    ] {
        let node = metrics(&analysis, id);
        assert!(close(node.self_time_ms, own), "self time of {id}: {node:?}");
        assert!(
            close(node.inclusive_time_ms, inclusive),
            "inclusive of {id}"
        );
        assert!(close(node.time_share, own / 100.0), "share of {id}");
        assert!(close(node.inclusive_time_share, inclusive / 100.0));
    }
    assert!(analysis.has_timing && analysis.has_actuals);
    assert!(close(analysis.total_time_ms, 100.0));
    assert!(close(analysis.execution_time_ms, 100.5));
    assert!(close(analysis.planning_time_ms, 0.4));
    assert_eq!(analysis.primary_metric, PlanMetric::Time);
    let shares: f64 = analysis
        .nodes
        .iter()
        .filter_map(|node| node.metrics.time_share)
        .sum();
    assert!(
        (shares - 1.0).abs() < 1e-9,
        "own shares add up to the whole"
    );
}

#[test]
fn self_cost_is_cumulative_cost_minus_inputs() {
    let analysis = analyze_plan(&pg_join());
    // sort 500 - 400, join 400 - 300 - 40, hash 40 - 30.
    for (id, own) in [(1, 100.0), (2, 60.0), (3, 300.0), (4, 10.0), (5, 30.0)] {
        let node = metrics(&analysis, id);
        assert!(close(node.self_cost, own), "self cost of {id}: {node:?}");
        assert!(close(node.cost_share, own / 500.0));
    }
    assert!(close(analysis.total_cost, 500.0));
}

#[test]
fn structure_order_bottleneck_and_critical_path() {
    let analysis = analyze_plan(&pg_join());
    assert_eq!(analysis.roots, vec![1]);
    // Hash joins build first: the build side (hash over customers) comes
    // before the probe side, and every input before its consumer.
    assert_eq!(analysis.execution_order, vec![5, 4, 3, 2, 1]);
    assert_eq!(analysis.bottleneck, Some(3));
    assert_eq!(analysis.critical_path, vec![1, 2, 3]);
    for id in [1, 2, 3] {
        assert!(flags(&analysis, id).on_critical_path);
    }
    for id in [4, 5] {
        assert!(!flags(&analysis, id).on_critical_path);
    }
    assert!(flags(&analysis, 3).bottleneck);
    assert_eq!(
        analysis
            .nodes
            .iter()
            .filter(|node| node.flags.bottleneck)
            .count(),
        1
    );
    let join = analysis.node(2).expect("join");
    assert_eq!(join.parent_id, Some(1));
    assert_eq!(join.children, vec![3, 4]);
    assert_eq!(join.depth, 1);
    assert_eq!(join.subtree_size, 4);
    assert!(close(analysis.result_rows, 1000.0));
    assert_eq!(analysis.max_rows, 1000.0);
}

#[test]
fn estimate_miss_and_filter_selectivity() {
    let analysis = analyze_plan(&pg_join());
    let orders = metrics(&analysis, 3);
    // Estimated 50, got 1,000.
    assert!(close(orders.estimate_factor, 20.0));
    assert_eq!(orders.estimate_direction, Some(EstimateDirection::Under));
    // 1,000 kept + 99,000 removed.
    assert!(close(orders.rows_in, 100_000.0));
    assert!(close(orders.rows_removed, 99_000.0));
    assert!(close(orders.rows_removed_share, 0.99));
    assert!(flags(&analysis, 3).misestimate_origin);
    let join = metrics(&analysis, 2);
    assert!(close(join.estimate_factor, 1.0));
    assert_eq!(join.estimate_direction, Some(EstimateDirection::Exact));
    // A join's input is both sides: 1,000 + 200.
    assert!(close(join.rows_in, 1200.0));
    // Heat for the estimate metric is log10(factor) / 3.
    let heat = f64::from(orders.heat.estimate);
    assert!((heat - 20.0_f64.log10() / 3.0).abs() < 1e-6, "{heat}");
    assert!((f64::from(orders.heat.time) - 0.7).abs() < 1e-6);
}

#[test]
fn an_inherited_miss_is_not_an_origin() {
    let mut plan = pg_join();
    // The join now repeats the scan's wrong estimate instead of adding one.
    plan.nodes[1].estimated_rows = 50.0;
    let analysis = analyze_plan(&plan);
    assert!(close(metrics(&analysis, 2).estimate_factor, 20.0));
    assert!(flags(&analysis, 3).misestimate_origin);
    assert!(!flags(&analysis, 2).misestimate_origin);
}

#[test]
fn overestimates_and_unknown_estimates() {
    let mut plan = pg_join();
    plan.nodes[2].estimated_rows = 100_000.0;
    let analysis = analyze_plan(&plan);
    let orders = metrics(&analysis, 3);
    assert!(close(orders.estimate_factor, 100.0));
    assert_eq!(orders.estimate_direction, Some(EstimateDirection::Over));

    // An estimate of zero means the format did not carry one.
    plan.nodes[2].estimated_rows = 0.0;
    let analysis = analyze_plan(&plan);
    let orders = metrics(&analysis, 3);
    assert_eq!(orders.estimate_factor, None);
    assert_eq!(orders.estimate_direction, None);
    assert!(!flags(&analysis, 3).misestimate_origin);
}

#[test]
fn estimated_plans_have_costs_and_no_measurements() {
    let analysis = analyze_plan(&pg_join_estimated());
    assert!(!analysis.has_actuals && !analysis.has_timing);
    assert!(analysis.has_costs && analysis.has_estimates);
    assert_ne!(analysis.primary_metric, PlanMetric::Time);
    assert_eq!(analysis.total_time_ms, None);
    assert_eq!(analysis.bottleneck, Some(3), "largest own cost");
    let orders = metrics(&analysis, 3);
    assert_eq!(orders.self_time_ms, None);
    assert_eq!(orders.rows_out, None);
    assert_eq!(orders.estimate_factor, None);
    assert!(close(orders.estimated_rows_total, 50.0));
    assert!(close(orders.cost_share, 0.6));
}

#[test]
fn loops_multiply_rows_and_inner_cost() {
    let analysis = analyze_plan(&pg_nested_loop());
    let inner = metrics(&analysis, 3);
    assert!(close(inner.loops, 100.0));
    assert!(close(inner.rows_per_loop, 2.0));
    assert!(close(inner.rows_out, 200.0));
    // The estimate is per loop too, so it is compared per loop.
    assert!(close(inner.estimate_factor, 1.0));
    assert_eq!(inner.processes, None, "repeated executions, not workers");
    assert!(close(inner.self_time_ms, 50.0));
    assert!(close(inner.self_cost, 850.0));
    let join = metrics(&analysis, 1);
    assert!(close(join.self_time_ms, 5.0));
    assert!(close(join.self_cost, 30.0));
    assert_eq!(analysis.bottleneck, Some(3));
    assert_eq!(analysis.execution_order, vec![2, 3, 1]);
}

#[test]
fn parallel_processes_are_not_loops() {
    let mut gather = ran(est(op(1, "Gather"), 300.0, 100.0), 300.0, 1.0, 30.0);
    gather.workers_planned = Some(2);
    gather.workers_launched = Some(2);
    // PostgreSQL prints per-process averages below a Gather: 3 "loops" of
    // 100 rows are the leader and two workers.
    let mut scan = ran(
        est(scan(2, "Parallel Seq Scan", "events"), 125.0, 80.0),
        100.0,
        3.0,
        24.0,
    );
    scan.parallel = true;
    let analysis = analyze_plan(&plan(
        crate::plans::PlanDialect::Postgres,
        vec![gather, scan],
        &[(1, 2)],
    ));
    let scan = metrics(&analysis, 2);
    assert!(close(scan.processes, 3.0));
    assert!(close(scan.rows_out, 300.0));
    assert!(close(scan.rows_per_loop, 100.0));
    assert!(close(scan.self_time_ms, 24.0));
    assert!(close(metrics(&analysis, 1).self_time_ms, 6.0));
    assert!(flags(&analysis, 2).parallel);
    assert_eq!(analysis.groups.len(), 1);
    assert_eq!(analysis.groups[0].kind, PlanGroupKind::Parallel);
    assert_eq!(analysis.groups[0].root_id, 1);
}

#[test]
fn a_cte_is_charged_to_itself_not_to_its_reader() {
    // Sort (30 ms) over a CTE Scan (25 ms, which includes pulling the CTE)
    // with the CTE's own subtree (20 ms) attached to the sort as an
    // InitPlan. The CTE's time is inside the scan's, so the scan's own time
    // is 5 and the sort's is 30 - 25, not 30 - 25 - 20.
    let sort = ran(est(op(1, "Sort"), 10.0, 60.0), 10.0, 1.0, 30.0);
    let mut definition = ran(
        est(scan(2, "Seq Scan", "events"), 10.0, 40.0),
        10.0,
        1.0,
        20.0,
    );
    definition.subplan_name = Some("CTE recent".into());
    definition.parent_relationship = Some("InitPlan".into());
    let mut reader = ran(est(op(3, "CTE Scan"), 10.0, 5.0), 10.0, 1.0, 25.0);
    reader.cte_name = Some("recent".into());
    let analysis = analyze_plan(&plan(
        crate::plans::PlanDialect::Postgres,
        vec![sort, definition, reader],
        &[(1, 2), (1, 3)],
    ));
    assert!(close(metrics(&analysis, 2).self_time_ms, 20.0));
    assert!(close(metrics(&analysis, 3).self_time_ms, 5.0));
    assert!(close(metrics(&analysis, 1).self_time_ms, 5.0));
    assert!(close(analysis.total_time_ms, 30.0));
    assert!(
        analysis
            .groups
            .iter()
            .any(|group| group.kind == PlanGroupKind::Cte && group.root_id == 2)
    );
}

fn sql_server_batch(root_ms: f64, elapsed_ms: f64) -> crate::plans::ExecutionPlan {
    let mut join = est(op(1, "Hash Match"), 100.0, 3.0);
    join.actual_rows = Some(100.0);
    join.actual_time_ms = Some(root_ms);
    join.execution_mode = Some("Batch".into());
    let mut scan = est(scan(2, "Clustered Index Scan", "Orders"), 100.0, 2.0);
    scan.actual_rows = Some(100.0);
    scan.actual_time_ms = Some(20.0);
    scan.execution_mode = Some("Batch".into());
    let mut plan = plan(
        crate::plans::PlanDialect::SqlServer,
        vec![join, scan],
        &[(1, 2)],
    );
    plan.execution_time_ms = Some(elapsed_ms);
    plan
}

#[test]
fn sql_server_batch_mode_times_are_own_times() {
    // 5 + 20 fits inside the 26 ms the statement ran for.
    let analysis = analyze_plan(&sql_server_batch(5.0, 26.0));
    assert!(close(metrics(&analysis, 1).self_time_ms, 5.0));
    assert!(close(metrics(&analysis, 1).inclusive_time_ms, 25.0));
    assert!(close(metrics(&analysis, 2).self_time_ms, 20.0));
    assert!(close(analysis.total_time_ms, 25.0));
}

fn fixture(name: &str) -> Vec<crate::plans::ExecutionPlan> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden/plans")
        .join(name);
    let text = std::fs::read_to_string(path).expect("fixture reads");
    import_plan_document(&text).expect("fixture parses").plans
}

fn own_times(analysis: &PlanAnalysis) -> Vec<(i32, Option<f64>)> {
    analysis
        .nodes
        .iter()
        .map(|node| (node.node_id, node.metrics.self_time_ms))
        .collect()
}

#[test]
fn sql_server_batch_times_stay_own_however_large_they_are() {
    // The mode decides, not whether the numbers happen to fit inside the
    // statement's elapsed time: a batch-mode operator's 25 ms is its own
    // even when the statement reports 26 ms and the scan below it 20.
    let analysis = analyze_plan(&sql_server_batch(25.0, 26.0));
    assert!(close(metrics(&analysis, 1).self_time_ms, 25.0));
    assert!(close(metrics(&analysis, 1).inclusive_time_ms, 45.0));
    assert!(close(metrics(&analysis, 2).self_time_ms, 20.0));
}

#[test]
fn sql_server_batch_mode_fixture_keeps_each_operators_own_time() {
    // q3b, every operator in batch mode, ActualElapsedms as printed:
    //   Compute Scalar 0 > Hash Match 0 > Hash Match 202 > Index Scan 8,
    //   Index Scan 16. QueryTimeStats ElapsedTime is 229.
    let plans = fixture("sqlserver/q3b-actual-hash-spill.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(
        own_times(&analysis),
        vec![
            (0, Some(0.0)),
            (1, Some(0.0)),
            (2, Some(202.0)),
            (3, Some(8.0)),
            (4, Some(16.0)),
        ]
    );
    assert!(close(analysis.total_time_ms, 226.0));
    assert!(close(metrics(&analysis, 0).inclusive_time_ms, 226.0));
    assert_eq!(analysis.bottleneck, Some(2));
}

#[test]
fn sql_server_row_mode_fixture_subtracts_the_inputs() {
    // q12, every operator in row mode, ActualElapsedms as printed:
    //   Compute Scalar (no counters) > Stream Aggregate 36 > Nested Loops 35
    //   > Hash Match 14 > Table Scan 2, and Clustered Index Seek 15.
    let plans = fixture("sqlserver/q12-actual-no-stats.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(
        own_times(&analysis),
        vec![
            (1, None),
            (2, Some(1.0)),
            (3, Some(6.0)),
            (5, Some(12.0)),
            (6, Some(2.0)),
            (7, Some(15.0)),
        ]
    );
    assert!(close(analysis.total_time_ms, 36.0));
}

#[test]
fn sql_server_mixed_mode_fixture_is_read_per_operator() {
    // q4 at DOP 7: a row-mode Gather Streams (45 ms, cumulative) over
    // batch-mode operators whose slowest thread reports
    //   Sort 6 > Hash Match 4 > Hash Match 12 > Index Scan 4,
    //   Compute Scalar 2 > Clustered Index Scan 13.
    // The batch operators own 41 ms, which leaves the exchange 4.
    let plans = fixture("sqlserver/q4-actual-parallel.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(
        own_times(&analysis),
        vec![
            (0, Some(4.0)),
            (1, Some(6.0)),
            (2, Some(4.0)),
            (3, Some(12.0)),
            (4, Some(4.0)),
            (5, Some(2.0)),
            (7, Some(13.0)),
        ]
    );
    assert!(close(analysis.total_time_ms, 45.0));
    assert!(close(metrics(&analysis, 1).inclusive_time_ms, 41.0));
    assert_eq!(analysis.bottleneck, Some(7));
}

#[test]
fn sql_server_inner_side_estimates_are_compared_per_execution() {
    // q2: a Key Lookup estimated at 1 row for each of 6 executions, which
    // ran 6 times and returned 6 rows. The parser stores the estimate for
    // all executions (6); the comparison is 1 row against 1 row.
    let plans = fixture("sqlserver/q2-actual-key-lookup.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    let lookup = metrics(&analysis, 3);
    assert!(close(lookup.loops, 6.0));
    assert!(close(lookup.rows_per_loop, 1.0));
    assert!(close(lookup.estimated_rows, 1.0));
    assert!(close(lookup.estimated_rows_total, 6.0));
    assert!(close(lookup.estimate_factor, 1.0));

    // q12: EstimateRows 1 with 76.4597 rebinds, 18,601 executions returning
    // 18,601 rows. One row each time is what was estimated; the surprise is
    // the number of executions, which belongs to the outer side.
    let plans = fixture("sqlserver/q12-actual-no-stats.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    let seek = metrics(&analysis, 7);
    assert!(close(seek.estimated_rows, 1.0));
    assert!(close(seek.estimated_rows_total, 77.4597));
    assert!(close(seek.estimate_factor, 1.0));
}

#[test]
fn sql_server_one_execution_on_several_threads_is_not_a_loop() {
    // q4 at DOP 7: every operator below the exchange reports
    // ActualExecutions 1 on each of 7 threads. The Sort returned 600 rows
    // against an estimate of 1,000, not 86 rows a time.
    let plans = fixture("sqlserver/q4-actual-parallel.xml.txt");
    let analysis = analyze_plan(&plans[0]);
    let sort = metrics(&analysis, 1);
    assert!(close(sort.loops, 1.0));
    assert!(close(sort.rows_per_loop, 600.0));
    assert!(close(sort.estimate_factor, 1000.0 / 600.0));
    assert!(sort.processes.is_none());
}

#[test]
fn postgres_parallel_fixture_adds_up_to_the_root() {
    // join-analyze: three processes below the Gather Merge. Each node's
    // "actual time" is the average of one process, so the own times are
    // plain differences of the printed numbers and add up to the root's
    // 111.453 ms.
    let plans = fixture("postgresql/join-analyze.txt");
    let analysis = analyze_plan(&plans[0]);
    assert!(close_to(
        metrics(&analysis, 3).self_time_ms,
        111.422 - 106.642
    ));
    assert!(close_to(
        metrics(&analysis, 7).self_time_ms,
        69.364 - 13.438 - 10.618
    ));
    assert!(close_to(metrics(&analysis, 8).self_time_ms, 13.438));
    assert!(close_to(analysis.total_time_ms, 111.453));
    assert_eq!(metrics(&analysis, 8).processes, Some(3.0));
    assert!(close_to(metrics(&analysis, 8).rows_per_loop, 400_000.0));
}

#[test]
fn postgres_init_plan_nobody_names_is_not_counted_twice() {
    // cte-subplan: the Limit took 100.402 ms and its input, the CTE Scan,
    // 100.321. InitPlan 5 (18.845) is named in the scan's filter. InitPlan 2
    // (17.906) is named nowhere, and cannot have run in the 0.081 ms the
    // Limit has to itself, so it ran inside the scan as well. The scan's own
    // time is 100.321 - 0.587 (CTE) - 34.734 (SubPlan 4) - 18.845 - 17.906.
    let plans = fixture("postgresql/cte-subplan.txt");
    let analysis = analyze_plan(&plans[0]);
    assert!(close_to(metrics(&analysis, 11).self_time_ms, 28.249));
    assert!(close_to(
        metrics(&analysis, 1).self_time_ms,
        100.402 - 100.321
    ));
    assert!(close_to(analysis.total_time_ms, 100.402));
}

#[test]
fn mysql_analyze_times_are_multiplied_by_loops() {
    // mysql84-q1: EXPLAIN ANALYZE prints the average of one loop. The index
    // lookup on oi is 0.00179 ms for each of 10,000 loops (17.9 ms) and the
    // primary-key lookup on c 514e-6 ms for each of 10,000 (5.14 ms), which
    // leaves the two joins 30.7 - 11.3 - 17.9 and 11.3 - 5.23 - 5.14.
    let plans = fixture("mysql/mysql84-q1-join-analyze-tree.txt");
    let analysis = analyze_plan(&plans[0]);
    assert!(close_to(metrics(&analysis, 9).self_time_ms, 17.9));
    assert!(close_to(metrics(&analysis, 8).self_time_ms, 5.14));
    assert!(close_to(metrics(&analysis, 4).self_time_ms, 1.5));
    assert!(close_to(metrics(&analysis, 5).self_time_ms, 0.93));
    assert!(close_to(analysis.total_time_ms, 36.1));
    assert!(close_to(metrics(&analysis, 9).rows_per_loop, 2.5));
    assert_eq!(analysis.bottleneck, Some(9));
}

#[test]
fn times_printed_in_steps_as_large_as_the_statement_are_not_split() {
    // Oracle A-Time is printed in hundredths of a second. In this plan every
    // line reads 00:00:00.01: the statement and both scans. Subtracting
    // gives each scan 10 ms of a 10 ms statement.
    let plans = sample("oracle-allstats.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(analysis.time_resolution_ms, Some(10.0));
    assert!(analysis.timing_too_coarse);
    assert!(!analysis.has_timing);
    assert_eq!(analysis.total_time_ms, None);
    assert_eq!(analysis.execution_time_ms, Some(10.0));
    assert_ne!(analysis.primary_metric, PlanMetric::Time);
    assert!(
        analysis.nodes.iter().all(|node| {
            node.metrics.self_time_ms.is_none() && node.metrics.time_share.is_none()
        })
    );

    // A DuckDB text profile prints `(0.07s)` in one box and `(0.00s)` in the
    // other twelve, which could hide more time than the one box shows.
    let plans = fixture("duckdb/duckdb142-join-analyze.txt");
    let analysis = analyze_plan(&plans[0]);
    assert!(analysis.timing_too_coarse);
    assert_eq!(analysis.total_time_ms, None);
    assert_eq!(analysis.bottleneck, None);
}

#[test]
fn coarse_steps_do_not_matter_in_a_long_statement() {
    // 560 ms, of which the WINDOW SORT line accounts for 540: ten
    // milliseconds either way changes nothing.
    let plans = sample("oracle-window-allstats.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(analysis.time_resolution_ms, Some(10.0));
    assert!(!analysis.timing_too_coarse);
    assert!(close_to(analysis.total_time_ms, 560.0));
    let slowest = analysis
        .node(analysis.bottleneck.expect("bottleneck"))
        .expect("node");
    assert!(close_to(slowest.metrics.self_time_ms, 540.0));

    // Formats that print fine-grained times have no resolution to report.
    let plans = fixture("postgresql/join-analyze.txt");
    let analysis = analyze_plan(&plans[0]);
    assert_eq!(analysis.time_resolution_ms, None);
    assert!(!analysis.timing_too_coarse);
}

#[test]
fn spark_expression_ids_are_removed_and_subquery_names_kept() {
    let spark = |text: &str| engine_text(PlanEngine::Spark, text);
    assert_eq!(
        spark("(isnotnull(customer_id#6L) AND (total#7 > 10))"),
        "(isnotnull(customer_id) AND (total > 10))"
    );
    assert_eq!(
        spark(
            "cast(t#225 as decimal(25,6)) > cast(Subquery subquery#224, [id=#1188] as decimal(25,6))"
        ),
        "cast(t as decimal(25,6)) > cast(subquery#224 as decimal(25,6))"
    );
    // Not an expression id: no name in front, or more name behind.
    assert_eq!(spark("x = '#12'"), "x = '#12'");
    assert_eq!(spark("tag#1a"), "tag#1a");
    // Other engines are left alone.
    assert_eq!(engine_text(PlanEngine::Postgres, "a#1"), "a#1");
}

#[test]
fn cockroachdb_sort_signs_become_directions() {
    let key = |text: &str| engine_sort_key(PlanEngine::CockroachDb, text);
    assert_eq!(key("-sum"), "sum DESC");
    assert_eq!(key("+id"), "id");
    assert_eq!(key("name"), "name");
    assert_eq!(key("-"), "-");
    assert_eq!(engine_sort_key(PlanEngine::Postgres, "-sum"), "-sum");
    assert_eq!(
        engine_sort_key(PlanEngine::Spark, "t#225 DESC NULLS LAST"),
        "t DESC NULLS LAST"
    );
}

#[test]
fn sql_server_row_mode_times_are_cumulative() {
    let mut plan = sql_server_batch(25.0, 26.0);
    for node in &mut plan.nodes {
        node.execution_mode = Some("Row".into());
    }
    let analysis = analyze_plan(&plan);
    assert!(close(metrics(&analysis, 1).self_time_ms, 5.0));
    assert!(close(metrics(&analysis, 2).self_time_ms, 20.0));
}

#[test]
fn duckdb_reports_own_time() {
    let mut top = op(1, "HASH_GROUP_BY");
    top.actual_rows = Some(5.0);
    top.actual_self_time_ms = Some(1.0);
    let mut scan = scan(2, "TABLE_SCAN", "orders");
    scan.actual_rows = Some(500.0);
    scan.actual_self_time_ms = Some(3.0);
    let analysis = analyze_plan(&plan(
        crate::plans::PlanDialect::DuckDb,
        vec![top, scan],
        &[(1, 2)],
    ));
    assert!(close(metrics(&analysis, 1).self_time_ms, 1.0));
    assert!(close(metrics(&analysis, 1).inclusive_time_ms, 4.0));
    assert!(close(metrics(&analysis, 1).time_share, 0.25));
    assert!(close(metrics(&analysis, 2).time_share, 0.75));
    assert_eq!(analysis.bottleneck, Some(2));
}

#[test]
fn postgres_page_counts_are_inclusive() {
    let mut plan = pg_join();
    let pages = |hit: f64, read: f64| {
        Some(PlanBuffers {
            shared_hit: Some(hit),
            shared_read: Some(read),
            ..Default::default()
        })
    };
    plan.nodes[0].buffers = pages(90.0, 10.0);
    plan.nodes[1].buffers = pages(90.0, 10.0);
    plan.nodes[2].buffers = pages(60.0, 20.0);
    plan.nodes[3].buffers = pages(20.0, 0.0);
    plan.nodes[4].buffers = pages(20.0, 0.0);
    let analysis = analyze_plan(&plan);
    assert!(analysis.has_io);
    assert!(close(analysis.total_io_pages, 100.0));
    assert!(close(metrics(&analysis, 3).io_pages_self, 80.0));
    assert!(close(metrics(&analysis, 3).io_share, 0.8));
    assert!(close(metrics(&analysis, 5).io_pages_self, 20.0));
    for id in [1, 2, 4] {
        assert!(close(metrics(&analysis, id).io_pages_self, 0.0), "{id}");
    }
    assert!(close(metrics(&analysis, 2).io_pages_inclusive, 100.0));
}

#[test]
fn never_executed_and_spilled_nodes_are_flagged() {
    let mut plan = pg_join();
    plan.nodes[0].sort_space_type = Some("Disk".into());
    plan.nodes[0].sort_space_kb = Some(2048.0);
    plan.nodes[4].never_executed = true;
    plan.nodes[4].actual_rows = None;
    plan.nodes[4].actual_time_ms = None;
    let analysis = analyze_plan(&plan);
    assert!(flags(&analysis, 1).spilled);
    assert!(close(metrics(&analysis, 1).spilled_kb, 2048.0));
    assert!(!flags(&analysis, 2).spilled);
    assert!(flags(&analysis, 5).never_executed);
    assert_eq!(metrics(&analysis, 5).estimate_factor, None);
    assert!(close(metrics(&analysis, 5).self_time_ms, 0.0));
    // The hash above it keeps all 12 ms now.
    assert!(close(metrics(&analysis, 4).self_time_ms, 12.0));
}

#[test]
fn objects_predicates_and_titles_are_resolved() {
    let analysis = analyze_plan(&pg_join());
    let orders = analysis.node(3).expect("orders scan");
    let object = orders.object.as_ref().expect("table");
    assert_eq!(object.name, "orders");
    assert_eq!(object.alias.as_deref(), Some("o"));
    assert_eq!(object.label(), "orders o");
    assert_eq!(orders.title().fallback(), "Seq Scan on orders o");
    let filter = orders.filter.as_ref().expect("filter");
    assert_eq!(filter.text, "o.status = 'open'");
    assert_eq!(filter.raw, "(o.status = 'open'::text)");
    assert_eq!(orders.category(), OperatorCategory::Scan);
    let customers = analysis.node(5).expect("customers scan");
    assert_eq!(customers.index.as_deref(), Some("customers_pkey"));
    assert_eq!(
        customers.title().fallback(),
        "Index Scan on customers c using customers_pkey"
    );
    assert_eq!(
        analysis.node(2).expect("join").category(),
        OperatorCategory::JoinHash
    );
    assert_eq!(
        analysis.node(1).expect("sort").sort_keys,
        vec!["o.created_at DESC"]
    );
}

#[test]
fn malformed_plans_do_not_panic_or_loop() {
    // A cycle, an edge to a node that does not exist, a duplicate edge and
    // a node with two parents.
    let nodes: Vec<PlanNode> = (1..=4)
        .map(|id| ran(est(op(id, "Result"), 1.0, 1.0), 1.0, 1.0, 1.0))
        .collect();
    let plan = plan(
        crate::plans::PlanDialect::Postgres,
        nodes,
        &[(1, 2), (2, 3), (3, 1), (1, 2), (4, 3), (2, 99)],
    );
    let analysis = analyze_plan(&plan);
    assert_eq!(analysis.nodes.len(), 4);
    assert_eq!(analysis.execution_order.len(), 4);
    let empty = analyze_plan(&crate::plans::ExecutionPlan::default());
    assert!(empty.nodes.is_empty());
    assert_eq!(empty.bottleneck, None);
}

#[test]
fn sample_postgres_parallel_plan() {
    let plans = sample("postgresql-analyze.json");
    let analysis = analyze_plan(&plans[0]);
    let orders = analysis
        .nodes
        .iter()
        .find(|node| {
            node.object
                .as_ref()
                .is_some_and(|object| object.name == "orders")
        })
        .expect("orders scan");
    // The file says: Actual Rows 200000, Actual Loops 3, under Gather Merge
    // with 2 workers launched.
    assert!(close(orders.metrics.processes, 3.0));
    assert!(close(orders.metrics.rows_out, 600_000.0));
    assert!(orders.flags.parallel);
    let shares: f64 = analysis
        .nodes
        .iter()
        .filter_map(|node| node.metrics.time_share)
        .sum();
    assert!((shares - 1.0).abs() < 1e-6, "{shares}");
}

#[test]
fn sample_sql_server_batch_has_three_statements() {
    let plans = sample("sqlserver-actual.xml");
    assert_eq!(plans.len(), 3);
    let second = analyze_plan(&plans[1]);
    let bottleneck = second
        .node(second.bottleneck.expect("bottleneck"))
        .expect("node");
    assert_eq!(bottleneck.object.as_ref().expect("table").name, "Orders");
    assert!(bottleneck.metrics.time_share.expect("share") > 0.9);
    // Statement 3 has row counts but no per-operator times.
    let third = analyze_plan(&plans[2]);
    assert!(third.has_actuals && !third.has_timing);
    assert_eq!(third.primary_metric, PlanMetric::Cost);
}

#[test]
fn every_sample_analyzes_consistently() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/planview-wasm/samples");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("samples").flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || name.ends_with(".md") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path()).expect("sample reads");
        let Ok(imported) = import_plan_document(&text) else {
            continue;
        };
        for plan in &imported.plans {
            let analysis = analyze_plan(plan);
            assert_eq!(analysis.nodes.len(), plan.nodes.len(), "{name}");
            let mut order = analysis.execution_order.clone();
            order.sort_unstable();
            order.dedup();
            assert_eq!(
                order.len(),
                plan.nodes.len(),
                "{name}: every node runs once"
            );
            for node in &analysis.nodes {
                let metrics = &node.metrics;
                for share in [metrics.time_share, metrics.cost_share, metrics.io_share]
                    .into_iter()
                    .flatten()
                {
                    assert!((0.0..=1.0 + 1e-9).contains(&share), "{name}: share {share}");
                }
                if let Some(factor) = metrics.estimate_factor {
                    assert!(
                        factor >= 1.0 && factor.is_finite(),
                        "{name}: factor {factor}"
                    );
                }
                let heat = metrics.heat;
                for value in [heat.time, heat.cost, heat.rows, heat.estimate, heat.io] {
                    assert!((0.0..=1.0).contains(&value), "{name}: heat {value}");
                }
            }
            if let Some(id) = analysis.bottleneck {
                assert!(
                    analysis.node(id).is_some_and(|node| node.flags.bottleneck),
                    "{name}"
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 10, "only {checked} sample plans parsed");
}
