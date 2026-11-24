//! How rows are found: scans, index filters, lookups, sorts, index merges.

use super::*;
use crate::plans::warning::{MissingIndex, PlanWarning};

/// PostgreSQL sequential scan of `orders` that reads `read` rows and keeps
/// `kept` of them, under a root that does nothing.
fn pg_filtered_scan(read: f64, kept: f64) -> ExecutionPlan {
    let root = node(1, "Result", kept, 1.0, 100.0);
    let mut orders = read_node(2, "Seq Scan", "orders", kept, 95.0);
    orders.filter = Some("(status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(read - kept);
    timed(
        plan(PlanDialect::Postgres, vec![root, orders], &[(1, 2)]),
        100.0,
    )
}

fn read_node(id: i32, name: &str, table: &str, rows: f64, time_ms: f64) -> PlanNode {
    read(id, name, table, rows, 1.0, time_ms)
}

#[test]
fn table_scan_fires_when_a_filter_discards_most_of_a_large_read() {
    let findings = findings_for_plan(&pg_filtered_scan(100_000.0, 500.0));
    let finding = get(&findings, K::TableScan);
    assert_eq!(variant(finding), "filtered");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(finding.confidence, C::High);
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_status\" ON \"orders\" (\"status\");"
    );
    assert_eq!(
        finding.summary.fallback(),
        "The plan reads all 100K rows of orders and keeps 500 (0.50%)."
    );
    assert!(finding.summary.fallback().contains("orders"));
    assert!(has_note(finding, "common.note.write_cost"));
    assert!(has_note(finding, "common.note.needs_schema"));
    assert_eq!(finding.node_ids, vec![2]);
    let impact = finding.impact.expect("timed plan");
    assert_eq!(impact.basis, ImpactBasis::Time);
    assert!(impact.share > 0.9);
}

#[test]
fn table_scan_is_silent_for_small_or_unselective_reads() {
    // The filter keeps most rows: an index would not help.
    assert_none(
        &findings_for_plan(&pg_filtered_scan(100_000.0, 60_000.0)),
        K::TableScan,
    );
    // A small table is cheap to scan whatever the filter keeps.
    assert_none(
        &findings_for_plan(&pg_filtered_scan(800.0, 4.0)),
        K::TableScan,
    );
}

#[test]
fn table_scan_is_critical_only_on_a_long_statement() {
    let short = findings_for_plan(&pg_filtered_scan(500_000.0, 10.0));
    assert_eq!(get(&short, K::TableScan).severity, S::Warning);
    let mut long = pg_filtered_scan(500_000.0, 10.0);
    for node in &mut long.nodes {
        node.actual_time_ms = node.actual_time_ms.map(|time| time * 50.0);
    }
    long.execution_time_ms = Some(5_000.0);
    assert_eq!(
        get(&findings_for_plan(&long), K::TableScan).severity,
        S::Critical
    );
}

/// SQL Server estimated plan: a clustered index scan that expects to read
/// `read` rows and return `kept`.
fn mssql_estimated_scan(read: f64, kept: f64) -> ExecutionPlan {
    let mut orders = est(scan(1, "Clustered Index Scan", "Orders"), kept, 12.0);
    orders.schema = Some("dbo".into());
    orders.estimated_rows_read = Some(read);
    orders.filter = Some("[Shop].[dbo].[Orders].[Status]=(3)".into());
    plan(PlanDialect::SqlServer, vec![orders], &[])
}

#[test]
fn table_scan_on_an_estimated_plan_needs_the_optimizers_own_numbers() {
    let findings = findings_for_plan(&mssql_estimated_scan(500_000.0, 120.0));
    let finding = get(&findings, K::TableScan);
    assert_eq!(variant(finding), "estimated");
    assert_eq!(finding.confidence, C::Medium);
    assert_eq!(
        sql(finding),
        "CREATE INDEX [IX_Orders_Status] ON [dbo].[Orders] ([Status]);"
    );
    assert!(has_note(finding, "common.note.estimated_only"));
    assert_eq!(
        finding.impact.expect("costed plan").basis,
        ImpactBasis::Cost
    );

    assert_none(
        &findings_for_plan(&mssql_estimated_scan(500_000.0, 200_000.0)),
        K::TableScan,
    );
    assert_none(
        &findings_for_plan(&mssql_estimated_scan(4_000.0, 1.0)),
        K::TableScan,
    );
}

