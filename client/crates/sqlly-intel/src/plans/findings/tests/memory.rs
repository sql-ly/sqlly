//! Spills, memory grants, temporary tables.

use super::*;
use crate::plans::warning::PlanWarning;
use crate::plans::{PlanMemoryGrant, PlanSpill};

fn pg_disk_sort(disk_kb: f64, rows: f64, space: &str) -> ExecutionPlan {
    let mut sort = node(1, "Sort", rows, 1.0, 900.0);
    sort.sort_keys = vec!["o.total".into(), "c.name".into()];
    sort.sort_method = Some("external merge".into());
    sort.sort_space_type = Some(space.into());
    sort.sort_space_kb = Some(disk_kb);
    // What the PostgreSQL parser records for "Sort Space Type: Disk".
    if space == "Disk" {
        sort.spill = Some(PlanSpill {
            kind: "sort".into(),
            written_kb: Some(disk_kb),
            ..Default::default()
        });
    }
    let mut join = node(2, "Hash Join", rows, 1.0, 300.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let orders = read(3, "Seq Scan", "orders", rows, 1.0, 150.0);
    let hash = node(4, "Hash", 500.0, 1.0, 5.0);
    let customers = read(5, "Seq Scan", "customers", 500.0, 1.0, 4.0);
    timed(
        plan(
            PlanDialect::Postgres,
            vec![sort, join, orders, hash, customers],
            &[(1, 2), (2, 3), (2, 4), (4, 5)],
        ),
        900.0,
    )
}

#[test]
fn postgres_disk_sort_gets_a_work_mem_sized_from_the_spill() {
    let findings = findings_for_plan(&pg_disk_sort(48_000.0, 400_000.0, "Disk"));
    let finding = get(&findings, K::Spill);
    assert_eq!(variant(finding), "pg_sort");
    assert_eq!(finding.severity, S::Warning);
    // Twice 47 MB, rounded up to a power of two.
    assert_eq!(sql(finding), "SET work_mem = '128MB';");
    assert_eq!(finding.node_ids, vec![1]);
    // A sort over a join has no single index to suggest.
    assert_none(&findings, K::ExpensiveSort);
}

#[test]
fn postgres_sorts_in_memory_or_barely_on_disk_are_not_spills() {
    assert_none(
        &findings_for_plan(&pg_disk_sort(48_000.0, 400_000.0, "Memory")),
        K::Spill,
    );
    assert_none(
        &findings_for_plan(&pg_disk_sort(600.0, 400_000.0, "Disk")),
        K::Spill,
    );
}

#[test]
fn a_spill_too_large_for_a_session_setting_gets_no_set_statement() {
    let findings = findings_for_plan(&pg_disk_sort(6_000_000.0, 40_000_000.0, "Disk"));
    let finding = get(&findings, K::Spill);
    assert_eq!(finding.action_sql, None);
    assert!(
        finding
            .action
            .key()
            .is_some_and(|key| key.ends_with("spill.pg_sort_large")),
        "{:?}",
        finding.action
    );
}

fn pg_hash_batches(batches: u32, rows: f64) -> ExecutionPlan {
    let mut join = node(1, "Hash Join", rows, 1.0, 900.0);
    join.join_type = Some("Inner".into());
    join.join_predicate = Some("(o.customer_id = c.id)".into());
    let orders = read(2, "Seq Scan", "orders", rows, 1.0, 150.0);
    let mut hash = node(3, "Hash", rows, 1.0, 500.0);
    hash.hash_batches = Some(batches);
    // What the PostgreSQL parser records for "Batches: n" above 1.
    if batches > 1 {
        hash.spill = Some(PlanSpill {
            kind: "hash".into(),
            ..Default::default()
        });
    }
    hash.original_hash_batches = Some(1);
    hash.peak_memory_kb = Some(4_096.0);
    let customers = read(4, "Seq Scan", "customers", rows, 1.0, 100.0);
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
fn postgres_hash_in_batches_is_a_spill() {
    let findings = findings_for_plan(&pg_hash_batches(16, 600_000.0));
    let finding = get(&findings, K::Spill);
    assert_eq!(variant(finding), "pg_hash");
    // 4 MB peak x 16 batches, halved, doubled for headroom.
    assert_eq!(sql(finding), "SET work_mem = '64MB';");
    assert!(evidence(finding).contains("16 (planned 1)"));

    assert_none(&findings_for_plan(&pg_hash_batches(1, 600_000.0)), K::Spill);
    assert_none(&findings_for_plan(&pg_hash_batches(4, 2_000.0)), K::Spill);
}

fn mssql_spill(estimated_input: f64, actual_input: f64) -> ExecutionPlan {
    let mut sort = ran(
        est(op(1, "Sort"), estimated_input, 50.0),
        actual_input,
        1.0,
        900.0,
    );
    sort.warnings.push(PlanWarning::SpillToTempDb);
    sort.spill = Some(PlanSpill {
        kind: "sort".into(),
        level: Some(1),
        threads: Some(1),
        written_kb: None,
    });
    let orders = ran(
        est(
            scan(2, "Clustered Index Scan", "Orders"),
            estimated_input,
            30.0,
        ),
        actual_input,
        1.0,
        300.0,
    );
    timed(
        plan(PlanDialect::SqlServer, vec![sort, orders], &[(1, 2)]),
        900.0,
    )
}

#[test]
fn sql_server_spill_names_the_underestimate_when_there_is_one() {
    let findings = findings_for_plan(&mssql_spill(2_000.0, 300_000.0));
    let finding = get(&findings, K::Spill);
    assert_eq!(variant(finding), "tempdb_underestimate");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(finding.node_ids, vec![1, 2]);
    assert!(evidence(finding).contains("SpillToTempDb, spill level 1, 1 threads spilled"));

    let accurate = findings_for_plan(&mssql_spill(300_000.0, 300_000.0));
    assert_eq!(variant(get(&accurate, K::Spill)), "tempdb");

    let mut none = mssql_spill(300_000.0, 300_000.0);
    none.nodes[0].warnings.clear();
    none.nodes[0].spill = None;
    assert_none(&findings_for_plan(&none), K::Spill);
}

fn mssql_grant(granted_kb: f64, used_kb: f64, wait: f64) -> ExecutionPlan {
    let orders = read(1, "Clustered Index Scan", "Orders", 100.0, 1.0, 400.0);
    let mut plan = timed(plan(PlanDialect::SqlServer, vec![orders], &[]), 400.0);
    plan.memory_grant = Some(PlanMemoryGrant {
        requested_kb: Some(granted_kb),
        granted_kb: Some(granted_kb),
        max_used_kb: Some(used_kb),
        grant_wait_seconds: Some(wait),
        max_query_memory_kb: Some(8_000_000.0),
        ..Default::default()
    });
    plan
}

#[test]
fn memory_grant_fires_for_a_large_grant_that_went_unused() {
    let findings = findings_for_plan(&mssql_grant(2_000_000.0, 40_000.0, 0.0));
    let finding = get(&findings, K::MemoryGrant);
    assert_eq!(variant(finding), "excessive");
    assert_eq!(finding.severity, S::Warning);
    // Twice the 40 MB used is 1% of the 8 GB a query may be granted.
    assert_eq!(
        finding.alternatives[0].sql.as_deref(),
        Some("OPTION (MAX_GRANT_PERCENT = 1)")
    );
    // The waste is the server's, not this statement's run time.
    assert_eq!(finding.impact, None);

    // Most of the grant was used.
    assert_none(
        &findings_for_plan(&mssql_grant(2_000_000.0, 1_500_000.0, 0.0)),
        K::MemoryGrant,
    );
    // A small grant is not worth a finding however little of it was used.
    assert_none(
        &findings_for_plan(&mssql_grant(4_096.0, 16.0, 0.0)),
        K::MemoryGrant,
    );
}

#[test]
fn a_small_grant_is_repeated_only_when_the_engine_flagged_it() {
    let mut plan = mssql_grant(4_096.0, 16.0, 0.0);
    plan.warnings.push(PlanWarning::Other(
        "Excessive Grant: granted 4096 kB, used 16 kB".into(),
    ));
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::MemoryGrant);
    assert_eq!(finding.severity, S::Info);
}

#[test]
fn waiting_for_a_memory_grant_is_reported() {
    let findings = findings_for_plan(&mssql_grant(500_000.0, 400_000.0, 6.0));
    let finding = get(&findings, K::MemoryGrant);
    assert_eq!(variant(finding), "wait");
    assert_eq!(finding.severity, S::Warning);
    assert!(sql(finding).contains("sys.dm_exec_query_memory_grants"));
    assert_none(
        &findings_for_plan(&mssql_grant(500_000.0, 400_000.0, 0.2)),
        K::MemoryGrant,
    );
}

fn mysql_temporary(rows: f64) -> ExecutionPlan {
    let mut aggregate = node(1, "Aggregate using temporary table", 1_200.0, 1.0, 400.0);
    aggregate.group_keys = vec!["o.customer_id".into()];
    let mut orders = read(2, "Table scan on o", "orders", rows, 1.0, 120.0);
    orders.alias = Some("o".into());
    timed(
        plan(PlanDialect::MySql, vec![aggregate, orders], &[(1, 2)]),
        400.0,
    )
}

#[test]
fn mysql_grouping_through_a_temporary_table_suggests_an_index_in_group_order() {
    let findings = findings_for_plan(&mysql_temporary(200_000.0));
    let finding = get(&findings, K::TemporaryTable);
    assert_eq!(variant(finding), "group");
    assert_eq!(finding.severity, S::Warning);
    assert_eq!(
        sql(finding),
        "CREATE INDEX `ix_orders_customer_id` ON `orders` (`customer_id`);"
    );
    assert_none(
        &findings_for_plan(&mysql_temporary(900.0)),
        K::TemporaryTable,
    );
}

#[test]
fn mysql_grouping_on_an_expression_gets_no_statement() {
    let mut plan = mysql_temporary(200_000.0);
    plan.nodes[0].group_keys = vec!["year(o.created_at)".into()];
    let findings = findings_for_plan(&plan);
    assert_eq!(get(&findings, K::TemporaryTable).action_sql, None);
}

#[test]
fn sqlite_temp_b_tree_for_group_by_is_a_low_confidence_note() {
    let text = "QUERY PLAN\n|--SCAN orders\n`--USE TEMP B-TREE FOR GROUP BY\n";
    let plans = import_plan_document(text).unwrap().plans;
    let findings = findings_for_plan(&plans[0]);
    let finding = get(&findings, K::TemporaryTable);
    assert_eq!(variant(finding), "sqlite");
    assert_eq!((finding.severity, finding.confidence), (S::Info, C::Low));

    // Two tables: the grouping columns could belong to either.
    let text = "QUERY PLAN\n|--SCAN o\n|--SEARCH c USING INTEGER PRIMARY KEY (rowid=?)\n`--USE TEMP B-TREE FOR GROUP BY\n";
    let plans = import_plan_document(text).unwrap().plans;
    assert_none(&findings_for_plan(&plans[0]), K::TemporaryTable);
}
