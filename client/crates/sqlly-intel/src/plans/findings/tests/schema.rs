//! Rules that need the table definitions.

use super::*;
use crate::plans::schema::SchemaContext;

fn with_schema(plan: &ExecutionPlan, ddl: &str) -> Vec<PlanFinding> {
    let schema = SchemaContext::from_ddl(ddl, plan.dialect.expect("dialect"));
    assert!(!schema.is_empty(), "no tables read from:\n{ddl}");
    findings_for_plan_with_schema(plan, Some(&schema))
}

fn both(node: PlanNode, estimated: f64, actual: f64, loops: f64, time_ms: f64) -> PlanNode {
    ran(est(node, estimated, 100.0), actual, loops, time_ms)
}

const PG_ORDERS: &str = "
CREATE TABLE customers (
    id integer PRIMARY KEY,
    name text NOT NULL
);
CREATE TABLE orders (
    id integer PRIMARY KEY,
    customer_id integer REFERENCES customers (id),
    status text NOT NULL,
    total numeric(10,2)
);
";

/// `orders` read in full for 20 open rows, where the optimizer expected
/// `estimated`.
fn pg_status_scan(estimated: f64) -> ExecutionPlan {
    let mut orders = both(scan(1, "Seq Scan", "orders"), estimated, 20.0, 1.0, 800.0);
    orders.filter = Some("(status = 'open'::text)".into());
    orders.rows_removed_by_filter = Some(499_980.0);
    timed(plan(PlanDialect::Postgres, vec![orders], &[]), 800.0)
}

#[test]
fn index_not_used_explains_a_scan_the_optimizer_chose_from_a_bad_estimate() {
    let ddl = format!("{PG_ORDERS}CREATE INDEX orders_status_idx ON orders (status);");
    let findings = with_schema(&pg_status_scan(120_000.0), &ddl);
    let finding = get(&findings, K::SchemaIndexNotUsed);
    assert_eq!(variant(finding), "misestimate");
    assert_eq!(finding.confidence, C::Medium);
    assert_eq!(sql(finding), "ANALYZE \"orders\";");
    assert!(evidence(finding).contains("orders_status_idx (status)"));
    // The scan is explained once; no CREATE INDEX for an index that exists.
    assert_none(&findings, K::TableScan);
    assert!(findings.iter().all(|f| !sql(f).starts_with("CREATE INDEX")));
}

#[test]
fn index_not_used_with_an_accurate_estimate_is_a_low_confidence_note() {
    let ddl = format!("{PG_ORDERS}CREATE INDEX orders_status_idx ON orders (status);");
    let findings = with_schema(&pg_status_scan(20.0), &ddl);
    let finding = get(&findings, K::SchemaIndexNotUsed);
    assert_eq!(variant(finding), "unused");
    assert_eq!(finding.confidence, C::Low);
}

#[test]
fn index_not_used_is_silent_without_an_index_that_leads_with_the_column() {
    // No index on status: the ordinary scan finding stands, with its index.
    let findings = with_schema(&pg_status_scan(20.0), PG_ORDERS);
    assert_none(&findings, K::SchemaIndexNotUsed);
    let scan = get(&findings, K::TableScan);
    assert_eq!(
        sql(scan),
        "CREATE INDEX \"ix_orders_status\" ON \"orders\" (\"status\");"
    );
    // The schema was supplied: nothing more it could add.
    assert!(!has_note(scan, "common.note.needs_schema"));

    // status is the second key column: that index cannot seek on it.
    let ddl =
        format!("{PG_ORDERS}CREATE INDEX orders_cust_status ON orders (customer_id, status);");
    assert_none(
        &with_schema(&pg_status_scan(20.0), &ddl),
        K::SchemaIndexNotUsed,
    );
}

