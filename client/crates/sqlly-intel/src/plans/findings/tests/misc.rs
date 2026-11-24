//! CTEs, partitions, spools, parallelism, compilation.

use super::*;
use crate::plans::PlanProperty;

fn pg_cte(read_rows: f64, kept: f64, producer_ms: f64) -> ExecutionPlan {
    let mut cte_scan = node(1, "CTE Scan", kept, 1.0, producer_ms + 40.0);
    cte_scan.cte_name = Some("recent".into());
    cte_scan.filter = Some("(recent.customer_id = 42)".into());
    cte_scan.rows_removed_by_filter = Some(read_rows - kept);
    let mut orders = read(2, "Seq Scan", "orders", read_rows, 1.0, producer_ms);
    orders.subplan_name = Some("CTE recent".into());
    orders.parent_relationship = Some("InitPlan".into());
    timed(
        plan(PlanDialect::Postgres, vec![cte_scan, orders], &[(1, 2)]),
        producer_ms + 40.0,
    )
}

#[test]
fn cte_materialization_fires_when_the_filter_is_applied_after_the_cte() {
    let findings = findings_for_plan(&pg_cte(600_000.0, 12.0, 700.0));
    let finding = get(&findings, K::CteMaterialization);
    assert_eq!(variant(finding), "filtered");
    assert_eq!(
        (finding.severity, finding.confidence),
        (S::Warning, C::High)
    );
    assert_eq!(finding.node_ids, vec![1, 2]);
    assert!(evidence(finding).contains("recent.customer_id = 42"));
}

#[test]
fn cte_materialization_is_silent_when_most_rows_are_used_or_few_are_read() {
    assert_none(
        &findings_for_plan(&pg_cte(600_000.0, 400_000.0, 700.0)),
        K::CteMaterialization,
    );
    assert_none(
        &findings_for_plan(&pg_cte(3_000.0, 12.0, 700.0)),
        K::CteMaterialization,
    );
    // A recursive CTE cannot be inlined.
    let mut recursive = pg_cte(600_000.0, 12.0, 700.0);
    recursive.nodes[1].physical_op = "Recursive Union".into();
    recursive.nodes[1].relation = None;
    assert_none(&findings_for_plan(&recursive), K::CteMaterialization);
}

fn pg_append(empty_branches: usize, rows_each: f64) -> ExecutionPlan {
    let mut nodes = vec![node(1, "Append", 500.0, 1.0, 400.0)];
    let mut edges = Vec::new();
    for index in 0..6usize {
        let id = index as i32 + 2;
        let empty = index < empty_branches;
        let kept = if empty { 0.0 } else { 250.0 };
        let mut part = read(
            id,
            "Seq Scan",
            &format!("events_2024_{:02}", index + 1),
            kept,
            1.0,
            60.0,
        );
        part.filter = Some("(account_id = 7)".into());
        part.rows_removed_by_filter = Some(rows_each - kept);
        nodes.push(part);
        edges.push((1, id));
    }
    timed(plan(PlanDialect::Postgres, nodes, &edges), 400.0)
}

#[test]
fn partition_pruning_fires_when_most_partitions_return_nothing() {
    let findings = findings_for_plan(&pg_append(4, 50_000.0));
    let finding = get(&findings, K::PartitionPruning);
    assert_eq!(variant(finding), "empty_branches");
    assert_eq!(finding.severity, S::Warning);
    assert!(evidence(finding).contains("6 scanned, 4 returned no rows"));
    assert_eq!(finding.node_ids, vec![1, 2, 3, 4, 5]);

    // One empty partition of six is ordinary.
    assert_none(
        &findings_for_plan(&pg_append(1, 50_000.0)),
        K::PartitionPruning,
    );
    // Empty but tiny partitions cost nothing.
    assert_none(
        &findings_for_plan(&pg_append(4, 200.0)),
        K::PartitionPruning,
    );
}