/// Hash join that reads all of `orders` to match `matched` rows of a small
/// filtered `customers`.
fn pg_join_scan(orders_rows: f64, matched: f64) -> ExecutionPlan {
    let mut join = node(1, "Hash Join", matched, 1.0, 400.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let mut orders = read_node(2, "Seq Scan", "orders", orders_rows, 380.0);
    orders.alias = Some("o".into());
    let hash = node(3, "Hash", 3.0, 1.0, 1.0);
    let mut customers = read_node(4, "Index Scan", "customers", 3.0, 0.5);
    customers.alias = Some("c".into());
    customers.index_name = Some("customers_email_idx".into());
    customers.index_cond = Some("(email = 'a@example.com'::text)".into());
    timed(
        plan(
            PlanDialect::Postgres,
            vec![join, orders, hash, customers],
            &[(1, 2), (1, 3), (3, 4)],
        ),
        400.0,
    )
}

#[test]
fn table_scan_fires_for_a_scan_that_feeds_a_join_needing_few_rows() {
    let findings = findings_for_plan(&pg_join_scan(500_000.0, 40.0));
    let finding = get(&findings, K::TableScan);
    assert_eq!(variant(finding), "join");
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_customer_id\" ON \"orders\" (\"customer_id\");"
    );
    assert_eq!(finding.node_ids, vec![2, 1]);
    // A join that keeps most of the scan needs the scan.
    assert_none(
        &findings_for_plan(&pg_join_scan(500_000.0, 400_000.0)),
        K::TableScan,
    );
}

#[test]
fn table_scan_is_not_reported_for_column_stores() {
    let mut orders = read_node(1, "SEQ_SCAN", "orders", 500.0, 95.0);
    orders.filter = Some("status='open'".into());
    orders.rows_read = Some(1_000_000.0);
    let plan = timed(plan(PlanDialect::DuckDb, vec![orders], &[]), 100.0);
    assert_none(&findings_for_plan(&plan), K::TableScan);
}

fn pg_index_filter(removed: f64) -> ExecutionPlan {
    let mut orders = read_node(1, "Index Scan", "orders", 100.0, 90.0);
    orders.index_name = Some("orders_customer_id_idx".into());
    orders.index_cond = Some("(customer_id = 42)".into());
    orders.filter = Some("(status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(removed);
    timed(plan(PlanDialect::Postgres, vec![orders], &[]), 90.0)
}

#[test]
fn filter_discards_rows_fires_when_an_index_read_throws_most_rows_away() {
    let findings = findings_for_plan(&pg_index_filter(80_000.0));
    let finding = get(&findings, K::FilterDiscardsRows);
    assert_eq!(variant(finding), "index");
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_customer_id_status\" ON \"orders\" (\"customer_id\", \"status\");"
    );
    assert!(
        finding
            .summary
            .fallback()
            .contains("orders_customer_id_idx")
    );
    assert!(evidence(finding).contains("80.0K"), "{}", evidence(finding));
    assert_none(&findings, K::TableScan);

    assert_none(
        &findings_for_plan(&pg_index_filter(900.0)),
        K::FilterDiscardsRows,
    );
}

/// SQL Server index seek plus key lookup that runs `lookups` times.
fn mssql_key_lookup(lookups: f64, columns: usize) -> ExecutionPlan {
    let join = node(1, "Nested Loops", lookups, 1.0, 500.0);
    let mut seek = read_node(2, "Index Seek", "Orders", lookups, 40.0);
    seek.schema = Some("dbo".into());
    seek.index_name = Some("IX_Orders_CustomerID".into());
    seek.index_cond = Some("[Shop].[dbo].[Orders].[CustomerID]=(42)".into());
    let mut lookup = ran(scan(3, "Key Lookup", "Orders"), 1.0, lookups, 440.0);
    lookup.schema = Some("dbo".into());
    lookup.index_name = Some("PK_Orders".into());
    lookup.output_columns = (0..columns)
        .map(|index| format!("[Shop].[dbo].[Orders].[Col{index}]"))
        .collect();
    timed(
        plan(
            PlanDialect::SqlServer,
            vec![join, seek, lookup],
            &[(1, 2), (1, 3)],
        ),
        500.0,
    )
}

