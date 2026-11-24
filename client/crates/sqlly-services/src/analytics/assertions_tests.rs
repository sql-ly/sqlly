use std::collections::BTreeMap;

use sqlly_core::{CellValue, ResultColumn};

use super::*;
use crate::analytics::kpi::Aggregate;
use crate::analytics::profile::{ProfileOptions, profile_rows};
use crate::analytics::test_support::*;
use crate::analytics::time::parse_timestamp;
use crate::analytics::{Completeness, OutputView};

fn orders() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
    let columns = vec![
        col("id", "int"),
        col("customer_id", "int"),
        col("status", "text"),
        col("amount", "decimal(10,2)"),
        col("email", "text"),
        col("created_at", "datetime"),
    ];
    let rows = vec![
        vec![
            i(1),
            i(10),
            s("paid"),
            s("19.99"),
            s("a@example.com"),
            s("2024-05-01 10:00:00"),
        ],
        vec![
            i(2),
            i(11),
            s("paid"),
            s("5.00"),
            s("b@example.com"),
            s("2024-05-02 11:00:00"),
        ],
        vec![
            i(3),
            i(10),
            s("refunded"),
            s("-5.00"),
            s("not-an-email"),
            s("2024-05-03 12:00:00"),
        ],
        vec![
            i(3),
            i(12),
            s("PAID"),
            CellValue::Null,
            CellValue::Null,
            s("2024-05-04 13:00:00"),
        ],
        vec![
            i(5),
            i(99),
            s("void"),
            s("1e9"),
            s("e@example.com"),
            CellValue::Null,
        ],
    ];
    (columns, rows)
}

fn check(assertion: Assertion, context: &AssertionContext<'_>) -> AssertionOutcome {
    evaluate(&AssertionSpec::from(assertion), context)
}

#[test]
fn assertion_table_over_a_fixture() {
    let (columns, rows) = orders();
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let cases: Vec<(Assertion, AssertionStatus, u64)> = vec![
        (
            Assertion::RowCount {
                min: Some(1),
                max: Some(10),
            },
            AssertionStatus::Pass,
            0,
        ),
        (
            Assertion::RowCount {
                min: Some(6),
                max: None,
            },
            AssertionStatus::Fail,
            0,
        ),
        (
            Assertion::Unique {
                columns: vec!["id".into()],
            },
            AssertionStatus::Fail,
            1,
        ),
        (
            Assertion::Unique {
                columns: vec!["id".into(), "customer_id".into()],
            },
            AssertionStatus::Pass,
            0,
        ),
        (
            Assertion::NotNull {
                column: "amount".into(),
                max_null_rate: 0.0,
            },
            AssertionStatus::Fail,
            1,
        ),
        (
            Assertion::NotNull {
                column: "amount".into(),
                max_null_rate: 0.2,
            },
            AssertionStatus::Pass,
            1,
        ),
        (
            Assertion::NotNull {
                column: "id".into(),
                max_null_rate: 0.0,
            },
            AssertionStatus::Pass,
            0,
        ),
        (
            Assertion::ValueRange {
                column: "amount".into(),
                min: Some(0.0),
                max: Some(1_000.0),
                exclusive: false,
            },
            AssertionStatus::Fail,
            2,
        ),
        (
            Assertion::ValueRange {
                column: "amount".into(),
                min: Some(-5.0),
                max: None,
                exclusive: false,
            },
            AssertionStatus::Pass,
            0,
        ),
        (
            Assertion::ValueRange {
                column: "amount".into(),
                min: Some(-5.0),
                max: None,
                exclusive: true,
            },
            AssertionStatus::Fail,
            1,
        ),
        (
            Assertion::AcceptedValues {
                column: "status".into(),
                values: vec!["paid".into(), "refunded".into(), "void".into()],
                case_insensitive: false,
                allow_null: true,
            },
            AssertionStatus::Fail,
            1,
        ),
        (
            Assertion::AcceptedValues {
                column: "status".into(),
                values: vec!["paid".into(), "refunded".into(), "void".into()],
                case_insensitive: true,
                allow_null: true,
            },
            AssertionStatus::Pass,
            0,
        ),
        (
            Assertion::Regex {
                column: "email".into(),
                pattern: r"[^@\s]+@[^@\s]+\.[a-z]+".into(),
                full_match: true,
                allow_null: true,
            },
            AssertionStatus::Fail,
            1,
        ),
        (
            Assertion::Regex {
                column: "email".into(),
                pattern: "@".into(),
                full_match: false,
                allow_null: false,
            },
            AssertionStatus::Fail,
            2,
        ),
        (
            Assertion::KpiThreshold {
                value: ValueSource::Aggregate {
                    column: "amount".into(),
                    aggregate: Aggregate::Sum,
                },
                op: CompareOp::Gt,
                threshold: 1_000.0,
            },
            AssertionStatus::Pass,
            0,
        ),
    ];
    for (assertion, expected, failing) in cases {
        let debug = format!("{assertion:?}");
        let outcome = check(assertion, &context);
        assert_eq!(outcome.status, expected, "{debug} {outcome:?}");
        assert_eq!(outcome.evidence.failing_count, failing, "{debug}");
    }
}

