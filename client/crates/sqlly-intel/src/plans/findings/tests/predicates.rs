//! Predicates an index cannot serve: conversions, functions, leading
//! wildcards.

use super::*;
use crate::plans::schema::SchemaContext;

/// A full scan of `table` that reads `read` rows and keeps `kept` under
/// `filter`, in a statement that ran for 200 ms.
fn filtered(
    dialect: PlanDialect,
    name: &str,
    table: &str,
    filter: &str,
    read: f64,
    kept: f64,
) -> ExecutionPlan {
    let mut node = ran(scan(1, name, table), kept, 1.0, 200.0);
    node.filter = Some(filter.into());
    node.rows_read = Some(read);
    node.rows_removed_by_filter = Some(read - kept);
    timed(plan(dialect, vec![node], &[]), 200.0)
}

/// The same scan on an estimated plan.
fn estimated(
    dialect: PlanDialect,
    name: &str,
    table: &str,
    filter: &str,
    read: f64,
    kept: f64,
) -> ExecutionPlan {
    let mut node = est(scan(1, name, table), kept, 500.0);
    node.filter = Some(filter.into());
    node.estimated_rows_read = Some(read);
    plan(dialect, vec![node], &[])
}

const MSSQL_CONVERT: &str =
    "CONVERT_IMPLICIT(nvarchar(20),[Shop].[dbo].[Orders].[OrderNo],0)=[@no]";

fn mssql(filter: &str, read: f64, kept: f64) -> ExecutionPlan {
    let mut plan = filtered(
        PlanDialect::SqlServer,
        "Clustered Index Scan",
        "Orders",
        filter,
        read,
        kept,
    );
    plan.nodes[0].schema = Some("dbo".into());
    plan
}

#[test]
fn sql_server_implicit_conversion_on_a_column_is_reported_with_its_volume() {
    let findings = findings_for_plan(&mssql(MSSQL_CONVERT, 400_000.0, 1.0));
    let finding = get(&findings, K::ImplicitConversion);
    assert_eq!(variant(finding), "scan");
    assert_eq!(finding.confidence, C::High);
    assert_eq!(
        finding.summary.fallback(),
        "SQL Server converts Orders.OrderNo to nvarchar on every row of dbo.Orders before comparing it with @no: 400K rows read, 1 kept."
    );
    // The plan does not say what type the column has.
    assert_eq!(finding.action_sql, None);
    assert!(has_note(finding, "implicit_conversion.needs_schema"));
    // The scan is the same fact.
    assert_none(&findings, K::TableScan);
    assert_none(&findings, K::NonSargable);
}

#[test]
fn sql_server_conversion_with_a_schema_names_the_type_to_pass() {
    let schema = SchemaContext::from_ddl(
        "CREATE TABLE dbo.Orders (OrderID int NOT NULL PRIMARY KEY, OrderNo varchar(20) NOT NULL);",
        PlanDialect::SqlServer,
    );
    let plan = mssql(MSSQL_CONVERT, 400_000.0, 1.0);
    let findings = findings_for_plan_with_schema(&plan, Some(&schema));
    let finding = get(&findings, K::ImplicitConversion);
    assert_eq!(sql(finding), "Orders.OrderNo = CAST(@no AS varchar(20))");
    assert!(finding.action.fallback().contains("varchar(20)"));
    assert!(!has_note(finding, "implicit_conversion.needs_schema"));
}

#[test]
fn sql_server_conversion_on_a_small_read_is_only_a_note() {
    let findings = findings_for_plan(&mssql(MSSQL_CONVERT, 40.0, 1.0));
    let finding = get(&findings, K::ImplicitConversion);
    assert_eq!(variant(finding), "column");
    assert_eq!(finding.severity, S::Info);
}

#[test]
fn a_conversion_on_the_value_side_is_not_a_finding() {
    // The constant is converted, the column is bare: an index still seeks.
    let plan = mssql(
        "[Shop].[dbo].[Orders].[OrderNo]=CONVERT_IMPLICIT(varchar(20),[@no],0)",
        400_000.0,
        1.0,
    );
    let findings = findings_for_plan(&plan);
    assert_none(&findings, K::ImplicitConversion);
    assert_none(&findings, K::NonSargable);
    // It is still a scan that discards nearly everything.
    assert!(has(&findings, K::TableScan));
}

#[test]
fn postgres_cast_to_another_type_is_a_conversion_but_a_text_relabel_is_not() {
    let plan = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "orders",
        "((order_no)::numeric = '42'::numeric)",
        400_000.0,
        1.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::ImplicitConversion);
    assert_eq!(variant(finding), "cast");
    assert_eq!(finding.confidence, C::Medium);
    assert_eq!(finding.alternatives.len(), 1);
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some("CREATE INDEX \"ix_orders_expr\" ON \"orders\" (((order_no)::numeric));")
    );

    // varchar columns print as (col)::text; an index on the column serves it.
    let plan = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "orders",
        "((status)::text = 'open'::text)",
        400_000.0,
        1.0,
    );
    let findings = findings_for_plan(&plan);
    assert_none(&findings, K::ImplicitConversion);
    assert_eq!(
        sql(get(&findings, K::TableScan)),
        "CREATE INDEX \"ix_orders_status\" ON \"orders\" (\"status\");"
    );
}