#[test]
fn key_lookup_fires_on_volume_and_names_the_columns() {
    let findings = findings_for_plan(&mssql_key_lookup(50_000.0, 2));
    let finding = get(&findings, K::KeyLookup);
    assert_eq!(variant(finding), "volume");
    assert!(finding.summary.fallback().contains("IX_Orders_CustomerID"));
    assert!(
        finding.action.fallback().contains("Col0, Col1"),
        "{:?}",
        finding.action
    );
    // The plan does not list the index's key columns, so no statement.
    assert_eq!(finding.action_sql, None);
    assert!(has_note(finding, "common.note.needs_schema"));
    assert_eq!(finding.node_ids, vec![3, 2]);

    assert_eq!(
        variant(get(
            &findings_for_plan(&mssql_key_lookup(50_000.0, 12)),
            K::KeyLookup
        )),
        "wide"
    );
    assert_none(
        &findings_for_plan(&mssql_key_lookup(200.0, 2)),
        K::KeyLookup,
    );
}

fn pg_index_only(rows: f64, fetches: f64) -> ExecutionPlan {
    let mut orders = read_node(1, "Index Only Scan", "orders", rows, 60.0);
    orders.index_name = Some("orders_status_idx".into());
    orders.index_cond = Some("(status = 'open'::text)".into());
    orders.heap_fetches = Some(fetches);
    timed(plan(PlanDialect::Postgres, vec![orders], &[]), 60.0)
}

#[test]
fn heap_fetches_fires_when_an_index_only_scan_visits_the_table() {
    let findings = findings_for_plan(&pg_index_only(50_000.0, 40_000.0));
    let finding = get(&findings, K::HeapFetches);
    assert_eq!(variant(finding), "volume");
    assert_eq!(sql(finding), "VACUUM (ANALYZE) \"orders\";");
    assert_eq!(finding.severity, S::Warning);

    assert_none(
        &findings_for_plan(&pg_index_only(50_000.0, 120.0)),
        K::HeapFetches,
    );
    // Many fetches, but a small part of a very large read.
    assert_none(
        &findings_for_plan(&pg_index_only(5_000_000.0, 2_000.0)),
        K::HeapFetches,
    );
}

fn pg_sort(rows: f64, filter: Option<&str>) -> ExecutionPlan {
    let mut sort = node(1, "Sort", rows, 1.0, 900.0);
    sort.sort_keys = vec!["o.created_at DESC".into()];
    sort.sort_method = Some("quicksort".into());
    let mut orders = read_node(2, "Seq Scan", "orders", rows, 200.0);
    orders.alias = Some("o".into());
    orders.filter = filter.map(str::to_string);
    if filter.is_some() {
        orders.rows_removed_by_filter = Some(rows);
    }
    timed(
        plan(PlanDialect::Postgres, vec![sort, orders], &[(1, 2)]),
        900.0,
    )
}

#[test]
fn expensive_sort_suggests_an_index_in_sort_order() {
    let findings = findings_for_plan(&pg_sort(300_000.0, None));
    let finding = get(&findings, K::ExpensiveSort);
    assert_eq!(variant(finding), "index");
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_created_at\" ON \"orders\" (\"created_at\");"
    );
    assert_eq!(finding.node_ids, vec![1, 2]);
    // One direction needs no DESC: the index is read backwards.

    // An equality filter goes first in the key.
    let findings = findings_for_plan(&pg_sort(300_000.0, Some("(o.status = 'open'::text)")));
    assert!(
        sql(get(&findings, K::ExpensiveSort)).contains("(\"status\", \"created_at\")"),
        "{}",
        render(&findings)
    );
}