#[test]
fn failing_evidence_is_capped_and_names_rows_and_values() {
    let columns = vec![col("v", "int")];
    let rows: Vec<Vec<CellValue>> = (0..500).map(|n| vec![i(n % 30)]).collect();
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let outcome = check(
        Assertion::ValueRange {
            column: "v".into(),
            min: None,
            max: Some(9.0),
            exclusive: false,
        },
        &context,
    );
    assert_eq!(outcome.status, AssertionStatus::Fail);
    assert_eq!(outcome.evidence.failing_count, 330);
    assert_eq!(outcome.evidence.failing_rows.len(), FAILING_ROW_CAP);
    assert_eq!(outcome.evidence.failing_rows[0], 10);
    assert_eq!(outcome.evidence.failing_values.len(), FAILING_VALUE_CAP);
    assert_eq!(outcome.evidence.failing_values[0], "10");
    assert_eq!(
        outcome.evidence.observed["observed_max"],
        Metric::Number(29.0)
    );
}

#[test]
fn incomplete_inputs_are_inconclusive_unless_opted_in() {
    let (columns, rows) = orders();
    for completeness in [
        Completeness::Truncated { cap: 5 },
        Completeness::Sampled { n: 5 },
        Completeness::Partial,
    ] {
        let context = AssertionContext::new(OutputView::new(&columns, &rows, completeness));
        let spec = AssertionSpec::from(Assertion::Unique {
            columns: vec!["id".into()],
        });
        let outcome = evaluate(&spec, &context);
        assert_eq!(outcome.status, AssertionStatus::Inconclusive);
        assert_eq!(
            outcome.issue,
            Some(AssertionIssue::IncompleteInput { completeness })
        );
        assert!(outcome.evidence.partial);
        assert_eq!(outcome.evidence.failing_count, 1, "evidence still gathered");
        let opted = AssertionSpec {
            allow_incomplete: true,
            ..spec
        };
        assert_eq!(evaluate(&opted, &context).status, AssertionStatus::Fail);
        let passing = AssertionSpec::from(Assertion::RowCount {
            min: Some(1),
            max: None,
        });
        assert_eq!(
            evaluate(&passing, &context).status,
            AssertionStatus::Inconclusive,
            "no false pass either"
        );
    }
    let spilled = AssertionContext::new(OutputView::new(
        &columns,
        &rows,
        Completeness::Spilled { total: 5 },
    ));
    assert_eq!(
        check(
            Assertion::RowCount {
                min: Some(5),
                max: Some(5)
            },
            &spilled
        )
        .status,
        AssertionStatus::Pass
    );
    let mut duration =
        AssertionContext::new(OutputView::new(&columns, &rows, Completeness::Partial));
    duration.elapsed_ms = Some(10);
    assert_eq!(
        check(
            Assertion::Duration {
                max_ms: 100,
                warn_ms: None
            },
            &duration
        )
        .status,
        AssertionStatus::Pass
    );
}

#[test]
fn severity_warn_downgrades_failures() {
    let (columns, rows) = orders();
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let spec = AssertionSpec {
        severity: Severity::Warn,
        name: Some("ids unique".into()),
        ..AssertionSpec::from(Assertion::Unique {
            columns: vec!["id".into()],
        })
    };
    let outcome = evaluate(&spec, &context);
    assert_eq!(outcome.status, AssertionStatus::Warn);
    assert_eq!(outcome.name.as_deref(), Some("ids unique"));
}

#[test]
fn referential_compares_large_ids_exactly() {
    let columns = vec![col("customer_id", "bigint")];
    let rows = vec![vec![i(1234567890123456789)]];
    let parent_columns = vec![col("id", "bigint")];
    let parent_rows = vec![vec![i(1234567890123456768)]];
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "customers".to_string(),
        OutputView::complete(&parent_columns, &parent_rows),
    );
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    context.outputs = &outputs;
    let outcome = check(
        Assertion::Referential {
            column: "customer_id".into(),
            parent: "customers".into(),
            parent_column: "id".into(),
        },
        &context,
    );
    assert_eq!(outcome.status, AssertionStatus::Fail);
    assert_eq!(outcome.evidence.failing_values, vec!["1234567890123456789"]);
}

