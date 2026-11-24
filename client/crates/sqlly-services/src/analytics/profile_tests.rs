use sqlly_core::CellValue;

use super::*;
use crate::analytics::test_support::*;
use crate::analytics::{BatchedRows, Completeness, OutputView};

fn one_column(data_type: &str, values: Vec<CellValue>) -> ColumnProfile {
    let columns = vec![col("v", data_type)];
    let rows: Vec<Vec<CellValue>> = values.into_iter().map(|v| vec![v]).collect();
    profile_rows(
        &columns,
        &rows,
        Completeness::Complete,
        &ProfileOptions::default(),
    )
    .columns
    .remove(0)
}

fn json(profile: &ResultProfile) -> String {
    serde_json::to_string(profile).expect("profile serializes")
}

#[test]
fn numeric_column_basic_statistics_are_exact() {
    let mut values: Vec<CellValue> = (1..=10).map(i).collect();
    values.push(CellValue::Null);
    let p = one_column("int", values);
    assert_eq!(p.count, 11);
    assert_eq!(p.null_count, 1);
    assert!((p.null_rate - 1.0 / 11.0).abs() < 1e-12);
    assert_eq!(
        p.distinct,
        Estimate {
            value: 10,
            approximate: false
        }
    );
    assert_eq!(p.semantic_type, SemanticType::Integer);
    let n = p.numeric.expect("numeric stats");
    assert_eq!((n.count, n.min, n.max, n.sum), (10, 1.0, 10.0, 55.0));
    assert!((n.mean - 5.5).abs() < 1e-12);
    assert!((n.variance - 55.0 / 6.0).abs() < 1e-9);
    assert!((n.percentiles.p50 - 5.5).abs() < 1e-12);
    assert!((n.percentiles.p25 - 3.25).abs() < 1e-12);
    assert!((n.percentiles.p99 - 9.91).abs() < 1e-9);
    assert!(!n.percentiles.approximate);
    assert_eq!(n.histogram.counts.iter().sum::<u64>(), 10);
    assert_eq!(
        n.histogram.counts.len(),
        10,
        "unit bins for a small integer range"
    );
    assert_eq!(n.outlier_count, 0);
    assert_eq!(p.monotonicity, Monotonicity::StrictlyIncreasing);
    assert_eq!(
        p.candidate_key,
        KeyCandidate::NotUnique,
        "a NULL rules out a key"
    );
}

#[test]
fn nan_and_infinity_are_counted_apart_from_the_statistics() {
    let p = one_column(
        "float",
        vec![
            f(1.0),
            f(f64::NAN),
            f(f64::INFINITY),
            f(f64::NEG_INFINITY),
            f(3.0),
            f(-0.0),
        ],
    );
    let n = p.numeric.expect("numeric");
    assert_eq!(n.count, 3);
    assert_eq!(n.nan_count, 1);
    assert_eq!(n.positive_infinity_count, 1);
    assert_eq!(n.negative_infinity_count, 1);
    assert_eq!(n.zero_count, 1);
    assert!((n.mean - 4.0 / 3.0).abs() < 1e-12);
    assert_eq!((n.min, n.max), (0.0, 3.0));
    assert_eq!(p.monotonicity, Monotonicity::NotMonotonic);
}

#[test]
fn outliers_use_iqr_fences() {
    let mut values: Vec<CellValue> = (0..20).map(|n| f(10.0 + (n % 5) as f64)).collect();
    values.push(f(1_000.0));
    values.push(f(-500.0));
    let n = one_column("float", values).numeric.unwrap();
    assert_eq!(n.outlier_count, 2);
    assert!(n.outlier_upper_fence < 1_000.0 && n.outlier_lower_fence > -500.0);
}

#[test]
fn decimals_as_strings_profile_as_numbers() {
    let p = one_column(
        "decimal(38,10)",
        vec![s("1.5000000000"), s("2.25"), s("-3"), CellValue::Null],
    );
    assert_eq!(p.semantic_type, SemanticType::Decimal);
    let n = p.numeric.unwrap();
    assert_eq!((n.count, n.min, n.max), (3, -3.0, 2.25));
    assert_eq!(n.negative_count, 1);
    assert!(!n.all_integral);
}