#[test]
fn expensive_sort_is_silent_when_no_single_index_can_serve_it() {
    // A range filter and a sort on another column.
    assert_none(
        &findings_for_plan(&pg_sort(300_000.0, Some("(o.total > 100)"))),
        K::ExpensiveSort,
    );
    // A sort above a join.
    assert_none(
        &findings_for_plan(&crate::plans::plan_fixtures::pg_join()),
        K::ExpensiveSort,
    );
    // A small sort.
    assert_none(&findings_for_plan(&pg_sort(600.0, None)), K::ExpensiveSort);
    // A sort on an expression.
    let mut expression = pg_sort(300_000.0, None);
    expression.nodes[0].sort_keys = vec!["lower(o.email)".into()];
    assert_none(&findings_for_plan(&expression), K::ExpensiveSort);
}

#[test]
fn sqlite_automatic_index_is_reported_with_the_searched_column() {
    let text =
        "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING AUTOMATIC COVERING INDEX (customer_id=?)\n";
    let plans = import_plan_document(text).unwrap().plans;
    let findings = findings_for_plan(&plans[0]);
    let finding = get(&findings, K::AutoIndex);
    assert_eq!(variant(finding), "sqlite");
    assert_eq!(finding.severity, S::Warning);
    // SQLite prints the query's alias, so no statement can name the table.
    assert!(finding.summary.fallback().contains('c'));

    let text = "QUERY PLAN\n|--SCAN o\n`--SEARCH c USING INDEX ix_c_customer (customer_id=?)\n";
    let plans = import_plan_document(text).unwrap().plans;
    assert_none(&findings_for_plan(&plans[0]), K::AutoIndex);
}

fn mssql_index_spool(logical: &str, rows: f64) -> ExecutionPlan {
    let join = node(1, "Nested Loops", 100.0, 1.0, 800.0);
    let outer = read_node(2, "Clustered Index Scan", "Customers", 100.0, 5.0);
    let mut spool = node(3, "Index Spool", rows, 1.0, 780.0);
    spool.logical_op = logical.into();
    spool.index_cond =
        Some("[Shop].[dbo].[Orders].[CustomerID]=[Shop].[dbo].[Customers].[ID]".into());
    let mut orders = read_node(4, "Clustered Index Scan", "Orders", rows, 300.0);
    orders.schema = Some("dbo".into());
    timed(
        plan(
            PlanDialect::SqlServer,
            vec![join, outer, spool, orders],
            &[(1, 2), (1, 3), (3, 4)],
        ),
        800.0,
    )
}

#[test]
fn eager_index_spool_is_reported_as_an_index_the_engine_built() {
    let findings = findings_for_plan(&mssql_index_spool("Eager Spool", 250_000.0));
    let finding = get(&findings, K::AutoIndex);
    assert_eq!(variant(finding), "spool");
    assert!(finding.summary.fallback().contains("dbo.Orders"));
    assert_eq!(finding.node_ids, vec![3, 4]);
    // The generic spool finding would repeat it.
    assert_none(&findings, K::Spool);

    assert_none(
        &findings_for_plan(&mssql_index_spool("Lazy Spool", 250_000.0)),
        K::AutoIndex,
    );
    assert_none(
        &findings_for_plan(&mssql_index_spool("Eager Spool", 300.0)),
        K::AutoIndex,
    );
}

fn pg_bitmap(combine: &str, rows: f64) -> ExecutionPlan {
    let mut heap = read_node(1, "Bitmap Heap Scan", "orders", rows, 120.0);
    heap.filter = None;
    let and = node(2, combine, 0.0, 1.0, 30.0);
    let mut first = read_node(3, "Bitmap Index Scan", "orders", 90_000.0, 14.0);
    first.index_name = Some("orders_status_idx".into());
    first.index_cond = Some("(status = 'open'::text)".into());
    let mut second = read_node(4, "Bitmap Index Scan", "orders", 70_000.0, 12.0);
    second.index_name = Some("orders_customer_id_idx".into());
    second.index_cond = Some("(customer_id = 42)".into());
    timed(
        plan(
            PlanDialect::Postgres,
            vec![heap, and, first, second],
            &[(1, 2), (2, 3), (2, 4)],
        ),
        120.0,
    )
}

