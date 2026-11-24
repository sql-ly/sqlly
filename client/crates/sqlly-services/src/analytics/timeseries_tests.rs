use std::collections::BTreeMap;

use sqlly_core::{CellValue, ResultColumn};

use super::*;
use crate::analytics::test_support::*;
use crate::analytics::{BatchedRows, Completeness, OutputView};

fn daily(values: &[f64]) -> Vec<SeriesPoint> {
    let start = super::super::time::parse_timestamp("2024-01-01").unwrap();
    values
        .iter()
        .enumerate()
        .map(|(day, value)| {
            let at = start + chrono::Duration::days(day as i64);
            SeriesPoint {
                t: epoch_seconds(at),
                label: at.format("%Y-%m-%d").to_string(),
                value: *value,
            }
        })
        .collect()
}

#[test]
fn series_buckets_fill_gaps_for_sums_and_skip_bad_times() {
    let columns = vec![col("at", "datetime"), col("v", "int")];
    let rows = vec![
        vec![s("2024-01-01 10:00:00"), i(1)],
        vec![s("2024-01-01 23:59:59"), i(2)],
        vec![s("2024-01-03 00:00:00"), i(4)],
        vec![CellValue::Null, i(100)],
        vec![s("not a date"), i(100)],
    ];
    let view = OutputView::complete(&columns, &rows);
    let sum = time_series(
        &view,
        "at",
        Some("v"),
        Aggregate::Sum,
        Some(TimeGrain::Day),
        100,
    )
    .unwrap();
    assert_eq!(
        sum.points.iter().map(|p| p.value).collect::<Vec<_>>(),
        vec![3.0, 0.0, 4.0]
    );
    assert_eq!(sum.points[1].label, "2024-01-02");
    assert_eq!(sum.skipped_rows, 2);
    let avg = time_series(
        &view,
        "at",
        Some("v"),
        Aggregate::Avg,
        Some(TimeGrain::Day),
        100,
    )
    .unwrap();
    assert_eq!(
        avg.points.iter().map(|p| p.value).collect::<Vec<_>>(),
        vec![1.5, 4.0]
    );
    let auto = time_series(&view, "at", None, Aggregate::Sum, None, 60).unwrap();
    assert_eq!(auto.grain, TimeGrain::Hour);
    assert_eq!(auto.points.iter().map(|p| p.value).sum::<f64>(), 3.0);
    let capped = time_series(
        &view,
        "at",
        Some("v"),
        Aggregate::Sum,
        Some(TimeGrain::Day),
        2,
    )
    .unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.points[0].label, "2024-01-02");
    assert_eq!(
        time_series(&view, "missing", None, Aggregate::Sum, None, 10).unwrap_err(),
        TimeSeriesError::MissingColumn {
            column: "missing".into()
        }
    );
    let empty: Vec<Vec<CellValue>> = Vec::new();
    assert_eq!(
        time_series(
            &OutputView::complete(&columns, &empty),
            "at",
            None,
            Aggregate::Sum,
            None,
            10
        )
        .unwrap_err(),
        TimeSeriesError::NoTimestamps
    );
}

#[test]
fn anomaly_bands_flag_spikes_against_prior_points_only() {
    let mut values = vec![10.0, 11.0, 9.0, 10.0, 10.5, 9.5, 10.0];
    values.push(30.0);
    values.push(10.0);
    let bands = anomaly_bands(&daily(&values), &AnomalyOptions::default());
    assert!(bands[..3].iter().all(|b| b.expected.is_none()));
    assert_eq!(bands[7].anomaly, Some(AnomalyDirection::Above));
    assert!(bands[7].z_score.unwrap() > 3.0);
    assert_eq!(
        bands[8].anomaly, None,
        "the spike widens later bands, not its own"
    );
    assert!(bands[3..7].iter().all(|b| b.anomaly.is_none()));
    let drop = anomaly_bands(
        &daily(&[5.0, 5.1, 4.9, 5.0, -10.0]),
        &AnomalyOptions::default(),
    );
    assert_eq!(drop[4].anomaly, Some(AnomalyDirection::Below));
    let nan = anomaly_bands(
        &daily(&[1.0, f64::NAN, 1.0, 1.0, 1.0]),
        &AnomalyOptions::default(),
    );
    assert_eq!(nan[1].expected, None);
}

