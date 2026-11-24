//! Rules for CockroachDB, Trino, Spark and DuckDB's filtered scans, run on
//! the engines' own plan text.

use super::*;

fn fixture(name: &str) -> ExecutionPlan {
    let path = format!("{}/tests/golden/plans/{name}", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    import_plan_document(&text)
        .unwrap_or_else(|error| panic!("{name}: {error:?}"))
        .plans
        .remove(0)
}

fn text_plan(text: &str) -> ExecutionPlan {
    import_plan_document(text)
        .unwrap_or_else(|error| panic!("{error:?}"))
        .plans
        .remove(0)
}

#[test]
fn cockroachdb_full_scan_under_a_selective_filter_is_reported_with_measured_rows() {
    // filter (actual row count: 1,334) over scan orders@orders_pkey
    // (actual row count: 20,000, spans: FULL SCAN).
    let plan = fixture("cockroachdb/crdb243-q1-analyze-table.txt");
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::TableScan);
    assert_eq!(variant(finding), "cockroach_filtered");
    assert_eq!(finding.confidence, C::High);
    assert_eq!(
        finding.summary.fallback(),
        "The scan of orders reads all 20.0K rows (spans: FULL SCAN) and the filter keeps 1,334 (7%)."
    );
    let evidence = evidence(finding);
    assert!(evidence.contains("Rows read: 20.0K"), "{evidence}");
    assert!(evidence.contains("orders_pkey"), "{evidence}");
    // Nothing written for PostgreSQL leaks in.
    for finding in &findings {
        let text = format!(
            "{} {} {}",
            finding.summary.fallback(),
            finding.why.fallback(),
            finding.action.fallback()
        );
        assert!(
            !text.contains("PostgreSQL") && !text.contains("SQL Server"),
            "{text}"
        );
    }
}

#[test]
fn cockroachdb_index_recommendations_are_reported_as_cockroachdbs_own() {
    let plan = fixture("cockroachdb/crdb243-q4-verbose-tsv.txt");
    let findings = findings_for_plan(&plan);
    let recommended: Vec<&PlanFinding> = findings
        .iter()
        .filter(|finding| finding.kind == K::MissingIndex)
        .collect();
    assert_eq!(recommended.len(), 3, "{}", render(&findings));
    for finding in &recommended {
        assert_eq!(variant(finding), "recommended");
        assert!(
            finding
                .summary
                .fallback()
                .starts_with("CockroachDB recommends an index on")
        );
        // CockroachDB prints no impact figure; none is invented.
        assert!(finding.impact.is_none());
        assert!(!evidence(finding).contains("impact"));
    }
    assert!(
        recommended.iter().any(|finding| finding.summary.fallback()
            == "CockroachDB recommends an index on public.orders (total) for this statement."),
        "{}",
        render(&findings)
    );
    // The scans those recommendations cover are not reported a second time.
    assert_none(&findings, K::TableScan);
}

#[test]
fn cockroachdb_constrained_scans_are_quiet() {
    for name in [
        "cockroachdb/crdb243-q6-analyze-tsv.txt",
        "cockroachdb/crdb243-q7-analyze-verbose-tsv.txt",
        "cockroachdb/crdb243-q5-explain-table.txt",
    ] {
        let findings = findings_for_plan(&fixture(name));
        assert!(findings.is_empty(), "{name}:\n{}", render(&findings));
    }
}

#[test]
fn trino_filter_over_connector_rows_is_reported_from_input_and_output() {
    // ScanFilterProject on orders: Input: 15000 rows, Filtered: 99.89%.
    let plan = fixture("trino/trino484-q4-analyze-verbose-aligned.txt");
    let findings = findings_for_plan(&plan);
    let finding = get(&findings, K::FilterDiscardsRows);
    assert_eq!(variant(finding), "trino_scan");
    assert_eq!(
        finding.summary.fallback(),
        "ScanFilterProject on orders received 15.0K rows from the connector and its filter kept 16 (0.11%)."
    );
    let evidence = evidence(finding);
    assert!(evidence.contains("Table: tiny.orders"), "{evidence}");
    assert!(evidence.contains("Rows read: 15.0K"), "{evidence}");
}