#[test]
fn referential_checks_child_values_against_a_parent_output() {
    let (columns, rows) = orders();
    let parent_columns = vec![col("id", "bigint")];
    let parent_rows = vec![vec![f(10.0)], vec![i(11)], vec![i(12)]];
    let mut outputs = BTreeMap::new();
    outputs.insert(
        "customers".to_string(),
        OutputView::complete(&parent_columns, &parent_rows),
    );
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    context.outputs = &outputs;
    let referential = || Assertion::Referential {
        column: "customer_id".into(),
        parent: "customers".into(),
        parent_column: "id".into(),
    };
    let outcome = check(referential(), &context);
    assert_eq!(outcome.status, AssertionStatus::Fail);
    assert_eq!(outcome.evidence.failing_values, vec!["99"]);
    assert_eq!(
        outcome.evidence.observed["orphan_values"],
        Metric::Integer(1)
    );

    let mut partial_parent = BTreeMap::new();
    partial_parent.insert(
        "customers".to_string(),
        OutputView::new(
            &parent_columns,
            &parent_rows,
            Completeness::Truncated { cap: 3 },
        ),
    );
    context.outputs = &partial_parent;
    let outcome = check(referential(), &context);
    assert_eq!(outcome.status, AssertionStatus::Inconclusive);
    assert!(matches!(
        outcome.issue,
        Some(AssertionIssue::IncompleteParent { .. })
    ));

    context.outputs = &NoOutputs;
    assert_eq!(
        check(referential(), &context).issue,
        Some(AssertionIssue::MissingOutput {
            name: "customers".into()
        })
    );
}

#[test]
fn drift_metrics_against_a_baseline_profile() {
    let columns = vec![col("v", "float"), col("cat", "text")];
    let base_rows: Vec<Vec<CellValue>> = (0..1_000)
        .map(|n| vec![f((n % 100) as f64), s(["a", "b", "c"][n % 3])])
        .collect();
    let same_rows = base_rows.clone();
    let shifted_rows: Vec<Vec<CellValue>> = (0..1_000)
        .map(|n| {
            vec![
                f((n % 100) as f64 + 80.0),
                s(if n % 10 == 0 { "a" } else { "d" }),
            ]
        })
        .collect();
    let baseline = profile_rows(
        &columns,
        &base_rows,
        Completeness::Complete,
        &ProfileOptions::default(),
    );
    let run = |rows: &[Vec<CellValue>], column: &str, metric: DriftMetric| {
        let mut context = AssertionContext::new(OutputView::complete(&columns, rows));
        context.baseline = Some(&baseline);
        check(
            Assertion::DistributionDrift {
                column: column.into(),
                metric,
                warn: None,
                fail: None,
            },
            &context,
        )
    };
    for metric in [DriftMetric::Psi, DriftMetric::Ks, DriftMetric::MeanShift] {
        assert_eq!(
            run(&same_rows, "v", metric).status,
            AssertionStatus::Pass,
            "{metric:?}"
        );
        let shifted = run(&shifted_rows, "v", metric);
        assert_eq!(
            shifted.status,
            AssertionStatus::Fail,
            "{metric:?} {shifted:?}"
        );
    }
    assert_eq!(
        run(&same_rows, "cat", DriftMetric::Psi).status,
        AssertionStatus::Pass
    );
    assert_eq!(
        run(&shifted_rows, "cat", DriftMetric::Psi).status,
        AssertionStatus::Fail
    );
    assert_eq!(
        run(&same_rows, "cat", DriftMetric::Ks).issue,
        Some(AssertionIssue::UnsupportedMetric)
    );

    let incomplete_baseline = profile_rows(
        &columns,
        &base_rows,
        Completeness::Sampled { n: 1_000 },
        &ProfileOptions::default(),
    );
    let mut context = AssertionContext::new(OutputView::complete(&columns, &shifted_rows));
    context.baseline = Some(&incomplete_baseline);
    let outcome = check(
        Assertion::DistributionDrift {
            column: "v".into(),
            metric: DriftMetric::Psi,
            warn: None,
            fail: None,
        },
        &context,
    );
    assert_eq!(outcome.status, AssertionStatus::Inconclusive);
    context.baseline = None;
    assert_eq!(
        check(
            Assertion::DistributionDrift {
                column: "v".into(),
                metric: DriftMetric::Psi,
                warn: None,
                fail: None
            },
            &context
        )
        .issue,
        Some(AssertionIssue::MissingBaseline)
    );
    assert!((psi(&[0.5, 0.5], &[0.5, 0.5], 1e-4)).abs() < 1e-12);
    assert!((ks(&[0.5, 0.5], &[1.0, 0.0]) - 0.5).abs() < 1e-12);
}