#[test]
fn seasonal_bands_compare_the_same_weekday() {
    // Mondays (2024-01-01 is a Monday) run at 100, other days at 10.
    let values: Vec<f64> = (0..35)
        .map(|day| {
            if day % 7 == 0 {
                100.0 + (day % 3) as f64
            } else {
                10.0 + (day % 2) as f64
            }
        })
        .collect();
    let options = AnomalyOptions {
        window: 6,
        ..AnomalyOptions::default()
    };
    let plain = anomaly_bands(&daily(&values), &options);
    let seasonal = anomaly_bands(
        &daily(&values),
        &AnomalyOptions {
            seasonality: Seasonality::SameWeekday,
            ..options
        },
    );
    assert_eq!(
        plain[28].anomaly,
        Some(AnomalyDirection::Above),
        "a flat baseline flags every Monday"
    );
    assert_eq!(seasonal[28].anomaly, None);
    assert!(seasonal[28].expected.unwrap() > 99.0);
}

#[test]
fn contribution_finds_the_dimension_that_explains_the_change() {
    let columns = vec![
        col("month", "text"),
        col("region", "text"),
        col("product", "text"),
        col("revenue", "int"),
    ];
    let rows = vec![
        vec![s("2024-01"), s("east"), s("a"), i(100)],
        vec![s("2024-01"), s("west"), s("a"), i(100)],
        vec![s("2024-01"), s("east"), s("b"), i(100)],
        vec![s("2024-01"), s("west"), s("b"), i(100)],
        vec![s("2024-02"), s("east"), s("a"), i(100)],
        vec![s("2024-02"), s("west"), s("a"), i(20)],
        vec![s("2024-02"), s("east"), s("b"), i(100)],
        vec![s("2024-02"), s("west"), s("b"), i(20)],
        vec![s("2023-12"), s("west"), s("b"), i(9_999)],
    ];
    let view = OutputView::complete(&columns, &rows);
    let spec = ContributionSpec {
        period_column: "month".into(),
        before: "2024-01".into(),
        after: "2024-02".into(),
        dimensions: vec!["product".into(), "region".into()],
        measure: Some("revenue".into()),
        aggregate: Aggregate::Sum,
        max_items: 10,
    };
    let result = contribution_by_dimensions(&view, &spec).unwrap();
    assert_eq!(result[0].dimension, "region");
    assert_eq!(result[0].delta, -160.0);
    assert_eq!(result[0].items[0].key, "west");
    assert_eq!(result[0].items[0].share_of_change, Some(1.0));
    assert_eq!(result[0].concentration, 1.0);
    assert_eq!(result[1].dimension, "product");
    assert!((result[1].concentration - 0.5).abs() < 1e-12);
}

#[test]
fn decompose_folds_the_tail_and_handles_new_and_gone_values() {
    let before = BTreeMap::from([
        ("a".to_string(), 10.0),
        ("b".to_string(), 5.0),
        ("c".to_string(), 1.0),
    ]);
    let after = BTreeMap::from([
        ("a".to_string(), 4.0),
        ("d".to_string(), 3.0),
        ("c".to_string(), 1.5),
    ]);
    let result = decompose("k", &before, &after, 3);
    assert_eq!(result.items[0].key, "a");
    assert_eq!(result.items[1].key, "b");
    assert_eq!(result.items[2].key, "(other)");
    assert_eq!(result.folded_items, 2);
    assert!((result.items[2].delta - 3.5).abs() < 1e-12);
    let unchanged = decompose("k", &before, &before, 10);
    assert!(
        unchanged
            .items
            .iter()
            .all(|item| item.share_of_change.is_none())
    );
    assert_eq!(unchanged.concentration, 0.0);
}

