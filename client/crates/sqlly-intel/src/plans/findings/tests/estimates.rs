//! Row estimates, statistics, parameters.

use super::*;
use crate::plans::warning::PlanWarning;
use crate::plans::{PlanParameter, PlanStatisticsUsage};

/// A node with both an estimate and a measurement.
fn both(node: PlanNode, estimated: f64, actual: f64, time_ms: f64) -> PlanNode {
    ran(est(node, estimated, 100.0), actual, 1.0, time_ms)
}

/// Hash join over a filtered scan of `orders` whose estimate is
/// `estimated` against `actual` measured rows.
fn pg_misestimated_scan(estimated: f64, actual: f64, filter: &str) -> ExecutionPlan {
    let mut join = both(op(1, "Hash Join"), estimated, actual, 900.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let mut orders = both(scan(2, "Seq Scan", "orders"), estimated, actual, 300.0);
    orders.alias = Some("o".into());
    orders.filter = Some(filter.into());
    orders.rows_removed_by_filter = Some(1_000.0);
    let hash = both(op(3, "Hash"), 5_000.0, 5_000.0, 20.0);
    let mut customers = both(scan(4, "Seq Scan", "customers"), 5_000.0, 5_000.0, 10.0);
    customers.alias = Some("c".into());
    timed(
        plan(
            PlanDialect::Postgres,
            vec![join, orders, hash, customers],
            &[(1, 2), (1, 3), (3, 4)],
        ),
        900.0,
    )
}

#[test]
fn estimate_mismatch_fires_where_the_error_starts() {
    let plan = pg_misestimated_scan(40.0, 80_000.0, "(o.status = 'open'::text)");
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::EstimateMismatch);
    assert_eq!(variant(finding), "scan");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(sql(finding), "ANALYZE \"orders\";");
    // The scan, and the join that planned from its number.
    assert_eq!(finding.node_ids, vec![2, 1]);
    assert!(finding.alternatives.is_empty());
    assert!(evidence(finding).contains("80.0K"));
}

#[test]
fn estimate_mismatch_offers_extended_statistics_for_correlated_columns() {
    let plan = pg_misestimated_scan(
        40.0,
        80_000.0,
        "((o.city = 'Oslo'::text) AND (o.country = 'NO'::text))",
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::EstimateMismatch);
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some(
            "CREATE STATISTICS \"st_orders_city_country\" (dependencies, ndistinct) ON \"city\", \"country\" FROM \"orders\";\nANALYZE \"orders\";"
        )
    );
}

#[test]
fn estimate_mismatch_ignores_small_errors_and_small_numbers() {
    // Off by 5x.
    let plan = pg_misestimated_scan(16_000.0, 80_000.0, "(o.status = 'open'::text)");
    assert_none(&findings_for_plan(&plan), K::EstimateMismatch);
    // Off by 50x, but 2 rows against 100.
    let plan = pg_misestimated_scan(2.0, 100.0, "(o.status = 'open'::text)");
    assert_none(&findings_for_plan(&plan), K::EstimateMismatch);
}