#[test]
fn index_merge_fires_for_an_intersection_of_two_indexes() {
    let findings = findings_for_plan(&pg_bitmap("BitmapAnd", 20_000.0));
    let finding = get(&findings, K::IndexMerge);
    assert_eq!(variant(finding), "intersect");
    assert_eq!(finding.severity, S::Info);
    let statement = sql(finding);
    assert!(
        statement.contains("\"status\"") && statement.contains("\"customer_id\""),
        "{statement}"
    );
    // A union of two indexes (OR) is not replaced by one composite index.
    assert_none(
        &findings_for_plan(&pg_bitmap("BitmapOr", 20_000.0)),
        K::IndexMerge,
    );
    assert_none(
        &findings_for_plan(&pg_bitmap("BitmapAnd", 300.0)),
        K::IndexMerge,
    );
}

fn pg_lossy(lossy: f64, exact: f64) -> ExecutionPlan {
    let mut heap = read_node(1, "Bitmap Heap Scan", "orders", 40_000.0, 200.0);
    heap.lossy_heap_blocks = Some(lossy);
    heap.exact_heap_blocks = Some(exact);
    let mut index = read_node(2, "Bitmap Index Scan", "orders", 40_000.0, 10.0);
    index.index_name = Some("orders_status_idx".into());
    index.index_cond = Some("(status = 'open'::text)".into());
    timed(
        plan(PlanDialect::Postgres, vec![heap, index], &[(1, 2)]),
        200.0,
    )
}

#[test]
fn lossy_bitmap_fires_when_much_of_the_bitmap_lost_row_precision() {
    let findings = findings_for_plan(&pg_lossy(5_000.0, 1_200.0));
    let finding = get(&findings, K::LossyBitmap);
    assert_eq!(variant(finding), "recheck");
    assert_eq!(sql(finding), "SHOW work_mem;");
    assert!(evidence(finding).contains("lossy=5,000"));

    assert_none(&findings_for_plan(&pg_lossy(0.0, 6_200.0)), K::LossyBitmap);
    assert_none(&findings_for_plan(&pg_lossy(20.0, 6_200.0)), K::LossyBitmap);
}

fn mssql_missing_index(impact: f64, twice: bool) -> ExecutionPlan {
    let mut orders = read_node(1, "Clustered Index Scan", "Orders", 12.0, 300.0);
    orders.schema = Some("dbo".into());
    orders.rows_read = Some(800_000.0);
    orders.filter = Some("[Shop].[dbo].[Orders].[CustomerID]=(42)".into());
    let mut plan = timed(plan(PlanDialect::SqlServer, vec![orders], &[]), 300.0);
    let warning = PlanWarning::MissingIndex(MissingIndex {
        database: "[Shop]".into(),
        schema: "[dbo]".into(),
        table: "[Orders]".into(),
        equality_columns: vec!["[CustomerID]".into()],
        inequality_columns: vec![],
        included_columns: vec!["[Total]".into()],
        impact,
    });
    plan.warnings.push(warning.clone());
    if twice {
        plan.warnings.push(warning);
    }
    plan
}

#[test]
fn missing_index_repeats_the_engines_request_once_with_measured_rows() {
    let findings = findings_for_plan(&mssql_missing_index(92.5, true));
    let finding = get(&findings, K::MissingIndex);
    assert_eq!(variant(finding), "measured");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(
        sql(finding),
        "CREATE INDEX [IX_Orders_CustomerID] ON [dbo].[Orders] ([CustomerID]) INCLUDE ([Total]);"
    );
    let impact = finding.impact.expect("optimizer impact");
    assert!(impact.optimizer_estimate);
    assert!((impact.share - 0.925).abs() < 1e-9);
    // The scan is the same fact: reported once.
    assert_none(&findings, K::TableScan);
    assert_eq!(findings[0].kind, K::MissingIndex);

    let low = findings_for_plan(&mssql_missing_index(4.0, false));
    assert_eq!(get(&low, K::MissingIndex).severity, S::Info);
}

#[test]
fn missing_index_needs_the_engine_warning() {
    let mut plan = mssql_missing_index(92.5, false);
    plan.warnings.clear();
    let findings = findings_for_plan(&plan);
    assert_none(&findings, K::MissingIndex);
    // Without the engine's request the scan itself is the finding.
    assert_eq!(variant(get(&findings, K::TableScan)), "filtered");
}