#[test]
fn pareto_ranks_and_counts_the_vital_few() {
    let result = pareto(
        vec![
            ("c".to_string(), 10.0),
            ("a".to_string(), 60.0),
            ("b".to_string(), 25.0),
            ("d".to_string(), 5.0),
            ("neg".to_string(), -3.0),
            ("nan".to_string(), f64::NAN),
        ],
        0.8,
    );
    assert_eq!(
        result
            .items
            .iter()
            .map(|i| i.key.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "b", "c", "d"]
    );
    assert_eq!(result.vital_few, 2);
    assert!((result.items[1].cumulative_share - 0.85).abs() < 1e-12);
    assert_eq!(result.excluded, 2);
    assert_eq!(pareto(Vec::new(), 0.8).vital_few, 0);

    let columns = vec![col("cat", "text")];
    let rows = vec![
        vec![s("x")],
        vec![s("x")],
        vec![s("y")],
        vec![CellValue::Null],
    ];
    let by = pareto_by(
        &OutputView::complete(&columns, &rows),
        "cat",
        None,
        Aggregate::Sum,
        0.5,
    )
    .unwrap();
    assert_eq!(by.items[0].key, "x");
    assert_eq!(by.items[0].value, 2.0);
    assert_eq!(by.total, 4.0);
}

#[test]
fn cohort_retention_matrix() {
    let columns = vec![col("user", "int"), col("active_on", "date")];
    let rows = vec![
        vec![i(1), s("2024-01-05")],
        vec![i(1), s("2024-02-10")],
        vec![i(1), s("2024-03-01")],
        vec![i(2), s("2024-01-20")],
        vec![i(2), s("2024-03-15")],
        vec![i(3), s("2024-02-02")],
        vec![i(3), s("2024-02-28")],
        vec![i(3), s("2024-03-30")],
        vec![CellValue::Null, s("2024-01-01")],
    ];
    let view = OutputView::complete(&columns, &rows);
    let spec = CohortSpec {
        entity_column: "user".into(),
        activity_column: "active_on".into(),
        cohort_column: None,
        grain: TimeGrain::Month,
        max_cohorts: 36,
        max_periods: 36,
    };
    let matrix = cohort_retention(&view, &spec).unwrap();
    assert_eq!(matrix.periods, 3);
    assert_eq!(matrix.rows.len(), 2);
    assert_eq!(matrix.rows[0].cohort, "2024-01");
    assert_eq!(matrix.rows[0].size, 2);
    assert_eq!(matrix.rows[0].retained, vec![2, 1, 2]);
    assert_eq!(matrix.rows[0].rates, vec![1.0, 0.5, 1.0]);
    assert_eq!(
        matrix.rows[1].retained,
        vec![1, 1],
        "April has not happened in this data, so it is not 0%"
    );
    let capped = cohort_retention(
        &view,
        &CohortSpec {
            max_cohorts: 1,
            max_periods: 2,
            ..spec.clone()
        },
    )
    .unwrap();
    assert!(capped.truncated);
    assert_eq!(capped.rows.len(), 1);
    assert_eq!(capped.rows[0].cohort, "2024-02");

    let with_signup = vec![
        col("user", "int"),
        col("active_on", "date"),
        col("signed_up", "date"),
    ];
    let rows = vec![vec![i(1), s("2024-03-01"), s("2024-01-15")]];
    let matrix = cohort_retention(
        &OutputView::complete(&with_signup, &rows),
        &CohortSpec {
            cohort_column: Some("signed_up".into()),
            ..spec
        },
    )
    .unwrap();
    assert_eq!(matrix.rows[0].cohort, "2024-01");
    assert_eq!(matrix.rows[0].retained, vec![0, 0, 1]);
}