#[test]
fn freshness_uses_the_injected_clock() {
    let (columns, rows) = orders();
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let freshness = |max: u64, warn: Option<u64>| Assertion::Freshness {
        column: "created_at".into(),
        max_age_seconds: max,
        warn_age_seconds: warn,
        time_zone: None,
    };
    assert_eq!(
        check(freshness(3_600, None), &context).issue,
        Some(AssertionIssue::NoClock)
    );
    context.now = parse_timestamp("2024-05-04 15:00:00");
    let outcome = check(freshness(3 * 3_600, Some(3_600)), &context);
    assert_eq!(outcome.status, AssertionStatus::Warn);
    assert_eq!(
        outcome.evidence.observed["age_seconds"],
        Metric::Integer(7_200)
    );
    assert_eq!(
        check(freshness(3_600, None), &context).status,
        AssertionStatus::Fail
    );
    assert_eq!(
        check(freshness(86_400, None), &context).status,
        AssertionStatus::Pass
    );
    context.now = parse_timestamp("2024-05-01 00:00:00");
    let future = check(freshness(1, None), &context);
    assert_eq!(
        future.status,
        AssertionStatus::Warn,
        "a newest row in the future means a clock or zone is off"
    );
    assert_eq!(future.issue, Some(AssertionIssue::FutureTimestamp));
}

#[test]
fn freshness_reads_zone_less_times_in_the_given_zone() {
    let columns = vec![col("at", "timestamp")];
    let rows = vec![vec![s("2024-05-04 10:00:00")]];
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    context.now = parse_timestamp("2024-05-04 09:30:00Z");
    let freshness = |zone: Option<&str>| Assertion::Freshness {
        column: "at".into(),
        max_age_seconds: 7_200,
        warn_age_seconds: None,
        time_zone: zone.map(str::to_string),
    };
    let age = |zone: Option<&str>| check(freshness(zone), &context);
    assert_eq!(age(None).issue, Some(AssertionIssue::FutureTimestamp));
    let berlin = age(Some("Europe/Berlin"));
    assert_eq!(berlin.status, AssertionStatus::Pass);
    assert_eq!(
        berlin.evidence.observed["age_seconds"],
        Metric::Integer(5_400)
    );
    assert_eq!(
        age(Some("+02:00")).evidence.observed["age_seconds"],
        Metric::Integer(5_400)
    );
    assert_eq!(
        age(Some("Mars/Olympus")).issue,
        Some(AssertionIssue::InvalidTimeZone {
            zone: "Mars/Olympus".into()
        })
    );
    let rows = vec![vec![s("2024-05-04 10:00:00+00:00")]];
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    context.now = parse_timestamp("2024-05-04 10:30:00Z");
    assert_eq!(
        check(freshness(Some("Europe/Berlin")), &context)
            .evidence
            .observed["age_seconds"],
        Metric::Integer(1_800),
        "a written offset wins over the zone"
    );
}

#[test]
fn duration_needs_elapsed_time() {
    let (columns, rows) = orders();
    let mut context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let duration = Assertion::Duration {
        max_ms: 1_000,
        warn_ms: Some(500),
    };
    assert_eq!(
        check(duration.clone(), &context).issue,
        Some(AssertionIssue::NoDuration)
    );
    for (elapsed, expected) in [
        (100, AssertionStatus::Pass),
        (700, AssertionStatus::Warn),
        (1_001, AssertionStatus::Fail),
    ] {
        context.elapsed_ms = Some(elapsed);
        assert_eq!(
            check(duration.clone(), &context).status,
            expected,
            "{elapsed}"
        );
    }
}