fn clickhouse_read(granules: &str, filtered: bool) -> ExecutionPlan {
    let filter = {
        let mut filter = est(op(1, "Filter"), 0.0, 0.0);
        filter.filter = Some("user_id = 42".into());
        filter
    };
    let mut events = est(scan(2, "ReadFromMergeTree", "events"), 0.0, 0.0);
    events.schema = Some("analytics".into());
    events.properties.push(PlanProperty {
        name: "PrimaryKey index granules".into(),
        value: granules.into(),
    });
    if filtered {
        plan(PlanDialect::ClickHouse, vec![filter, events], &[(1, 2)])
    } else {
        let mut top = est(op(1, "Expression"), 0.0, 0.0);
        top.filter = None;
        plan(PlanDialect::ClickHouse, vec![top, events], &[(1, 2)])
    }
}

#[test]
fn clickhouse_filter_that_skips_no_granules_is_reported() {
    let findings = findings_for_plan(&clickhouse_read("1200/1200", true));
    let finding = get(&findings, K::PartitionPruning);
    assert_eq!(variant(finding), "granules");
    assert_eq!(
        sql(finding),
        "SELECT sorting_key, partition_key, primary_key\nFROM system.tables\nWHERE database = 'analytics' AND name = 'events';"
    );

    // The primary key skipped most of the table.
    assert_none(
        &findings_for_plan(&clickhouse_read("3/1200", true)),
        K::PartitionPruning,
    );
    // No WHERE clause: reading everything is the query.
    assert_none(
        &findings_for_plan(&clickhouse_read("1200/1200", false)),
        K::PartitionPruning,
    );
    // A small table.
    assert_none(
        &findings_for_plan(&clickhouse_read("4/4", true)),
        K::PartitionPruning,
    );
}

fn oracle_partitions(operation: &str, rows: f64) -> ExecutionPlan {
    let select = est(op(1, "SELECT STATEMENT"), rows / 100.0, 900.0);
    let mut partition = est(op(2, operation), rows / 100.0, 900.0);
    // Pstart 1, Pstop 12 (or 4..4 for a single partition).
    partition.partitions_scanned = Some(if operation.ends_with(" ALL") { 12 } else { 1 });
    let mut sales = est(scan(3, "TABLE ACCESS FULL", "SALES"), rows / 100.0, 900.0);
    sales.estimated_rows_read = Some(rows);
    sales.filter = Some("\"CHANNEL_ID\"=3".into());
    plan(
        PlanDialect::Oracle,
        vec![select, partition, sales],
        &[(1, 2), (2, 3)],
    )
}

#[test]
fn oracle_partition_range_all_with_a_filter_is_reported() {
    let findings = findings_for_plan(&oracle_partitions("PARTITION RANGE ALL", 2_000_000.0));
    let finding = get(&findings, K::PartitionPruning);
    assert_eq!(variant(finding), "all");
    assert_eq!(finding.severity, S::Info);
    assert_eq!(finding.node_ids, vec![2, 3]);
    assert!(
        finding
            .summary
            .fallback()
            .contains("all 12 partitions of SALES")
    );

    assert_none(
        &findings_for_plan(&oracle_partitions("PARTITION RANGE SINGLE", 2_000_000.0)),
        K::PartitionPruning,
    );
    assert_none(
        &findings_for_plan(&oracle_partitions("PARTITION RANGE ALL", 800.0)),
        K::PartitionPruning,
    );
}

fn mssql_spool(rows: f64, own_ms: f64) -> ExecutionPlan {
    let top = node(1, "Nested Loops", rows, 1.0, own_ms + 100.0);
    let customers = read(2, "Clustered Index Scan", "Customers", 100.0, 1.0, 5.0);
    let mut spool = node(3, "Table Spool", rows, 100.0, own_ms + 20.0);
    spool.logical_op = "Lazy Spool".into();
    let orders = read(4, "Clustered Index Scan", "Orders", rows / 100.0, 1.0, 20.0);
    timed(
        plan(
            PlanDialect::SqlServer,
            vec![top, customers, spool, orders],
            &[(1, 2), (1, 3), (3, 4)],
        ),
        own_ms + 100.0,
    )
}

#[test]
fn spool_fires_when_the_spool_itself_takes_the_time() {
    let findings = findings_for_plan(&mssql_spool(400_000.0, 900.0));
    let finding = get(&findings, K::Spool);
    assert_eq!(variant(finding), "heavy");
    assert_eq!(
        (finding.severity, finding.confidence),
        (S::Warning, C::Medium)
    );
    assert_eq!(finding.node_ids, vec![3]);

    assert_none(&findings_for_plan(&mssql_spool(2_000.0, 4.0)), K::Spool);
}