#[test]
fn mysql_number_compared_with_a_string_column_is_named_as_the_reason() {
    let ddl = "
CREATE TABLE contacts (
    id int NOT NULL PRIMARY KEY,
    phone varchar(20) NOT NULL,
    KEY ix_contacts_phone (phone)
) ENGINE=InnoDB;";
    let build = |filter: &str| {
        let top = node(1, "Filter", 1.0, 1.0, 300.0);
        let mut top = top;
        top.filter = Some(filter.into());
        let mut contacts = read(2, "Table scan on c", "contacts", 400_000.0, 1.0, 250.0);
        contacts.alias = Some("c".into());
        timed(
            plan(PlanDialect::MySql, vec![top, contacts], &[(1, 2)]),
            300.0,
        )
    };
    let findings = with_schema(&build("(c.phone = 5551234)"), ddl);
    let finding = get(&findings, K::SchemaIndexNotUsed);
    assert_eq!(variant(finding), "literal_type");
    assert_eq!(finding.confidence, C::High);
    assert!(evidence(finding).contains("phone varchar(20)"));

    // A quoted literal has the column's type; the reason is something else.
    let findings = with_schema(&build("(c.phone = '5551234')"), ddl);
    assert_ne!(
        variant(get(&findings, K::SchemaIndexNotUsed)),
        "literal_type"
    );
}