#[test]
fn trino_small_or_unselective_scans_and_estimated_plans_are_quiet() {
    for name in [
        // customer: Input 1500 rows, Filtered 77.53%.
        "trino/trino484-q2-analyze-aligned.txt",
        "trino/trino484-q2-explain-aligned.txt",
        "trino/trino484-q5-explain-aligned.txt",
    ] {
        let findings = findings_for_plan(&fixture(name));
        assert!(findings.is_empty(), "{name}:\n{}", render(&findings));
    }
}

const SPARK_SCAN: &str = "== Physical Plan ==\n*(1) Filter (isnotnull(total#7) AND (cast(total#7 as double) > 400.5))\n+- *(1) ColumnarToRow\n   +- FileScan parquet spark_catalog.default.orders[id#5L,customer_id#6L,total#7,status#8] Batched: true, DataFilters: [isnotnull(total#7), (cast(total#7 as double) > 400.5)], Format: Parquet, Location: InMemoryFileIndex(1 paths)[file:/tmp/spark-warehouse/orders], PartitionFilters: [], PushedFilters: [IsNotNull(total)], ReadSchema: struct<id:bigint,customer_id:bigint,total:decimal(11,2),status:string>\n";

#[test]
fn spark_filter_that_is_not_pushed_to_the_reader_is_reported() {
    let findings = findings_for_plan(&text_plan(SPARK_SCAN));
    let finding = get(&findings, K::NonSargable);
    assert_eq!(variant(finding), "spark_not_pushed");
    assert_eq!(finding.severity, S::Info);
    // Spark has no row counts in EXPLAIN, so the finding claims no share.
    assert!(finding.impact.is_none());
    assert_eq!(
        finding.summary.fallback(),
        "The scan of default.orders filters on (cast(total as double) > 400.5), and none of it is pushed to the Parquet reader."
    );

    // Every fixture pushes its comparisons down: nothing to report.
    for name in [
        "spark/spark43-q1-simple.txt",
        "spark/spark43-q2-simple.txt",
        "spark/spark43-q4-noaqe-formatted.txt",
        "spark/spark43-q5-formatted.txt",
        "spark/spark43-q6-simple.txt",
    ] {
        let findings = findings_for_plan(&fixture(name));
        assert!(findings.is_empty(), "{name}:\n{}", render(&findings));
    }
}

#[test]
fn spark_join_without_a_condition_is_reported() {
    let cross = "== Physical Plan ==\nCartesianProduct\n:- *(1) ColumnarToRow\n:  +- FileScan parquet spark_catalog.default.orders[id#5L] Batched: true, DataFilters: [], Format: Parquet, Location: InMemoryFileIndex(1 paths)[file:/tmp/spark-warehouse/orders], PartitionFilters: [], PushedFilters: [], ReadSchema: struct<id:bigint>\n+- *(2) ColumnarToRow\n   +- FileScan parquet spark_catalog.default.customers[id#31L] Batched: true, DataFilters: [], Format: Parquet, Location: InMemoryFileIndex(1 paths)[file:/tmp/spark-warehouse/customers], PartitionFilters: [], PushedFilters: [], ReadSchema: struct<id:bigint>\n";
    let findings = findings_for_plan(&text_plan(cross));
    let finding = get(&findings, K::NoJoin);
    assert_eq!(variant(finding), "unknown");
    assert_eq!(finding.severity, S::Info);

    // A nested loop join with a condition is a legitimate non-equality join.
    let conditioned = cross.replace(
        "CartesianProduct\n",
        "BroadcastNestedLoopJoin BuildRight, Inner, (id#5L < id#31L)\n",
    );
    assert_none(&findings_for_plan(&text_plan(&conditioned)), K::NoJoin);
}

fn duckdb_join(estimated: f64, actual: f64, filtered: bool) -> ExecutionPlan {
    let join = node(1, "HASH_JOIN", 50_000.0, 1.0, 40.0);
    let mut orders = est(
        read(2, "TABLE_SCAN", "orders", actual, 1.0, 20.0),
        estimated,
        0.0,
    );
    if filtered {
        orders.filter = Some("status='paid'".into());
    }
    let customers = est(
        read(3, "TABLE_SCAN", "customers", 50_000.0, 1.0, 5.0),
        50_000.0,
        0.0,
    );
    plan(
        PlanDialect::DuckDb,
        vec![join, orders, customers],
        &[(1, 2), (1, 3)],
    )
}