#[test]
fn oracle_to_number_on_a_column_is_a_conversion() {
    let plan = filtered(
        PlanDialect::Oracle,
        "TABLE ACCESS FULL",
        "ORDERS",
        "TO_NUMBER(\"ORDER_NO\")=42",
        400_000.0,
        1.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::ImplicitConversion);
    assert_eq!(variant(finding), "oracle");
    assert!(
        finding
            .summary
            .fallback()
            .contains("TO_NUMBER(\"ORDER_NO\")")
    );

    let plan = filtered(
        PlanDialect::Oracle,
        "TABLE ACCESS FULL",
        "ORDERS",
        "\"ORDER_NO\"='42'",
        400_000.0,
        1.0,
    );
    assert_none(&findings_for_plan(&plan), K::ImplicitConversion);
}

#[test]
fn function_on_a_column_gets_an_expression_index_where_the_engine_has_them() {
    let plan = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(lower(email) = 'a@example.com'::text)",
        250_000.0,
        1.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "function");
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_customers_expr\" ON \"customers\" (lower(email));"
    );
    assert!(has_note(finding, "common.note.write_cost"));
    assert_none(&findings, K::TableScan);
}

#[test]
fn function_on_a_column_is_silent_for_small_or_unselective_scans() {
    let small = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(lower(email) = 'a@example.com'::text)",
        300.0,
        1.0,
    );
    assert_none(&findings_for_plan(&small), K::NonSargable);
    let unselective = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(lower(email) = 'a@example.com'::text)",
        250_000.0,
        200_000.0,
    );
    assert_none(&findings_for_plan(&unselective), K::NonSargable);
    // A function of constants only leaves the column bare.
    let constant = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(created_at > (now() - '7 days'::interval))",
        250_000.0,
        10.0,
    );
    assert_none(&findings_for_plan(&constant), K::NonSargable);
}

#[test]
fn sql_server_year_function_is_rewritten_as_a_range() {
    let plan = mssql(
        // SQL Server prints YEAR(OrderDate) this way.
        "datepart(year,[Shop].[dbo].[Orders].[OrderDate])=(2024)",
        900_000.0,
        4_000.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "function");
    assert_eq!(
        sql(finding),
        "Orders.OrderDate >= '20240101' AND Orders.OrderDate < '20250101'"
    );
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some("CREATE INDEX [IX_Orders_OrderDate] ON [dbo].[Orders] ([OrderDate]);")
    );
}

#[test]
fn mysql_date_function_is_rewritten_and_the_alias_is_never_used_as_a_table() {
    // EXPLAIN FORMAT=TREE prints the alias only.
    let text = "-> Filter: (cast(o.created_at as date) = DATE'2024-05-01')  (cost=10050 rows=10000) (actual time=0.05..180 rows=120 loops=1)\n    -> Table scan on o  (cost=10050 rows=100000) (actual time=0.04..120 rows=100000 loops=1)\n";
    let plans = import_plan_document(text).unwrap().plans;
    let findings = findings_for_plan(&plans[0]);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "function");
    // MySQL prints DATE(col) as a cast; the rewrite is a range on the column.
    assert_eq!(
        sql(finding),
        "o.created_at >= '2024-05-01' AND o.created_at < '2024-05-01' + INTERVAL 1 DAY"
    );
    // No CREATE INDEX on a table called "o".
    assert!(finding.alternatives.is_empty(), "{}", render(&findings));
}

#[test]
fn leading_wildcard_like_offers_a_trigram_index_on_postgres_only() {
    let plan = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(name ~~ '%smith'::text)",
        250_000.0,
        40.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "like");
    assert_eq!(
        sql(finding),
        "CREATE EXTENSION IF NOT EXISTS pg_trgm;\nCREATE INDEX \"ix_customers_name_trgm\" ON \"customers\" USING gin (\"name\" gin_trgm_ops);"
    );

    let plan = mssql(
        "[Shop].[dbo].[Orders].[Note] like N'%late%'",
        250_000.0,
        40.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "like");
    assert_eq!(finding.action_sql, None);
}

#[test]
fn anchored_or_negated_like_is_not_reported() {
    let anchored = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(name ~~ 'smith%'::text)",
        250_000.0,
        40.0,
    );
    assert_none(&findings_for_plan(&anchored), K::NonSargable);
    let negated = filtered(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(name !~~ '%smith'::text)",
        250_000.0,
        40.0,
    );
    assert_none(&findings_for_plan(&negated), K::NonSargable);
}

#[test]
fn estimated_plans_need_a_large_costed_scan() {
    let plan = estimated(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(lower(email) = 'a@example.com'::text)",
        250_000.0,
        1_250.0,
    );
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "function_unmeasured");
    assert_eq!(finding.confidence, C::Medium);
    assert!(has_note(finding, "common.note.estimated_only"));

    let mut small = estimated(
        PlanDialect::Postgres,
        "Seq Scan",
        "customers",
        "(lower(email) = 'a@example.com'::text)",
        900.0,
        5.0,
    );
    small.nodes[0].estimated_cost = 20.0;
    small.total_subtree_cost = 20.0;
    assert_none(&findings_for_plan(&small), K::NonSargable);
}

#[test]
fn column_stores_are_left_alone() {
    let plan = filtered(
        PlanDialect::DuckDb,
        "SEQ_SCAN",
        "customers",
        "lower(email)='a@example.com'",
        250_000.0,
        1.0,
    );
    assert!(findings_for_plan(&plan).is_empty());
}