/// Hash join of customers to orders on the foreign key; `orders` is read in
/// full.
fn pg_fk_join(order_rows: f64) -> ExecutionPlan {
    let mut join = node(1, "Hash Join", order_rows, 1.0, 900.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let mut orders = read(2, "Seq Scan", "orders", order_rows, 1.0, 600.0);
    orders.alias = Some("o".into());
    let hash = node(3, "Hash", 10.0, 1.0, 1.0);
    let mut customers = read(4, "Index Scan", "customers", 10.0, 1.0, 1.0);
    customers.alias = Some("c".into());
    customers.index_name = Some("customers_pkey".into());
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
fn fk_unindexed_fires_for_a_join_on_a_foreign_key_with_no_index() {
    let findings = with_schema(&pg_fk_join(800_000.0), PG_ORDERS);
    let finding = get(&findings, K::SchemaFkUnindexed);
    assert_eq!(variant(finding), "join");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(
        sql(finding),
        "CREATE INDEX \"ix_orders_customer_id\" ON \"orders\" (\"customer_id\");"
    );
    assert!(evidence(finding).contains("orders.customer_id references customers.id"));
    assert_eq!(finding.node_ids, vec![2, 1]);
    assert!(has_note(finding, "common.note.write_cost"));
}

#[test]
fn fk_unindexed_is_silent_when_indexed_small_or_not_a_foreign_key() {
    let indexed = format!("{PG_ORDERS}CREATE INDEX orders_customer_idx ON orders (customer_id);");
    assert_none(
        &with_schema(&pg_fk_join(800_000.0), &indexed),
        K::SchemaFkUnindexed,
    );
    assert_none(
        &with_schema(&pg_fk_join(300.0), PG_ORDERS),
        K::SchemaFkUnindexed,
    );
    let no_fk = PG_ORDERS.replace(" REFERENCES customers (id)", "");
    assert_none(
        &with_schema(&pg_fk_join(800_000.0), &no_fk),
        K::SchemaFkUnindexed,
    );
    // Without the schema nothing says the column is a foreign key.
    assert_none(
        &findings_for_plan(&pg_fk_join(800_000.0)),
        K::SchemaFkUnindexed,
    );
}

#[test]
fn type_mismatch_fires_for_a_join_between_text_and_integer_columns() {
    let ddl = "
CREATE TABLE customers (id integer PRIMARY KEY, name text);
CREATE TABLE orders (id integer PRIMARY KEY, customer_id varchar(20), status text);";
    let findings = with_schema(&pg_fk_join(800_000.0), ddl);
    let finding = get(&findings, K::SchemaTypeMismatch);
    assert_eq!(variant(finding), "join");
    assert_eq!(finding.severity, S::Warning);
    assert!(
        evidence(finding).contains("orders.customer_id varchar(20), customers.id integer"),
        "{}",
        evidence(finding)
    );
    assert_eq!(finding.node_ids, vec![1, 2, 4]);

    // integer against bigint keeps index use.
    let ddl = "
CREATE TABLE customers (id bigint PRIMARY KEY, name text);
CREATE TABLE orders (id integer PRIMARY KEY, customer_id integer, status text);";
    assert_none(
        &with_schema(&pg_fk_join(800_000.0), ddl),
        K::SchemaTypeMismatch,
    );
}

#[test]
fn collation_mismatch_is_reported_on_mysql() {
    let ddl = |collation: &str| {
        format!(
            "CREATE TABLE customers (code varchar(12) COLLATE utf8mb4_0900_ai_ci NOT NULL, PRIMARY KEY (code));
CREATE TABLE orders (id int NOT NULL PRIMARY KEY, customer_code varchar(12) COLLATE {collation} NOT NULL);"
        )
    };
    let mut join = node(1, "Inner hash join", 300_000.0, 1.0, 700.0);
    join.join_predicate = Some("(o.customer_code = c.code)".into());
    let mut orders = read(2, "Table scan on o", "orders", 300_000.0, 1.0, 200.0);
    orders.alias = Some("o".into());
    let mut customers = read(3, "Table scan on c", "customers", 5_000.0, 1.0, 10.0);
    customers.alias = Some("c".into());
    let plan = timed(
        plan(
            PlanDialect::MySql,
            vec![join, orders, customers],
            &[(1, 2), (1, 3)],
        ),
        700.0,
    );
    let findings = with_schema(&plan, &ddl("utf8mb4_general_ci"));
    let finding = get(&findings, K::SchemaTypeMismatch);
    assert_eq!(variant(finding), "collation");
    assert!(evidence(finding).contains("utf8mb4_general_ci"));

    assert_none(
        &with_schema(&plan, &ddl("utf8mb4_0900_ai_ci")),
        K::SchemaTypeMismatch,
    );
}

fn pg_not_in() -> ExecutionPlan {
    let mut customers = read(1, "Seq Scan", "customers", 40.0, 1.0, 900.0);
    customers.alias = Some("c".into());
    customers.filter = Some("(NOT (hashed SubPlan 1))".into());
    customers.rows_removed_by_filter = Some(59_960.0);
    let mut orders = read(2, "Seq Scan", "orders", 800_000.0, 1.0, 500.0);
    orders.alias = Some("o".into());
    orders.parent_relationship = Some("SubPlan".into());
    orders.subplan_name = Some("SubPlan 1".into());
    orders.output_columns = vec!["o.customer_id".into()];
    timed(
        plan(PlanDialect::Postgres, vec![customers, orders], &[(1, 2)]),
        900.0,
    )
}

#[test]
fn nullable_not_in_fires_for_a_subquery_column_that_allows_null() {
    let findings = with_schema(&pg_not_in(), PG_ORDERS);
    let finding = get(&findings, K::SchemaNullableNotIn);
    assert_eq!(variant(finding), "anti");
    assert_eq!(finding.severity, S::Warning);
    assert!(evidence(finding).contains("orders.customer_id integer NULL"));
    assert_eq!(finding.node_ids, vec![1, 2]);

    let not_null = PG_ORDERS.replace(
        "customer_id integer REFERENCES customers (id)",
        "customer_id integer NOT NULL REFERENCES customers (id)",
    );
    assert_none(
        &with_schema(&pg_not_in(), &not_null),
        K::SchemaNullableNotIn,
    );
    assert_none(&findings_for_plan(&pg_not_in()), K::SchemaNullableNotIn);
}

#[test]
fn heap_table_fires_for_a_large_sql_server_table_without_a_clustered_index() {
    let events = read(1, "Table Scan", "Events", 400_000.0, 1.0, 300.0);
    let plan = timed(plan(PlanDialect::SqlServer, vec![events], &[]), 300.0);
    let heap = "CREATE TABLE dbo.Events (Id int NOT NULL, Kind varchar(20) NOT NULL, At datetime2 NOT NULL);";
    let findings = with_schema(&plan, heap);
    let finding = get(&findings, K::SchemaHeapTable);
    assert_eq!(variant(finding), "heap");
    assert_eq!((finding.severity, finding.impact), (S::Info, None));

    let clustered = "CREATE TABLE dbo.Events (Id int NOT NULL PRIMARY KEY CLUSTERED, Kind varchar(20) NOT NULL, At datetime2 NOT NULL);";
    assert_none(&with_schema(&plan, clustered), K::SchemaHeapTable);
    let columnstore =
        format!("{heap}\nCREATE CLUSTERED COLUMNSTORE INDEX CCI_Events ON dbo.Events;");
    assert_none(&with_schema(&plan, &columnstore), K::SchemaHeapTable);
}

#[test]
fn a_table_without_a_primary_key_is_noted_only_when_large() {
    let build = |rows: f64| {
        let events = read(1, "Seq Scan", "events", rows, 1.0, 300.0);
        timed(plan(PlanDialect::Postgres, vec![events], &[]), 300.0)
    };
    let ddl = "CREATE TABLE events (id integer, kind text, at timestamptz);";
    let findings = with_schema(&build(400_000.0), ddl);
    assert_eq!(
        variant(get(&findings, K::SchemaHeapTable)),
        "no_primary_key"
    );
    assert_none(&with_schema(&build(900.0), ddl), K::SchemaHeapTable);
    let keyed = "CREATE TABLE events (id integer PRIMARY KEY, kind text, at timestamptz);";
    assert_none(&with_schema(&build(400_000.0), keyed), K::SchemaHeapTable);
}

#[test]
fn wide_index_fires_for_an_index_with_six_key_columns() {
    let mut orders = read(1, "Index Scan", "orders", 200.0, 1.0, 5.0);
    orders.index_name = Some("orders_everything".into());
    orders.index_cond = Some("(a = 1)".into());
    let plan = timed(plan(PlanDialect::Postgres, vec![orders], &[]), 5.0);
    let table = "CREATE TABLE orders (id integer PRIMARY KEY, a int, b int, c int, d int, e int, f int, g int);";
    let wide = format!("{table}\nCREATE INDEX orders_everything ON orders (a, b, c, d, e, f);");
    let findings = with_schema(&plan, &wide);
    let finding = get(&findings, K::SchemaWideIndex);
    assert_eq!(variant(finding), "keys");
    assert_eq!(finding.severity, S::Info);
    assert!(evidence(finding).contains("orders_everything (a, b, c, d, e, f)"));

    let narrow = format!("{table}\nCREATE INDEX orders_everything ON orders (a, b);");
    assert_none(&with_schema(&plan, &narrow), K::SchemaWideIndex);
}

#[test]
fn stale_schema_is_reported_when_the_plan_uses_objects_the_schema_lacks() {
    let mut orders = read(1, "Index Scan", "orders", 200.0, 1.0, 5.0);
    orders.index_name = Some("orders_created_idx".into());
    orders.index_cond = Some("(orders.created_at > '2024-01-01'::date)".into());
    let audit = read(2, "Seq Scan", "audit_log", 50.0, 1.0, 1.0);
    let top = node(3, "Nested Loop", 200.0, 1.0, 8.0);
    let plan = timed(
        plan(
            PlanDialect::Postgres,
            vec![top, orders, audit],
            &[(3, 1), (3, 2)],
        ),
        8.0,
    );
    let ddl = format!("{PG_ORDERS}CREATE INDEX orders_status_idx ON orders (status);");
    let findings = with_schema(&plan, &ddl);
    let finding = get(&findings, K::SchemaStale);
    assert_eq!(variant(finding), "mismatch");
    let lines = evidence(finding);
    assert!(
        lines.contains("index orders_created_idx on orders"),
        "{lines}"
    );
    assert!(lines.contains("table audit_log"), "{lines}");
    assert!(lines.contains("column orders.created_at"), "{lines}");
}

#[test]
fn stale_schema_is_silent_when_everything_matches_or_nothing_does() {
    let mut orders = read(1, "Index Scan", "orders", 200.0, 1.0, 5.0);
    orders.index_name = Some("orders_status_idx".into());
    orders.index_cond = Some("(orders.status = 'open'::text)".into());
    let plan = timed(plan(PlanDialect::Postgres, vec![orders], &[]), 5.0);
    let ddl = format!("{PG_ORDERS}CREATE INDEX orders_status_idx ON orders (status);");
    assert!(with_schema(&plan, &ddl).is_empty());

    // The schema of some other database: not stale, just unrelated.
    let other = "CREATE TABLE invoices (id integer PRIMARY KEY);\nCREATE TABLE payments (id integer PRIMARY KEY);";
    assert_none(&with_schema(&plan, other), K::SchemaStale);
    // The primary key's index is named by the engine.
    let mut by_key = plan.clone();
    by_key.nodes[0].index_name = Some("orders_pkey".into());
    by_key.nodes[0].index_cond = Some("(orders.id = 7)".into());
    assert_none(&with_schema(&by_key, &ddl), K::SchemaStale);
}