#[test]
fn unicode_text_lengths_count_characters() {
    let p = one_column(
        "nvarchar(50)",
        vec![
            s("日本語"),
            s("é"),
            s(""),
            s("   "),
            s(" padded "),
            s("🦀🦀"),
        ],
    );
    let t = p.text.expect("text stats");
    assert_eq!(t.count, 6);
    assert_eq!(t.min_length, 0);
    assert_eq!(t.max_length, 8);
    assert_eq!(t.empty_count, 1);
    assert_eq!(t.whitespace_only_count, 1);
    assert_eq!(t.padded_count, 1);
    assert!((t.mean_length - (3.0 + 1.0 + 0.0 + 3.0 + 8.0 + 2.0) / 6.0).abs() < 1e-12);
    assert!(p.numeric.is_none());
}

#[test]
fn semantic_types_are_inferred_from_values() {
    let repeat = |text: &str, n: usize| {
        (0..n)
            .map(|k| s(&text.replace('#', &k.to_string())))
            .collect::<Vec<_>>()
    };
    let cases: Vec<(&str, &str, Vec<CellValue>, SemanticType)> = vec![
        ("v", "", repeat("#", 20), SemanticType::Integer),
        ("v", "", repeat("#.5", 20), SemanticType::Decimal),
        (
            "v",
            "",
            repeat("user#@example.com", 20),
            SemanticType::Email,
        ),
        (
            "v",
            "",
            repeat("https://sqlly.app/#", 20),
            SemanticType::Url,
        ),
        ("v", "", repeat("10.0.0.#", 20), SemanticType::Ip),
        ("v", "", repeat("{\"k\": #}", 20), SemanticType::Json),
        ("v", "", repeat("$#.00", 20), SemanticType::CurrencyLike),
        ("v", "", repeat("2024-01-1#", 9), SemanticType::Date),
        (
            "v",
            "",
            repeat("2024-01-01 10:0#:00", 9),
            SemanticType::DateTime,
        ),
        ("v", "", repeat("10:0#:00", 9), SemanticType::Time),
        (
            "v",
            "",
            (0..20)
                .map(|k| s(&format!("6f9619ff-8b86-d011-b42d-00c04fc964{k:02}")))
                .collect(),
            SemanticType::Uuid,
        ),
        (
            "v",
            "",
            (0..20)
                .map(|k| s(if k % 2 == 0 { "yes" } else { "no" }))
                .collect(),
            SemanticType::Boolean,
        ),
        (
            "v",
            "",
            (0..40)
                .map(|k| s(["red", "green", "blue"][k % 3]))
                .collect(),
            SemanticType::Categorical,
        ),
        (
            "v",
            "",
            repeat("this is a fairly long note number # about the order", 20),
            SemanticType::FreeText,
        ),
        ("v", "", repeat("code-#", 20), SemanticType::Text),
        (
            "customer_id",
            "int",
            (0..20).map(i).collect(),
            SemanticType::IdLike,
        ),
        ("OrderId", "", repeat("A#", 20), SemanticType::IdLike),
        (
            "v",
            "",
            vec![CellValue::Null, s(""), s("  ")],
            SemanticType::Empty,
        ),
        (
            "v",
            "varbinary",
            vec![CellValue::Bytes(vec![1]), CellValue::Bytes(vec![2])],
            SemanticType::Binary,
        ),
        (
            "v",
            "money",
            vec![f(1.5), f(2.5)],
            SemanticType::CurrencyLike,
        ),
        (
            "v",
            "bit",
            vec![CellValue::Bool(true), CellValue::Bool(false)],
            SemanticType::Boolean,
        ),
    ];
    for (name, data_type, values, expected) in cases {
        let columns = vec![col(name, data_type)];
        let rows: Vec<Vec<CellValue>> = values.into_iter().map(|v| vec![v]).collect();
        let profile = profile_rows(
            &columns,
            &rows,
            Completeness::Complete,
            &ProfileOptions::default(),
        );
        assert_eq!(
            profile.columns[0].semantic_type,
            expected,
            "{name} {data_type} {:?}",
            rows.first()
        );
    }
}

#[test]
fn boolean_and_date_details_are_reported() {
    let p = one_column(
        "bit",
        vec![CellValue::Bool(true), CellValue::Bool(true), i(0)],
    );
    assert_eq!(
        p.boolean,
        Some(BooleanStats {
            true_count: 2,
            false_count: 1
        })
    );

    let dates: Vec<CellValue> = (0..90)
        .map(|day| {
            let date =
                chrono::NaiveDate::from_ymd_opt(2024, 1, 1).unwrap() + chrono::Duration::days(day);
            s(&date.format("%Y-%m-%d").to_string())
        })
        .collect();
    let p = one_column("date", dates);
    let t = p.temporal.expect("temporal stats");
    assert_eq!(t.min, "2024-01-01");
    assert_eq!(t.max, "2024-03-30");
    assert!(!t.has_time);
    assert_eq!(t.grain, crate::analytics::time::TimeGrain::Week);
    assert_eq!(t.buckets.iter().map(|b| b.count).sum::<u64>(), 90);
    assert_eq!(t.buckets[0].start, "2024-01-01");
    assert_eq!(t.buckets[0].count, 7);
    assert_eq!(p.monotonicity, Monotonicity::StrictlyIncreasing);
}