#[test]
fn duckdb_filtered_scan_estimate_is_compared_with_the_actual_cardinality() {
    let findings = findings_for_plan(&duckdb_join(2_400.0, 1_200_000.0, true));
    let finding = get(&findings, K::EstimateMismatch);
    assert_eq!(variant(finding), "duckdb_scan");
    assert_eq!(
        finding.summary.fallback(),
        "DuckDB estimated 2,400 rows from the filtered scan of orders and 1.20M came out, off by a factor of 500×."
    );
    assert!(finding.impact.is_none());

    // An unfiltered scan's estimate is the table's row count: nothing to say.
    assert_none(
        &findings_for_plan(&duckdb_join(2_400.0, 1_200_000.0, false)),
        K::EstimateMismatch,
    );
    // An overestimate does not make DuckDB build from the larger input.
    assert_none(
        &findings_for_plan(&duckdb_join(1_200_000.0, 12_000.0, true)),
        K::EstimateMismatch,
    );
    // Within a factor of ten.
    assert_none(
        &findings_for_plan(&duckdb_join(300_000.0, 1_200_000.0, true)),
        K::EstimateMismatch,
    );
    // The real fixtures estimate their filtered scans well.
    for name in [
        "duckdb/duckdb113-join-analyze.json",
        "duckdb/duckdb142-join-analyze.json",
    ] {
        assert_none(&findings_for_plan(&fixture(name)), K::EstimateMismatch);
    }
}

#[test]
fn a_filter_on_a_row_number_is_not_a_statistics_problem() {
    // Oracle: VIEW with filter("RN">299990) over WINDOW SORT, E-Rows 300K and
    // A-Rows 10. No statistics describe the result of ROW_NUMBER().
    let plan = sample("oracle-window-allstats.txt").remove(0);
    let analysis = crate::plans::analyze_plan(&plan);
    let view = analysis
        .nodes
        .iter()
        .find(|node| node.operator.key == "oracle.view")
        .expect("the VIEW step");
    assert!(view.flags.filters_window_result);
    assert!(!view.flags.misestimate_origin);
    assert!(view.metrics.estimate_factor.is_some_and(|f| f > 1_000.0));
    let findings = findings_for_plan(&plan);
    assert!(
        findings
            .iter()
            .all(|finding| finding.kind != FindingKind::EstimateMismatch),
        "{:?}",
        findings
            .iter()
            .map(|f| f.summary.clone())
            .collect::<Vec<_>>()
    );
    let story = crate::plans::plan_story(&plan, &analysis);
    assert!(
        story
            .summary
            .iter()
            .all(|line| !line.fallback().contains("estimate")),
        "{:?}",
        story
            .summary
            .iter()
            .map(|l| l.fallback())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_sql_server_bitmap_test_is_not_shown_as_the_querys_filter() {
    let plan = sample("sqlserver-actual.xml").remove(0);
    let analysis = crate::plans::analyze_plan(&plan);
    let story = crate::plans::plan_story(&plan, &analysis);
    let notes: Vec<String> = story
        .steps
        .iter()
        .flat_map(|step| {
            step.notes
                .iter()
                .map(|note| note.message.fallback().to_string())
        })
        .collect();
    assert!(
        notes.iter().all(|note| !note.contains("PROBE(")),
        "{notes:?}"
    );
    assert!(
        notes.iter().any(|note| note
            == "Drops rows whose o.CustomerID cannot match in a join higher in the plan (the join built a bitmap filter and pushed it down to this read), which discards 600K rows (>99% of the rows it read)."),
        "{notes:?}"
    );
    let evidence: Vec<String> = findings_for_plan(&plan)
        .iter()
        .flat_map(|finding| {
            finding
                .evidence
                .iter()
                .map(|line| line.fallback().to_string())
        })
        .collect();
    assert!(
        evidence.iter().all(|line| !line.contains("PROBE(")),
        "{evidence:?}"
    );
    assert!(
        evidence.contains(&"Bitmap filter from a join, on: o.CustomerID".to_string()),
        "{evidence:?}"
    );
}