#[test]
fn bad_specs_are_errors_not_failures() {
    let (columns, rows) = orders();
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    assert_eq!(
        check(
            Assertion::NotNull {
                column: "nope".into(),
                max_null_rate: 0.0
            },
            &context
        )
        .issue,
        Some(AssertionIssue::MissingColumn {
            column: "nope".into()
        })
    );
    let invalid = check(
        Assertion::Regex {
            column: "email".into(),
            pattern: "(".into(),
            full_match: true,
            allow_null: true,
        },
        &context,
    );
    assert_eq!(invalid.status, AssertionStatus::Error);
    assert!(matches!(
        invalid.issue,
        Some(AssertionIssue::InvalidPattern { .. })
    ));
    let huge = check(
        Assertion::Regex {
            column: "email".into(),
            pattern: "(a{1000}){1000}".into(),
            full_match: true,
            allow_null: true,
        },
        &context,
    );
    assert!(
        matches!(huge.issue, Some(AssertionIssue::InvalidPattern { .. })),
        "{huge:?}"
    );
    let long = check(
        Assertion::Regex {
            column: "email".into(),
            pattern: "a".repeat(MAX_PATTERN_CHARS + 1),
            full_match: true,
            allow_null: true,
        },
        &context,
    );
    assert_eq!(
        long.issue,
        Some(AssertionIssue::PatternTooLong {
            max_chars: MAX_PATTERN_CHARS
        })
    );
    let kpi = check(
        Assertion::KpiThreshold {
            value: ValueSource::Cell {
                column: Some("status".into()),
                row: 0,
            },
            op: CompareOp::Gt,
            threshold: 0.0,
        },
        &context,
    );
    assert!(matches!(kpi.issue, Some(AssertionIssue::Kpi { .. })));
}

#[test]
fn catastrophic_looking_patterns_match_in_linear_time() {
    let columns = vec![col("v", "text")];
    let rows = vec![vec![s(&format!("{}!", "a".repeat(50_000)))]];
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let outcome = check(
        Assertion::Regex {
            column: "v".into(),
            pattern: "(a+)+$".into(),
            full_match: true,
            allow_null: true,
        },
        &context,
    );
    assert_eq!(outcome.status, AssertionStatus::Fail);
}

#[test]
fn empty_and_unicode_inputs() {
    let columns = vec![col("name", "nvarchar")];
    let empty: Vec<Vec<CellValue>> = Vec::new();
    let context = AssertionContext::new(OutputView::complete(&columns, &empty));
    assert_eq!(
        check(
            Assertion::RowCount {
                min: Some(1),
                max: None
            },
            &context
        )
        .status,
        AssertionStatus::Fail
    );
    assert_eq!(
        check(
            Assertion::NotNull {
                column: "name".into(),
                max_null_rate: 0.0
            },
            &context
        )
        .status,
        AssertionStatus::Pass
    );
    assert_eq!(
        check(
            Assertion::Unique {
                columns: vec!["name".into()]
            },
            &context
        )
        .status,
        AssertionStatus::Pass
    );

    let rows = vec![vec![s("Ünïcødé")], vec![s("日本")], vec![s("ünïcødé")]];
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let accepted = |case_insensitive| Assertion::AcceptedValues {
        column: "name".into(),
        values: vec!["ünïcødé".into(), "日本".into()],
        case_insensitive,
        allow_null: false,
    };
    assert_eq!(
        check(accepted(false), &context).evidence.failing_values,
        vec!["Ünïcødé"]
    );
    assert_eq!(
        check(accepted(true), &context).status,
        AssertionStatus::Pass
    );
    assert_eq!(
        check(
            Assertion::Regex {
                column: "name".into(),
                pattern: r"\p{Han}+".into(),
                full_match: true,
                allow_null: false
            },
            &context
        )
        .evidence
        .failing_count,
        2
    );
}

#[test]
fn specs_round_trip_and_are_deterministic() {
    let json = r#"[
        {"kind":"row_count","min":1},
        {"kind":"unique","columns":["id"],"severity":"warn"},
        {"kind":"not_null","column":"amount","max_null_rate":0.1,"allow_incomplete":true},
        {"kind":"distribution_drift","column":"v","metric":"ks","fail":0.3},
        {"kind":"kpi_threshold","value":{"kind":"row_count"},"op":"ge","threshold":5}
    ]"#;
    let specs: Vec<AssertionSpec> = serde_json::from_str(json).expect("parses");
    assert_eq!(specs[1].severity, Severity::Warn);
    assert!(specs[2].allow_incomplete);
    let again: Vec<AssertionSpec> =
        serde_json::from_str(&serde_json::to_string(&specs).unwrap()).unwrap();
    assert_eq!(specs, again);
    let (columns, rows) = orders();
    let context = AssertionContext::new(OutputView::complete(&columns, &rows));
    let first = evaluate_all(&specs, &context);
    assert_eq!(first, evaluate_all(&specs, &context));
    assert_eq!(first[4].status, AssertionStatus::Pass);
    assert_eq!(
        serde_json::to_string(&first[0]).unwrap(),
        serde_json::to_string(&evaluate_all(&specs, &context)[0]).unwrap()
    );
    assert!(CompareOp::Eq.holds(0.1 + 0.2, 0.3));
    assert!(!CompareOp::Lt.holds(0.1 + 0.2, 0.3));
}