#[test]
fn empty_inputs_profile_cleanly() {
    let columns = vec![col("a", "int"), col("b", "")];
    let profile = profile_rows(
        &columns,
        &[],
        Completeness::Complete,
        &ProfileOptions::default(),
    );
    assert_eq!(profile.row_count, 0);
    for column in &profile.columns {
        assert_eq!(column.count, 0);
        assert_eq!(column.null_rate, 0.0);
        assert_eq!(column.semantic_type, SemanticType::Empty);
        assert_eq!(column.monotonicity, Monotonicity::Unknown);
        assert!(column.numeric.is_none());
        assert!(column.top_values.is_empty());
    }
    assert!(!json(&profile).is_empty());

    // Short rows read as NULL for the missing cells.
    let rows = vec![vec![i(1)]];
    let profile = profile_rows(
        &columns,
        &rows,
        Completeness::Complete,
        &ProfileOptions::default(),
    );
    assert_eq!(profile.columns[1].null_count, 1);
}

#[test]
fn candidate_keys_respect_completeness() {
    let columns = vec![col("id", "int")];
    let rows: Vec<Vec<CellValue>> = (0..50).map(|n| vec![i(n)]).collect();
    let options = ProfileOptions::default();
    let complete = profile_rows(&columns, &rows, Completeness::Complete, &options);
    assert_eq!(complete.columns[0].candidate_key, KeyCandidate::Unique);
    let truncated = profile_rows(
        &columns,
        &rows,
        Completeness::Truncated { cap: 50 },
        &options,
    );
    assert_eq!(
        truncated.columns[0].candidate_key,
        KeyCandidate::LikelyUnique
    );
    assert_eq!(truncated.completeness, Completeness::Truncated { cap: 50 });
    let mut duplicated = rows.clone();
    duplicated.push(vec![i(3)]);
    let dup = profile_rows(&columns, &duplicated, Completeness::Complete, &options);
    assert_eq!(dup.columns[0].candidate_key, KeyCandidate::NotUnique);
    assert_eq!(
        dup.columns[0].top_values[0],
        ValueCount {
            value: "3".into(),
            count: 2,
            error: 0
        }
    );
}

#[test]
fn monotonicity_table() {
    let cases: Vec<(Vec<CellValue>, Monotonicity)> = vec![
        (vec![i(1), i(2), i(2), i(3)], Monotonicity::Increasing),
        (vec![i(3), i(2), i(1)], Monotonicity::StrictlyDecreasing),
        (vec![i(3), i(3), i(1)], Monotonicity::Decreasing),
        (vec![i(2), i(2)], Monotonicity::Constant),
        (vec![i(1), i(3), i(2)], Monotonicity::NotMonotonic),
        (
            vec![i(1), CellValue::Null, i(2)],
            Monotonicity::StrictlyIncreasing,
        ),
        (
            vec![s("a"), s("b"), s("c")],
            Monotonicity::StrictlyIncreasing,
        ),
        (
            vec![s("2024-01-02"), s("2024-01-01")],
            Monotonicity::StrictlyDecreasing,
        ),
        (vec![i(1)], Monotonicity::Unknown),
    ];
    for (values, expected) in cases {
        let debug = format!("{values:?}");
        assert_eq!(one_column("", values).monotonicity, expected, "{debug}");
    }
}

fn big_rows(n: u64) -> Vec<Vec<CellValue>> {
    (0..n)
        .map(|k| {
            let scrambled = crate::analytics::value::splitmix64(k) % n;
            vec![
                i(scrambled as i64),
                s(&format!("2020-01-01 00:00:{:02}", k % 60)),
            ]
        })
        .collect()
}