#[test]
fn correlation_matrix_is_symmetric_and_pairwise() {
    let columns: Vec<ResultColumn> = vec![
        col("x", "float"),
        col("y", "float"),
        col("z", ""),
        col("label", "text"),
    ];
    let rows: Vec<Vec<CellValue>> = (0..20)
        .map(|n| {
            let x = n as f64;
            vec![f(x), f(2.0 * x + 1.0), s(&format!("{}", 100.0 - x)), s("t")]
        })
        .chain(std::iter::once(vec![
            f(f64::NAN),
            CellValue::Null,
            s("1"),
            s("t"),
        ]))
        .collect();
    let matrix = correlation_matrix(&OutputView::complete(&columns, &rows), None).unwrap();
    assert_eq!(matrix.columns, vec!["x", "y", "z"]);
    assert!((matrix.values[0][1].unwrap() - 1.0).abs() < 1e-12);
    assert!((matrix.values[0][2].unwrap() + 1.0).abs() < 1e-12);
    assert_eq!(matrix.values[1][0], matrix.values[0][1]);
    assert_eq!(matrix.values[0][0], Some(1.0));
    assert_eq!(matrix.counts[0][1], 20);
    assert_eq!(matrix.counts[2][2], 21);
    let constant = vec![
        vec![f(1.0), f(2.0)],
        vec![f(1.0), f(3.0)],
        vec![f(1.0), f(4.0)],
    ];
    let m = correlation_matrix(&OutputView::complete(&columns[..2], &constant), None).unwrap();
    assert_eq!(m.values[0][1], None);
}

#[test]
fn linear_trend_and_naive_forecast() {
    let points = daily(&[10.0, 12.0, 14.0, 16.0, 18.0]);
    let fit = linear_trend(&points).unwrap();
    assert!((fit.slope * 86_400.0 - 2.0).abs() < 1e-9);
    assert!((fit.r_squared - 1.0).abs() < 1e-12);
    assert_eq!(fit.method, "naive_linear");
    let series = TimeSeries {
        grain: TimeGrain::Day,
        points,
        skipped_rows: 0,
        truncated: false,
    };
    let forecast = forecast(&series, 2, 0.95).unwrap();
    assert_eq!(forecast.points[0].label, "2024-01-06");
    assert!((forecast.points[0].value - 20.0).abs() < 1e-9);
    assert!((forecast.points[1].value - 22.0).abs() < 1e-9);
    assert!(
        forecast.points[0].upper - forecast.points[0].lower < 1e-6,
        "a perfect fit has no spread"
    );

    let noisy = daily(&[10.0, 13.0, 11.0, 15.0, 14.0, 17.0, 15.0, 19.0]);
    let series = TimeSeries {
        grain: TimeGrain::Day,
        points: noisy,
        skipped_rows: 0,
        truncated: false,
    };
    let f = super::forecast(&series, 3, 0.95).unwrap();
    let widths: Vec<f64> = f.points.iter().map(|p| p.upper - p.lower).collect();
    assert!(
        widths[0] > 0.0 && widths[0] < widths[1] && widths[1] < widths[2],
        "{widths:?}"
    );
    assert!(linear_trend(&daily(&[1.0])).is_none());
    // t critical values against tables.
    assert!((t_critical(10, 0.95) - 2.228).abs() < 0.02);
    assert!((t_critical(30, 0.95) - 2.042).abs() < 0.01);
    assert!((normal_quantile(0.975) - 1.959_964).abs() < 1e-6);
}

#[test]
fn streaming_sources_give_the_same_series() {
    let columns = vec![col("at", "date"), col("v", "int")];
    let rows: Vec<Vec<CellValue>> = (0..400)
        .map(|n| {
            let day = chrono::NaiveDate::from_ymd_opt(2023, 1, 1).unwrap()
                + chrono::Duration::days(n % 200);
            vec![s(&day.format("%Y-%m-%d").to_string()), i(n)]
        })
        .collect();
    let batches: Vec<Vec<Vec<CellValue>>> = rows.chunks(33).map(|c| c.to_vec()).collect();
    let source = BatchedRows(&batches);
    let whole = time_series(
        &OutputView::complete(&columns, &rows),
        "at",
        Some("v"),
        Aggregate::Sum,
        None,
        60,
    )
    .unwrap();
    let streamed = time_series(
        &OutputView::from_source(&columns, &source, Completeness::Complete),
        "at",
        Some("v"),
        Aggregate::Sum,
        None,
        60,
    )
    .unwrap();
    assert_eq!(whole, streamed);
    assert_eq!(whole.grain, TimeGrain::Week);
}