#[test]
fn an_overestimate_under_a_limit_is_expected() {
    let limit = both(op(1, "Limit"), 10.0, 10.0, 5.0);
    let sort = both(op(2, "Sort"), 50_000.0, 10.0, 5.0);
    let mut orders = both(scan(3, "Seq Scan", "orders"), 50_000.0, 10.0, 4.0);
    orders.filter = Some("(status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(3.0);
    let plan = timed(
        plan(
            PlanDialect::Postgres,
            vec![limit, sort, orders],
            &[(1, 2), (2, 3)],
        ),
        5.0,
    );
    assert_none(&findings_for_plan(&plan), K::EstimateMismatch);
}

#[test]
fn an_estimate_nothing_plans_from_is_not_reported() {
    // A lone misestimated scan: no join, sort or aggregate above uses it.
    let mut orders = both(scan(1, "Seq Scan", "orders"), 40.0, 80_000.0, 300.0);
    orders.filter = Some("(status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(1_000.0);
    let plan = timed(plan(PlanDialect::Postgres, vec![orders], &[]), 300.0);
    assert_none(&findings_for_plan(&plan), K::EstimateMismatch);
}

/// SQL Server actual plan: seek on Orders under a hash join, estimate off.
fn mssql_parameter_plan(compiled: Option<&str>, runtime: Option<&str>) -> ExecutionPlan {
    let mut join = both(op(1, "Hash Match"), 30.0, 250_000.0, 2_000.0);
    join.logical_op = "Inner Join".into();
    join.join_predicate =
        Some("[Shop].[dbo].[Orders].[CustomerID]=[Shop].[dbo].[Customers].[ID]".into());
    let customers = both(
        scan(2, "Clustered Index Scan", "Customers"),
        5_000.0,
        5_000.0,
        20.0,
    );
    let mut orders = both(scan(3, "Index Seek", "Orders"), 30.0, 250_000.0, 1_200.0);
    orders.schema = Some("dbo".into());
    orders.index_name = Some("IX_Orders_Status".into());
    orders.index_cond = Some("[Shop].[dbo].[Orders].[Status]=[@status]".into());
    let mut plan = timed(
        plan(
            PlanDialect::SqlServer,
            vec![join, customers, orders],
            &[(1, 2), (1, 3)],
        ),
        2_000.0,
    );
    plan.parameters.push(PlanParameter {
        name: "@status".into(),
        data_type: Some("int".into()),
        compiled_value: compiled.map(str::to_string),
        runtime_value: runtime.map(str::to_string),
    });
    plan
}

#[test]
fn parameter_sniffing_needs_different_values_and_a_misestimate_on_that_parameter() {
    let findings = findings_for_plan(&mssql_parameter_plan(Some("(7)"), Some("(1)")));
    let finding = get(&findings, K::ParameterSniffing);
    assert_eq!(variant(finding), "compiled");
    assert_eq!(sql(finding), "OPTION (RECOMPILE)");
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some("OPTION (OPTIMIZE FOR (@status UNKNOWN))")
    );
    assert!(finding.summary.fallback().contains("@status"));
    assert_eq!(finding.node_ids, vec![3]);

    // Same value compiled and run: the estimate is wrong for another reason.
    let same = findings_for_plan(&mssql_parameter_plan(Some("(7)"), Some("(7)")));
    assert_none(&same, K::ParameterSniffing);
    assert_eq!(variant(get(&same, K::EstimateMismatch)), "scan");

    // The parameter differs but no predicate that uses it is misestimated.
    let mut unrelated = mssql_parameter_plan(Some("(7)"), Some("(1)"));
    unrelated.parameters[0].name = "@region".into();
    assert_none(&findings_for_plan(&unrelated), K::ParameterSniffing);
}

#[test]
fn a_variable_without_a_compiled_value_explains_the_estimate() {
    let findings = findings_for_plan(&mssql_parameter_plan(None, Some("(1)")));
    assert_none(&findings, K::ParameterSniffing);
    let finding = get(&findings, K::EstimateMismatch);
    assert_eq!(variant(finding), "unknown_value");
    assert_eq!(sql(finding), "OPTION (RECOMPILE)");
    assert!(finding.summary.fallback().contains("@status"));
}

fn pg_unfiltered_scan(estimated: f64, actual: f64) -> ExecutionPlan {
    let aggregate = both(op(1, "Aggregate"), 1.0, 1.0, 400.0);
    let orders = both(scan(2, "Seq Scan", "orders"), estimated, actual, 380.0);
    timed(
        plan(PlanDialect::Postgres, vec![aggregate, orders], &[(1, 2)]),
        400.0,
    )
}

#[test]
fn stale_statistics_fires_when_an_unfiltered_scan_returns_a_different_table_size() {
    let findings = findings_for_plan(&pg_unfiltered_scan(1_000.0, 480_000.0));
    let finding = get(&findings, K::StaleStatistics);
    assert_eq!(variant(finding), "rows");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(sql(finding), "ANALYZE \"orders\";");
    // The scan's estimate needs no predicate to explain: one finding.
    assert_none(&findings, K::EstimateMismatch);

    // 3x off is worth a note; 1.3x is noise.
    let mild = findings_for_plan(&pg_unfiltered_scan(160_000.0, 480_000.0));
    assert_eq!(get(&mild, K::StaleStatistics).severity, S::Info);
    assert_none(
        &findings_for_plan(&pg_unfiltered_scan(370_000.0, 480_000.0)),
        K::StaleStatistics,
    );
    // A small table.
    assert_none(
        &findings_for_plan(&pg_unfiltered_scan(100.0, 900.0)),
        K::StaleStatistics,
    );
}

#[test]
fn stale_statistics_is_not_inferred_from_a_filtered_scan() {
    // A filter's selectivity can be wrong with fresh statistics.
    let mut plan = pg_unfiltered_scan(1_000.0, 480_000.0);
    plan.nodes[1].filter = Some("(status = 'open'::text)".into());
    plan.nodes[1].rows_removed_by_filter = Some(10.0);
    assert_none(&findings_for_plan(&plan), K::StaleStatistics);
}

fn mssql_modified_statistics(modified: f64) -> ExecutionPlan {
    let mut orders = both(
        scan(1, "Clustered Index Scan", "Orders"),
        200_000.0,
        200_000.0,
        300.0,
    );
    orders.schema = Some("dbo".into());
    orders.rows_read = Some(200_000.0);
    let mut plan = timed(plan(PlanDialect::SqlServer, vec![orders], &[]), 300.0);
    plan.statistics_used.push(PlanStatisticsUsage {
        schema: Some("[dbo]".into()),
        table: "[Orders]".into(),
        statistics: "[IX_Orders_Status]".into(),
        modification_count: Some(modified),
        sampling_percent: Some(12.0),
        last_update: Some("2026-03-01T09:12:44.03".into()),
    });
    plan
}

#[test]
fn sql_server_statistics_with_many_modifications_are_reported() {
    let findings = findings_for_plan(&mssql_modified_statistics(90_000.0));
    let finding = get(&findings, K::StaleStatistics);
    assert_eq!(variant(finding), "modified");
    assert_eq!(finding.severity, S::Info);
    assert_eq!(
        sql(finding),
        "UPDATE STATISTICS [dbo].[Orders] WITH FULLSCAN;"
    );
    assert!(evidence(finding).contains("IX_Orders_Status, 90.0K modifications, sampled 12%"));

    // 2% of the table changed since the statistics were built.
    assert_none(
        &findings_for_plan(&mssql_modified_statistics(4_000.0)),
        K::StaleStatistics,
    );
}

#[test]
fn columns_without_statistics_get_a_create_statistics_statement() {
    let mut orders = both(
        scan(1, "Clustered Index Scan", "Orders"),
        100.0,
        100.0,
        30.0,
    );
    orders.schema = Some("dbo".into());
    orders.columns_without_statistics = vec![
        "[Shop].[dbo].[Orders].[Region]".into(),
        "[Shop].[dbo].[Customers].[Tier]".into(),
    ];
    let plan = timed(plan(PlanDialect::SqlServer, vec![orders], &[]), 30.0);
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::Cardinality);
    assert_eq!(variant(finding), "no_stats");
    // Only the column of the table this operator reads.
    assert_eq!(
        sql(finding),
        "CREATE STATISTICS [st_Orders_Region] ON [dbo].[Orders] ([Region]);"
    );

    let mut clean = plan.clone();
    clean.nodes[0].columns_without_statistics.clear();
    assert_none(&findings_for_plan(&clean), K::Cardinality);
}