fn pg_gather(planned: u32, launched: u32, time_ms: f64) -> ExecutionPlan {
    let mut gather = node(1, "Gather", 1_000.0, 1.0, time_ms);
    gather.workers_planned = Some(planned);
    gather.workers_launched = Some(launched);
    let orders = read(
        2,
        "Parallel Seq Scan",
        "orders",
        1_000.0,
        1.0,
        time_ms - 5.0,
    );
    timed(
        plan(PlanDialect::Postgres, vec![gather, orders], &[(1, 2)]),
        time_ms,
    )
}

#[test]
fn parallel_workers_fires_when_fewer_started_than_planned() {
    let findings = findings_for_plan(&pg_gather(4, 0, 900.0));
    let finding = get(&findings, K::ParallelWorkers);
    assert_eq!(variant(finding), "fewer");
    assert_eq!(finding.severity, S::Warning);
    assert!(evidence(finding).contains("4 planned, 0 launched"));
    assert!(sql(finding).contains("SHOW max_parallel_workers;"));

    assert_none(
        &findings_for_plan(&pg_gather(4, 4, 900.0)),
        K::ParallelWorkers,
    );
    // A 12 ms statement does not need its workers.
    assert_none(
        &findings_for_plan(&pg_gather(4, 0, 12.0)),
        K::ParallelWorkers,
    );
}

fn mssql_udf(udf_ms: f64, total_ms: f64) -> ExecutionPlan {
    let mut compute = node(1, "Compute Scalar", 50_000.0, 1.0, total_ms);
    compute.udf_elapsed_ms = Some(udf_ms);
    compute.udf_cpu_ms = Some(udf_ms * 0.9);
    let orders = read(2, "Clustered Index Scan", "Orders", 50_000.0, 1.0, 60.0);
    timed(
        plan(PlanDialect::SqlServer, vec![compute, orders], &[(1, 2)]),
        total_ms,
    )
}

#[test]
fn scalar_udf_fires_when_the_function_takes_a_real_share_of_the_time() {
    let findings = findings_for_plan(&mssql_udf(2_400.0, 3_000.0));
    let finding = get(&findings, K::ScalarUdf);
    assert_eq!(variant(finding), "time");
    assert_eq!(finding.severity, S::Critical);
    assert!(sql(finding).contains("is_inlineable"));
    let impact = finding.impact.as_ref().expect("impact");
    assert_eq!(impact.time_ms, Some(2_400.0));
    assert!((impact.share - 0.8).abs() < 1e-9);
    // The largest saving in the plan comes first.
    assert_eq!(findings[0].kind, K::ScalarUdf);

    assert_none(&findings_for_plan(&mssql_udf(40.0, 3_000.0)), K::ScalarUdf);
    assert_none(&findings_for_plan(&mssql_udf(60.0, 70.0)), K::ScalarUdf);
}

fn mssql_threads(thread_rows: &[f64]) -> ExecutionPlan {
    let gather = node(1, "Parallelism", thread_rows.iter().sum(), 1.0, 2_000.0);
    let mut orders = read(
        2,
        "Clustered Index Scan",
        "Orders",
        thread_rows.iter().sum(),
        1.0,
        1_800.0,
    );
    orders.thread_rows = thread_rows.to_vec();
    timed(
        plan(PlanDialect::SqlServer, vec![gather, orders], &[(1, 2)]),
        2_000.0,
    )
}

#[test]
fn parallel_skew_fires_when_one_thread_did_the_work() {
    let findings = findings_for_plan(&mssql_threads(&[1_960_000.0, 20_000.0, 10_000.0, 10_000.0]));
    let finding = get(&findings, K::ParallelSkew);
    assert_eq!(variant(finding), "one_thread");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(finding.node_ids, vec![2]);
    assert!(
        finding.summary.fallback().contains("98%"),
        "{:?}",
        finding.summary
    );

    // An even split.
    assert_none(
        &findings_for_plan(&mssql_threads(&[
            500_000.0, 510_000.0, 495_000.0, 495_000.0,
        ])),
        K::ParallelSkew,
    );
    // Skewed, but over a few thousand rows.
    assert_none(
        &findings_for_plan(&mssql_threads(&[9_800.0, 100.0, 50.0, 50.0])),
        K::ParallelSkew,
    );
}