#[test]
fn huge_cardinality_switches_to_sketches_within_documented_error() {
    let n = 200_000u64;
    let columns = vec![col("v", "bigint"), col("t", "datetime")];
    let rows = big_rows(n);
    let options = ProfileOptions {
        exact_distinct_cap: 1_000,
        exact_quantile_cap: 1_000,
        ..ProfileOptions::default()
    };
    let profile = profile_rows(&columns, &rows, Completeness::Complete, &options);
    let v = &profile.columns[0];
    assert!(v.distinct.approximate);
    let true_distinct = rows
        .iter()
        .map(|row| match row[0] {
            CellValue::I64(n) => n,
            _ => unreachable!(),
        })
        .collect::<std::collections::HashSet<_>>()
        .len() as f64;
    let error = (v.distinct.value as f64 - true_distinct).abs() / true_distinct;
    assert!(
        error < 0.05,
        "distinct {} vs {true_distinct}",
        v.distinct.value
    );
    assert!(v.top_values_approximate);
    let numeric = v.numeric.as_ref().unwrap();
    assert!(numeric.percentiles.approximate);
    for (q, value) in [
        (0.25, numeric.percentiles.p25),
        (0.5, numeric.percentiles.p50),
        (0.95, numeric.percentiles.p95),
    ] {
        assert!((value / n as f64 - q).abs() < 0.02, "q={q} value={value}");
    }
    assert_eq!(numeric.histogram.counts.iter().sum::<u64>(), numeric.count);
    assert!(numeric.histogram.approximate);
    // Exact figures stay exact.
    assert_eq!(numeric.count, n);
    let t = profile.columns[1].temporal.as_ref().unwrap();
    assert!(t.buckets_approximate);
    assert_eq!(t.buckets.iter().map(|b| b.count).sum::<u64>(), n);
}

#[test]
fn heavy_hitters_survive_the_switch_to_space_saving() {
    let columns = vec![col("v", "varchar")];
    let rows: Vec<Vec<CellValue>> = (0..50_000u64)
        .map(|k| {
            if k % 3 == 0 {
                vec![s("hot")]
            } else {
                vec![s(&format!("cold-{k}"))]
            }
        })
        .collect();
    let options = ProfileOptions {
        exact_distinct_cap: 100,
        heavy_hitter_capacity: 64,
        ..ProfileOptions::default()
    };
    let profile = profile_rows(&columns, &rows, Completeness::Complete, &options);
    let top = &profile.columns[0].top_values[0];
    assert_eq!(top.value, "hot");
    let truth = 50_000u64.div_ceil(3);
    assert!(top.count >= truth && top.count - top.error <= truth);
}

#[test]
fn streaming_batches_equal_one_pass_and_runs_are_deterministic() {
    let columns = vec![col("v", "bigint"), col("t", "datetime")];
    let rows = big_rows(30_000);
    for options in [
        ProfileOptions::default(),
        ProfileOptions {
            exact_distinct_cap: 500,
            exact_quantile_cap: 700,
            ..ProfileOptions::default()
        },
    ] {
        let whole = profile_rows(&columns, &rows, Completeness::Complete, &options);
        for batch_size in [1usize, 7, 1_000, 29_999] {
            let batches: Vec<Vec<Vec<CellValue>>> = rows
                .chunks(batch_size)
                .map(|chunk| chunk.to_vec())
                .collect();
            let source = BatchedRows(&batches);
            let view = OutputView::from_source(&columns, &source, Completeness::Complete);
            let streamed = profile_output(&view, &options);
            assert_eq!(json(&whole), json(&streamed), "batch size {batch_size}");
        }
        let again = profile_rows(&columns, &rows, Completeness::Complete, &options);
        assert_eq!(json(&whole), json(&again));
    }
}

#[test]
fn profiles_round_trip_through_serde() {
    let columns = vec![col("v", "int"), col("name", "text")];
    let rows: Vec<Vec<CellValue>> = (0..100)
        .map(|k| vec![i(k % 7), s(&format!("n{k}"))])
        .collect();
    let profile = profile_rows(
        &columns,
        &rows,
        Completeness::Sampled { n: 100 },
        &ProfileOptions::default(),
    );
    let text = json(&profile);
    let back: ResultProfile = serde_json::from_str(&text).expect("deserializes");
    assert_eq!(json(&back), text);
    assert_eq!(back.column("V").map(|c| c.name.as_str()), Some("v"));
}

#[test]
fn a_sketched_distinct_count_never_drops_below_the_exact_cap() {
    let cap = ProfileOptions::default().exact_distinct_cap as i64;
    for extra in [1, 7, 60] {
        let values: Vec<CellValue> = (0..cap + extra).map(i).collect();
        let column = one_column("int", values);
        assert!(column.distinct.approximate);
        assert!(
            column.distinct.value >= (cap + 1) as u64
                && column.distinct.value <= (cap + extra) as u64,
            "{extra}: {}",
            column.distinct.value
        );
    }
}