fn mssql_table_variable(estimated: f64, actual: f64) -> ExecutionPlan {
    let mut join = both(op(1, "Nested Loops"), estimated, actual, 900.0);
    join.logical_op = "Inner Join".into();
    let ids = both(scan(2, "Table Scan", "@ids"), estimated, actual, 10.0);
    let mut orders = ran(
        est(scan(3, "Clustered Index Seek", "Orders"), 1.0, 1.0),
        1.0,
        actual,
        850.0,
    );
    orders.index_name = Some("PK_Orders".into());
    orders.index_cond = Some("[Shop].[dbo].[Orders].[ID]=[@ids].[ID]".into());
    timed(
        plan(
            PlanDialect::SqlServer,
            vec![join, ids, orders],
            &[(1, 2), (1, 3)],
        ),
        900.0,
    )
}

#[test]
fn table_variable_fires_when_one_row_was_planned_and_thousands_came() {
    let findings = findings_for_plan(&mssql_table_variable(1.0, 60_000.0));
    let finding = get(&findings, K::TableVariable);
    assert_eq!(variant(finding), "rows");
    assert_eq!(sql(finding), "OPTION (RECOMPILE)");
    assert_eq!(finding.node_ids, vec![2, 1]);

    assert_none(
        &findings_for_plan(&mssql_table_variable(1.0, 40.0)),
        K::TableVariable,
    );
    // SQL Server 2019 deferred compilation already plans the real count.
    assert_none(
        &findings_for_plan(&mssql_table_variable(60_000.0, 60_000.0)),
        K::TableVariable,
    );
}

fn mssql_row_goal(read: f64, warned: bool) -> ExecutionPlan {
    let top = both(op(1, "Top"), 10.0, 10.0, 700.0);
    let mut orders = both(scan(2, "Clustered Index Scan", "Orders"), 10.0, 10.0, 690.0);
    orders.schema = Some("dbo".into());
    orders.rows_read = Some(read);
    orders.filter = Some("[Shop].[dbo].[Orders].[Status]=(9)".into());
    if warned {
        orders.warnings.push(PlanWarning::RowGoal);
    }
    timed(
        plan(PlanDialect::SqlServer, vec![top, orders], &[(1, 2)]),
        700.0,
    )
}

#[test]
fn row_goal_fires_when_the_scan_read_far_more_than_the_goal_assumed() {
    let findings = findings_for_plan(&mssql_row_goal(2_000_000.0, true));
    let finding = get(&findings, K::RowGoal);
    assert_eq!(variant(finding), "scan");
    assert_eq!(
        sql(finding),
        "OPTION (USE HINT ('DISABLE_OPTIMIZER_ROWGOAL'))"
    );

    // The goal held: the scan stopped early as planned.
    assert_none(&findings_for_plan(&mssql_row_goal(600.0, true)), K::RowGoal);
    // No row goal in the plan.
    assert_none(
        &findings_for_plan(&mssql_row_goal(2_000_000.0, false)),
        K::RowGoal,
    );
}