fn mssql_serial(reason: &str, cost: f64, time_ms: Option<f64>) -> ExecutionPlan {
    let orders = est(scan(1, "Clustered Index Scan", "Orders"), 900_000.0, cost);
    let mut plan = plan(PlanDialect::SqlServer, vec![orders], &[]);
    plan.non_parallel_reason = Some(reason.into());
    plan.execution_time_ms = time_ms;
    plan
}

#[test]
fn serial_plan_forced_names_the_reason_the_engine_gave() {
    let findings = findings_for_plan(&mssql_serial(
        "TSQLUserDefinedFunctionsNotParallelizable",
        180.0,
        None,
    ));
    let finding = get(&findings, K::SerialPlanForced);
    assert_eq!(variant(finding), "udf");
    assert_eq!(finding.severity, S::Info);
    assert!(evidence(finding).contains("TSQLUserDefinedFunctionsNotParallelizable"));

    let findings = findings_for_plan(&mssql_serial("MaxDOPSetToOne", 180.0, None));
    assert_eq!(variant(get(&findings, K::SerialPlanForced)), "maxdop");
    let findings = findings_for_plan(&mssql_serial(
        "TableVariableTransactionsDoNotSupportParallelNestedTransaction",
        180.0,
        None,
    ));
    assert_eq!(
        variant(get(&findings, K::SerialPlanForced)),
        "table_variable"
    );
}

#[test]
fn serial_plan_forced_is_silent_for_cheap_plans_and_benign_reasons() {
    assert_none(
        &findings_for_plan(&mssql_serial("MaxDOPSetToOne", 3.0, None)),
        K::SerialPlanForced,
    );
    assert_none(
        &findings_for_plan(&mssql_serial("EstimatedDOPIsOne", 180.0, None)),
        K::SerialPlanForced,
    );
    assert_none(
        &findings_for_plan(&mssql_serial(
            "NoParallelPlansInDesktopOrExpressEdition",
            180.0,
            None,
        )),
        K::SerialPlanForced,
    );
}

fn pg_timings(jit: Option<f64>, planning: Option<f64>, execution: f64) -> ExecutionPlan {
    let orders = read(1, "Seq Scan", "orders", 2_000.0, 1.0, execution);
    let mut plan = timed(plan(PlanDialect::Postgres, vec![orders], &[]), execution);
    plan.jit_time_ms = jit;
    plan.jit_functions = jit.map(|_| 14);
    plan.planning_time_ms = planning;
    plan
}

#[test]
fn jit_overhead_fires_when_compilation_is_a_large_part_of_the_run() {
    let findings = findings_for_plan(&pg_timings(Some(420.0), None, 600.0));
    let finding = get(&findings, K::JitOverhead);
    assert_eq!(variant(finding), "slow");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(sql(finding), "SET jit = off;");
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some("SET jit_above_cost = 1000000;")
    );
    assert!(evidence(finding).contains("14 functions compiled"));

    // 60 ms of a 9 s statement paid for itself.
    assert_none(
        &findings_for_plan(&pg_timings(Some(60.0), None, 9_000.0)),
        K::JitOverhead,
    );
    assert_none(
        &findings_for_plan(&pg_timings(Some(8.0), None, 10.0)),
        K::JitOverhead,
    );
}

#[test]
fn planning_time_fires_only_when_planning_outlasts_execution() {
    let findings = findings_for_plan(&pg_timings(None, Some(140.0), 6.0));
    let finding = get(&findings, K::PlanningTime);
    assert_eq!(variant(finding), "dominant");
    assert_eq!(finding.severity, S::Info);

    assert_none(
        &findings_for_plan(&pg_timings(None, Some(140.0), 900.0)),
        K::PlanningTime,
    );
    // Both tiny: nothing to save.
    assert_none(
        &findings_for_plan(&pg_timings(None, Some(3.0), 1.0)),
        K::PlanningTime,
    );
}
