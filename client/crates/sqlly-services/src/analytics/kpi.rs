//! KPI math: one value from an output (a cell or a column aggregate),
//! compared with the previous period, a baseline output or a target, with
//! direction-aware status, thresholds, a sparkline series and a formatting
//! hint.

use std::collections::HashSet;
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::time::{TimeGrain, bucket_label, floor_to_grain, previous_bucket};
use super::timeseries::{SeriesPoint, TimeSeries, TimeSeriesError, time_series};
use super::value::{DeclaredClass, cell_hash, declared_class, finite_number};
use super::{Completeness, OutputView};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    Sum,
    Avg,
    Min,
    Max,
    /// Every row, NULL or not.
    Count,
    CountNonNull,
    CountDistinct,
    Median,
    First,
    Last,
}

impl Aggregate {
    /// Whether an empty group is 0 rather than unknown.
    pub fn empty_is_zero(self) -> bool {
        matches!(
            self,
            Aggregate::Sum | Aggregate::Count | Aggregate::CountNonNull | Aggregate::CountDistinct
        )
    }

    pub fn is_count(self) -> bool {
        matches!(
            self,
            Aggregate::Count | Aggregate::CountNonNull | Aggregate::CountDistinct
        )
    }
}

/// Folds cells into one aggregate. Non-numeric cells are skipped by the
/// numeric aggregates (NaN and infinities too), as SQL skips NULLs.
#[derive(Debug, Clone)]
pub struct AggregateAccumulator {
    aggregate: Aggregate,
    rows: u64,
    non_null: u64,
    numeric: u64,
    sum: f64,
    min: f64,
    max: f64,
    first: Option<f64>,
    last: Option<f64>,
    values: Vec<f64>,
    distinct: HashSet<u64>,
}

impl AggregateAccumulator {
    pub fn new(aggregate: Aggregate) -> Self {
        Self {
            aggregate,
            rows: 0,
            non_null: 0,
            numeric: 0,
            sum: 0.0,
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
            first: None,
            last: None,
            values: Vec::new(),
            distinct: HashSet::new(),
        }
    }

    pub fn push(&mut self, cell: &CellValue) {
        self.rows += 1;
        if matches!(cell, CellValue::Null) {
            return;
        }
        self.non_null += 1;
        match self.aggregate {
            Aggregate::Count | Aggregate::CountNonNull => return,
            Aggregate::CountDistinct => {
                self.distinct.insert(cell_hash(cell, 0));
                return;
            }
            _ => {}
        }
        let Some(value) = finite_number(cell) else {
            return;
        };
        self.numeric += 1;
        self.sum += value;
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.first.get_or_insert(value);
        self.last = Some(value);
        if self.aggregate == Aggregate::Median {
            self.values.push(value);
        }
    }

    /// The aggregate, `None` when it is undefined (no numeric values).
    pub fn finish(&self) -> Option<f64> {
        let has_numbers = self.numeric > 0;
        match self.aggregate {
            Aggregate::Count => Some(self.rows as f64),
            Aggregate::CountNonNull => Some(self.non_null as f64),
            Aggregate::CountDistinct => Some(self.distinct.len() as f64),
            Aggregate::Sum => Some(self.sum),
            Aggregate::Avg => has_numbers.then(|| self.sum / self.numeric as f64),
            Aggregate::Min => has_numbers.then_some(self.min),
            Aggregate::Max => has_numbers.then_some(self.max),
            Aggregate::First => self.first,
            Aggregate::Last => self.last,
            Aggregate::Median => {
                if self.values.is_empty() {
                    return None;
                }
                let mut sorted = self.values.clone();
                sorted.sort_by(f64::total_cmp);
                let mid = sorted.len() / 2;
                Some(if sorted.len().is_multiple_of(2) {
                    (sorted[mid - 1] + sorted[mid]) / 2.0
                } else {
                    sorted[mid]
                })
            }
        }
    }
}

/// Where a KPI's value comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ValueSource {
    /// One cell: `column` (the first column when omitted) of 0-based `row`.
    Cell {
        #[serde(default)]
        column: Option<String>,
        #[serde(default)]
        row: usize,
    },
    Aggregate {
        column: String,
        aggregate: Aggregate,
    },
    RowCount,
}

impl ValueSource {
    fn column(&self) -> Option<&str> {
        match self {
            ValueSource::Cell { column, .. } => column.as_deref(),
            ValueSource::Aggregate { column, .. } => Some(column),
            ValueSource::RowCount => None,
        }
    }

    /// Whether a truncated or sampled input changes the value.
    pub fn depends_on_every_row(&self) -> bool {
        !matches!(self, ValueSource::Cell { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum KpiError {
    MissingColumn {
        column: String,
    },
    /// The output has no column to read.
    NoColumns,
    RowOutOfRange {
        row: usize,
        rows: u64,
    },
    NotNumeric {
        column: String,
        row: usize,
    },
    /// The aggregate is undefined (no numeric values).
    NoValue,
    /// Previous-period comparison needs an aggregate source.
    PeriodNeedsAggregate,
    TimeSeries(TimeSeriesError),
}

impl From<TimeSeriesError> for KpiError {
    fn from(error: TimeSeriesError) -> Self {
        KpiError::TimeSeries(error)
    }
}

fn require_column(view: &OutputView<'_>, name: &str) -> Result<usize, KpiError> {
    view.column_index(name)
        .ok_or_else(|| KpiError::MissingColumn {
            column: name.to_string(),
        })
}

/// Aggregate one column of a view.
pub fn aggregate_column(view: &OutputView<'_>, column: usize, aggregate: Aggregate) -> Option<f64> {
    let mut accumulator = AggregateAccumulator::new(aggregate);
    view.for_each_row(|_, row| {
        accumulator.push(row.get(column).unwrap_or(&CellValue::Null));
        ControlFlow::Continue(())
    });
    accumulator.finish()
}

/// Read a value source from a view.
pub fn evaluate_value(view: &OutputView<'_>, source: &ValueSource) -> Result<f64, KpiError> {
    match source {
        ValueSource::RowCount => Ok(view.row_count() as f64),
        ValueSource::Aggregate { column, aggregate } => {
            let index = require_column(view, column)?;
            aggregate_column(view, index, *aggregate).ok_or(KpiError::NoValue)
        }
        ValueSource::Cell { column, row } => {
            let index = match column {
                Some(name) => require_column(view, name)?,
                None if view.columns.is_empty() => return Err(KpiError::NoColumns),
                None => 0,
            };
            let mut found = None;
            view.for_each_row(|at, cells| {
                if at == *row {
                    found = Some(cells.get(index).cloned().unwrap_or(CellValue::Null));
                    return ControlFlow::Break(());
                }
                ControlFlow::Continue(())
            });
            let cell = found.ok_or(KpiError::RowOutOfRange {
                row: *row,
                rows: view.row_count(),
            })?;
            finite_number(&cell).ok_or_else(|| KpiError::NotNumeric {
                column: view.columns[index].name.clone(),
                row: *row,
            })
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Comparison {
    /// Bucket the output by `time_column`; the value is the latest bucket's
    /// aggregate, compared with the bucket before it.
    PreviousPeriod {
        time_column: String,
        #[serde(default)]
        grain: Option<TimeGrain>,
    },
    /// The same value source read from a baseline output.
    Baseline,
    Target {
        value: f64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    HigherIsBetter,
    LowerIsBetter,
    Neutral,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThresholdBasis {
    #[default]
    Value,
    DeltaAbsolute,
    DeltaPercent,
}

/// Status bands. For higher-is-better a metric below `critical` is
/// critical and below `warn` is a warning; lower-is-better flips the
/// comparison; neutral bands the metric's magnitude.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Thresholds {
    #[serde(default)]
    pub basis: ThresholdBasis,
    #[serde(default)]
    pub warn: Option<f64>,
    #[serde(default)]
    pub critical: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparklineSpec {
    pub time_column: String,
    /// Defaults to the value source's column.
    #[serde(default)]
    pub value_column: Option<String>,
    #[serde(default = "default_sparkline_aggregate")]
    pub aggregate: Aggregate,
    #[serde(default)]
    pub grain: Option<TimeGrain>,
    #[serde(default = "default_sparkline_points")]
    pub max_points: usize,
}

fn default_sparkline_aggregate() -> Aggregate {
    Aggregate::Sum
}

fn default_sparkline_points() -> usize {
    60
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiSpec {
    pub value: ValueSource,
    #[serde(default)]
    pub comparison: Option<Comparison>,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub thresholds: Option<Thresholds>,
    #[serde(default)]
    pub sparkline: Option<SparklineSpec>,
    /// Overrides the inferred formatting.
    #[serde(default)]
    pub format: Option<FormatHint>,
    /// Report a status even when the input is truncated or sampled.
    #[serde(default)]
    pub allow_incomplete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trend {
    Up,
    Down,
    Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KpiStatus {
    Good,
    Warning,
    Critical,
    /// No direction or nothing to compare with.
    Neutral,
    /// The value could not be judged (missing, or incomplete input).
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NumberStyle {
    Integer,
    Decimal,
    Currency,
    /// `percent_of_one`: the value is a fraction (0.25 → 25%).
    Percent {
        percent_of_one: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormatHint {
    pub style: NumberStyle,
    pub decimals: u8,
    /// Abbreviate large magnitudes (1.2K, 3.4M).
    pub compact: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KpiResult {
    pub value: f64,
    /// The latest period's label (previous-period comparison only).
    pub period: Option<String>,
    pub comparison_value: Option<f64>,
    pub comparison_period: Option<String>,
    pub delta_absolute: Option<f64>,
    /// Percent change against the comparison (`None` when it is 0).
    pub delta_percent: Option<f64>,
    pub trend: Option<Trend>,
    /// Whether the change is good news for this KPI's direction.
    pub favorable: Option<bool>,
    pub status: KpiStatus,
    pub sparkline: Vec<SeriesPoint>,
    pub sparkline_grain: Option<TimeGrain>,
    pub format: FormatHint,
    pub completeness: Completeness,
    /// The input was incomplete and the value depends on every row, so the
    /// status is withheld (`Unknown`) unless the spec allows incomplete data.
    pub inconclusive: bool,
}

/// Evaluate a KPI over `view`, with `baseline` for a baseline comparison.
pub fn evaluate_kpi(
    view: &OutputView<'_>,
    spec: &KpiSpec,
    baseline: Option<&OutputView<'_>>,
) -> Result<KpiResult, KpiError> {
    let mut completeness = view.completeness;
    let (value, comparison_value) = match &spec.comparison {
        Some(Comparison::PreviousPeriod { time_column, grain }) => {
            let ValueSource::Aggregate { column, aggregate } = &spec.value else {
                if spec.value != ValueSource::RowCount {
                    return Err(KpiError::PeriodNeedsAggregate);
                }
                return evaluate_period(view, spec, time_column, None, Aggregate::Count, *grain);
            };
            return evaluate_period(view, spec, time_column, Some(column), *aggregate, *grain);
        }
        Some(Comparison::Baseline) => {
            let current = evaluate_value(view, &spec.value)?;
            let previous = match baseline {
                Some(baseline) => {
                    if !baseline.completeness.is_complete() {
                        completeness = baseline.completeness;
                    }
                    Some(evaluate_value(baseline, &spec.value)?)
                }
                None => None,
            };
            (current, previous)
        }
        Some(Comparison::Target { value }) => (evaluate_value(view, &spec.value)?, Some(*value)),
        None => (evaluate_value(view, &spec.value)?, None),
    };
    let sparkline = sparkline_for(view, spec)?;
    Ok(assemble(
        view,
        spec,
        value,
        comparison_value,
        (None, None),
        sparkline,
        completeness,
    ))
}

fn evaluate_period(
    view: &OutputView<'_>,
    spec: &KpiSpec,
    time_column: &str,
    value_column: Option<&str>,
    aggregate: Aggregate,
    grain: Option<TimeGrain>,
) -> Result<KpiResult, KpiError> {
    let series = time_series(
        view,
        time_column,
        value_column,
        aggregate,
        grain,
        usize::MAX,
    )?;
    let last = series.points.last().ok_or(KpiError::NoValue)?;
    let grain = series.grain;
    let last_start = super::time::from_epoch_seconds(last.t).ok_or(KpiError::NoValue)?;
    let previous_start = previous_bucket(floor_to_grain(last_start, grain), grain);
    let previous_t = super::time::epoch_seconds(previous_start);
    let previous = series
        .points
        .iter()
        .find(|point| point.t == previous_t)
        .map(|point| point.value)
        .or_else(|| aggregate.empty_is_zero().then_some(0.0));
    let sparkline = sparkline_for(view, spec)?;
    Ok(assemble(
        view,
        spec,
        last.value,
        previous,
        (
            Some(last.label.clone()),
            Some(bucket_label(previous_start, grain)),
        ),
        sparkline,
        view.completeness,
    ))
}

fn sparkline_for(view: &OutputView<'_>, spec: &KpiSpec) -> Result<Option<TimeSeries>, KpiError> {
    let Some(sparkline) = &spec.sparkline else {
        return Ok(None);
    };
    let value_column = sparkline.value_column.as_deref().or(spec.value.column());
    let aggregate = if value_column.is_none() {
        Aggregate::Count
    } else {
        sparkline.aggregate
    };
    let series = time_series(
        view,
        &sparkline.time_column,
        value_column,
        aggregate,
        sparkline.grain,
        sparkline.max_points.max(1),
    )?;
    Ok(Some(series))
}

fn assemble(
    view: &OutputView<'_>,
    spec: &KpiSpec,
    value: f64,
    comparison_value: Option<f64>,
    periods: (Option<String>, Option<String>),
    sparkline: Option<TimeSeries>,
    completeness: Completeness,
) -> KpiResult {
    let delta_absolute = comparison_value.map(|c| value - c);
    let delta_percent = comparison_value
        .filter(|c| *c != 0.0)
        .map(|c| (value - c) / c.abs() * 100.0);
    let trend = delta_absolute.map(|delta| {
        if delta.abs() <= f64::EPSILON * value.abs().max(1.0) {
            Trend::Flat
        } else if delta > 0.0 {
            Trend::Up
        } else {
            Trend::Down
        }
    });
    let favorable = trend.and_then(|trend| match (spec.direction, trend) {
        (Direction::Neutral, _) | (_, Trend::Flat) => None,
        (Direction::HigherIsBetter, Trend::Up) | (Direction::LowerIsBetter, Trend::Down) => {
            Some(true)
        }
        _ => Some(false),
    });
    let inconclusive = !completeness.is_complete()
        && (spec.value.depends_on_every_row()
            || matches!(spec.comparison, Some(Comparison::PreviousPeriod { .. })))
        && !spec.allow_incomplete;
    let status = if inconclusive {
        KpiStatus::Unknown
    } else if let Some(thresholds) = &spec.thresholds {
        let metric = match thresholds.basis {
            ThresholdBasis::Value => Some(value),
            ThresholdBasis::DeltaAbsolute => delta_absolute,
            ThresholdBasis::DeltaPercent => delta_percent,
        };
        match metric {
            Some(metric) => band(metric, spec.direction, thresholds),
            None => KpiStatus::Unknown,
        }
    } else if spec.direction == Direction::Neutral || comparison_value.is_none() {
        KpiStatus::Neutral
    } else if favorable == Some(false) {
        KpiStatus::Warning
    } else {
        KpiStatus::Good
    };
    let sparkline_grain = sparkline.as_ref().map(|series| series.grain);
    let sparkline = sparkline.map(|series| series.points).unwrap_or_default();
    KpiResult {
        value,
        period: periods.0,
        comparison_value,
        comparison_period: periods.1,
        delta_absolute,
        delta_percent,
        trend,
        favorable,
        status,
        sparkline,
        sparkline_grain,
        format: spec
            .format
            .unwrap_or_else(|| infer_format(view, &spec.value, value)),
        completeness,
        inconclusive,
    }
}

fn band(metric: f64, direction: Direction, thresholds: &Thresholds) -> KpiStatus {
    let breached = |limit: Option<f64>| {
        limit.is_some_and(|limit| match direction {
            Direction::HigherIsBetter => metric < limit,
            Direction::LowerIsBetter => metric > limit,
            Direction::Neutral => metric.abs() > limit.abs(),
        })
    };
    if breached(thresholds.critical) {
        KpiStatus::Critical
    } else if breached(thresholds.warn) {
        KpiStatus::Warning
    } else {
        KpiStatus::Good
    }
}

/// A formatting hint from the column's type and name and the value.
pub fn infer_format(view: &OutputView<'_>, source: &ValueSource, value: f64) -> FormatHint {
    let column = source
        .column()
        .and_then(|name| view.column_index(name))
        .or(match source {
            ValueSource::Cell { column: None, .. } if !view.columns.is_empty() => Some(0),
            _ => None,
        })
        .map(|index| &view.columns[index]);
    let counting = matches!(
        source,
        ValueSource::RowCount
            | ValueSource::Aggregate {
                aggregate: Aggregate::Count | Aggregate::CountNonNull | Aggregate::CountDistinct,
                ..
            }
    );
    let compact = value.abs() >= 1_000_000.0;
    if counting {
        return FormatHint {
            style: NumberStyle::Integer,
            decimals: 0,
            compact,
        };
    }
    let name = column
        .map(|c| c.name.to_ascii_lowercase())
        .unwrap_or_default();
    let data_type = column
        .map(|c| c.data_type.to_ascii_lowercase())
        .unwrap_or_default();
    let has_word = |words: &[&str]| words.iter().any(|word| name.contains(word));
    if data_type.contains("money")
        || has_word(&[
            "revenue", "price", "amount", "cost", "sales", "spend", "usd", "eur", "gbp", "mrr",
            "arr",
        ])
    {
        return FormatHint {
            style: NumberStyle::Currency,
            decimals: 2,
            compact,
        };
    }
    if has_word(&["pct", "percent", "rate", "ratio", "share"]) {
        return FormatHint {
            style: NumberStyle::Percent {
                percent_of_one: value.abs() <= 1.5,
            },
            decimals: 1,
            compact: false,
        };
    }
    let integral = column.is_some_and(|c| declared_class(&c.data_type) == DeclaredClass::Integer)
        || value.fract() == 0.0;
    FormatHint {
        style: if integral {
            NumberStyle::Integer
        } else {
            NumberStyle::Decimal
        },
        decimals: if integral { 0 } else { 2 },
        compact,
    }
}

/// Locale-neutral rendering of a KPI value for exports and tests (the UI
/// formats with the user's locale). Currency renders without a symbol,
/// since the result does not say which currency it is.
pub fn format_value(value: f64, hint: &FormatHint) -> String {
    if !value.is_finite() {
        return super::value::format_float(value);
    }
    let (scaled, suffix) = match hint.style {
        NumberStyle::Percent { percent_of_one } => {
            let percent = if percent_of_one { value * 100.0 } else { value };
            return format!("{}%", group(percent, hint.decimals));
        }
        _ if hint.compact => compact_scale(value),
        _ => (value, ""),
    };
    let decimals = if suffix.is_empty() { hint.decimals } else { 1 };
    format!("{}{suffix}", group(scaled, decimals))
}

fn compact_scale(value: f64) -> (f64, &'static str) {
    let magnitude = value.abs();
    if magnitude >= 1e12 {
        (value / 1e12, "T")
    } else if magnitude >= 1e9 {
        (value / 1e9, "B")
    } else if magnitude >= 1e6 {
        (value / 1e6, "M")
    } else if magnitude >= 1e3 {
        (value / 1e3, "K")
    } else {
        (value, "")
    }
}

fn group(value: f64, decimals: u8) -> String {
    let text = format!("{:.*}", usize::from(decimals), value.abs());
    let (int_part, frac_part) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (index, ch) in int_part.chars().enumerate() {
        if index > 0 && (int_part.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    let negative = value < 0.0 && text.chars().any(|c| c.is_ascii_digit() && c != '0');
    let sign = if negative { "-" } else { "" };
    if frac_part.is_empty() {
        format!("{sign}{grouped}")
    } else {
        format!("{sign}{grouped}.{frac_part}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::*;
    use sqlly_core::ResultColumn;

    fn sales() -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let columns = vec![
            col("day", "date"),
            col("revenue", "decimal(18,2)"),
            col("region", "text"),
        ];
        let rows = vec![
            vec![s("2024-01-01"), s("100.00"), s("east")],
            vec![s("2024-01-15"), s("50.00"), s("west")],
            vec![s("2024-02-03"), s("200.00"), s("east")],
            vec![s("2024-02-20"), CellValue::Null, s("west")],
            vec![s("2024-03-05"), s("120.00"), s("east")],
        ];
        (columns, rows)
    }

    #[test]
    fn aggregates_table() {
        let values = [
            i(3),
            f(1.5),
            s("2.5"),
            CellValue::Null,
            s("x"),
            f(f64::NAN),
            i(3),
        ];
        let cases = [
            (Aggregate::Sum, Some(10.0)),
            (Aggregate::Avg, Some(2.5)),
            (Aggregate::Min, Some(1.5)),
            (Aggregate::Max, Some(3.0)),
            (Aggregate::Count, Some(7.0)),
            (Aggregate::CountNonNull, Some(6.0)),
            (Aggregate::CountDistinct, Some(5.0)),
            (Aggregate::Median, Some(2.75)),
            (Aggregate::First, Some(3.0)),
            (Aggregate::Last, Some(3.0)),
        ];
        for (aggregate, expected) in cases {
            let mut acc = AggregateAccumulator::new(aggregate);
            values.iter().for_each(|v| acc.push(v));
            assert_eq!(acc.finish(), expected, "{aggregate:?}");
        }
        let empty = AggregateAccumulator::new(Aggregate::Avg);
        assert_eq!(empty.finish(), None);
        assert_eq!(
            AggregateAccumulator::new(Aggregate::Sum).finish(),
            Some(0.0)
        );
    }

    #[test]
    fn value_sources_read_cells_and_aggregates() {
        let (columns, rows) = sales();
        let view = OutputView::complete(&columns, &rows);
        assert_eq!(evaluate_value(&view, &ValueSource::RowCount), Ok(5.0));
        assert_eq!(
            evaluate_value(
                &view,
                &ValueSource::Aggregate {
                    column: "Revenue".into(),
                    aggregate: Aggregate::Sum
                }
            ),
            Ok(470.0)
        );
        assert_eq!(
            evaluate_value(
                &view,
                &ValueSource::Cell {
                    column: Some("revenue".into()),
                    row: 2
                }
            ),
            Ok(200.0)
        );
        assert_eq!(
            evaluate_value(
                &view,
                &ValueSource::Cell {
                    column: Some("revenue".into()),
                    row: 3
                }
            ),
            Err(KpiError::NotNumeric {
                column: "revenue".into(),
                row: 3
            })
        );
        assert_eq!(
            evaluate_value(
                &view,
                &ValueSource::Cell {
                    column: None,
                    row: 9
                }
            ),
            Err(KpiError::RowOutOfRange { row: 9, rows: 5 })
        );
        assert_eq!(
            evaluate_value(
                &view,
                &ValueSource::Aggregate {
                    column: "nope".into(),
                    aggregate: Aggregate::Sum
                }
            ),
            Err(KpiError::MissingColumn {
                column: "nope".into()
            })
        );
    }

    #[test]
    fn previous_period_comparison_and_sparkline() {
        let (columns, rows) = sales();
        let view = OutputView::complete(&columns, &rows);
        let spec = KpiSpec {
            value: ValueSource::Aggregate {
                column: "revenue".into(),
                aggregate: Aggregate::Sum,
            },
            comparison: Some(Comparison::PreviousPeriod {
                time_column: "day".into(),
                grain: Some(TimeGrain::Month),
            }),
            direction: Direction::HigherIsBetter,
            thresholds: None,
            sparkline: Some(SparklineSpec {
                time_column: "day".into(),
                value_column: None,
                aggregate: Aggregate::Sum,
                grain: Some(TimeGrain::Month),
                max_points: 12,
            }),
            format: None,
            allow_incomplete: false,
        };
        let kpi = evaluate_kpi(&view, &spec, None).unwrap();
        assert_eq!(kpi.value, 120.0);
        assert_eq!(kpi.period.as_deref(), Some("2024-03"));
        assert_eq!(kpi.comparison_value, Some(200.0));
        assert_eq!(kpi.comparison_period.as_deref(), Some("2024-02"));
        assert_eq!(kpi.delta_absolute, Some(-80.0));
        assert_eq!(kpi.delta_percent, Some(-40.0));
        assert_eq!(kpi.trend, Some(Trend::Down));
        assert_eq!(kpi.favorable, Some(false));
        assert_eq!(kpi.status, KpiStatus::Warning);
        assert_eq!(
            kpi.sparkline.iter().map(|p| p.value).collect::<Vec<_>>(),
            vec![150.0, 200.0, 120.0]
        );
        assert_eq!(kpi.sparkline_grain, Some(TimeGrain::Month));
        assert_eq!(kpi.format.style, NumberStyle::Currency);

        let lower = KpiSpec {
            direction: Direction::LowerIsBetter,
            ..spec.clone()
        };
        let kpi = evaluate_kpi(&view, &lower, None).unwrap();
        assert_eq!((kpi.favorable, kpi.status), (Some(true), KpiStatus::Good));
    }

    #[test]
    fn missing_previous_period_is_zero_for_sums_and_unknown_for_averages() {
        let columns = vec![col("day", "date"), col("v", "int")];
        let rows = vec![vec![s("2024-01-01"), i(5)], vec![s("2024-03-01"), i(7)]];
        let view = OutputView::complete(&columns, &rows);
        let spec = |aggregate| KpiSpec {
            value: ValueSource::Aggregate {
                column: "v".into(),
                aggregate,
            },
            comparison: Some(Comparison::PreviousPeriod {
                time_column: "day".into(),
                grain: Some(TimeGrain::Month),
            }),
            direction: Direction::HigherIsBetter,
            thresholds: None,
            sparkline: None,
            format: None,
            allow_incomplete: false,
        };
        let sum = evaluate_kpi(&view, &spec(Aggregate::Sum), None).unwrap();
        assert_eq!(sum.comparison_value, Some(0.0));
        assert_eq!(sum.delta_percent, None, "no percent change from zero");
        let avg = evaluate_kpi(&view, &spec(Aggregate::Avg), None).unwrap();
        assert_eq!(avg.comparison_value, None);
        assert_eq!(avg.status, KpiStatus::Neutral);
        let cell = KpiSpec {
            value: ValueSource::Cell {
                column: None,
                row: 0,
            },
            ..spec(Aggregate::Sum)
        };
        assert_eq!(
            evaluate_kpi(&view, &cell, None).unwrap_err(),
            KpiError::PeriodNeedsAggregate
        );
    }

    #[test]
    fn baseline_target_and_threshold_statuses() {
        let columns = vec![col("latency_ms", "float")];
        let current = vec![vec![f(250.0)]];
        let previous = vec![vec![f(200.0)]];
        let view = OutputView::complete(&columns, &current);
        let base_view = OutputView::complete(&columns, &previous);
        let base = KpiSpec {
            value: ValueSource::Cell {
                column: None,
                row: 0,
            },
            comparison: Some(Comparison::Baseline),
            direction: Direction::LowerIsBetter,
            thresholds: None,
            sparkline: None,
            format: None,
            allow_incomplete: false,
        };
        let kpi = evaluate_kpi(&view, &base, Some(&base_view)).unwrap();
        assert_eq!(
            (kpi.delta_percent, kpi.status),
            (Some(25.0), KpiStatus::Warning)
        );

        let cases = [
            (
                ThresholdBasis::Value,
                Some(240.0),
                Some(300.0),
                KpiStatus::Warning,
            ),
            (
                ThresholdBasis::Value,
                Some(260.0),
                Some(300.0),
                KpiStatus::Good,
            ),
            (
                ThresholdBasis::Value,
                Some(100.0),
                Some(200.0),
                KpiStatus::Critical,
            ),
            (
                ThresholdBasis::DeltaPercent,
                Some(10.0),
                Some(20.0),
                KpiStatus::Critical,
            ),
            (
                ThresholdBasis::DeltaAbsolute,
                Some(40.0),
                None,
                KpiStatus::Warning,
            ),
        ];
        for (basis, warn, critical, expected) in cases {
            let spec = KpiSpec {
                thresholds: Some(Thresholds {
                    basis,
                    warn,
                    critical,
                }),
                ..base.clone()
            };
            assert_eq!(
                evaluate_kpi(&view, &spec, Some(&base_view)).unwrap().status,
                expected,
                "{basis:?} {warn:?} {critical:?}"
            );
        }
        let target = KpiSpec {
            comparison: Some(Comparison::Target { value: 300.0 }),
            ..base.clone()
        };
        let kpi = evaluate_kpi(&view, &target, None).unwrap();
        assert_eq!(
            (kpi.delta_absolute, kpi.status),
            (Some(-50.0), KpiStatus::Good)
        );
        let neutral = KpiSpec {
            direction: Direction::Neutral,
            ..base.clone()
        };
        assert_eq!(
            evaluate_kpi(&view, &neutral, Some(&base_view))
                .unwrap()
                .status,
            KpiStatus::Neutral
        );
        let neutral_band = KpiSpec {
            thresholds: Some(Thresholds {
                basis: ThresholdBasis::DeltaPercent,
                warn: Some(10.0),
                critical: Some(50.0),
            }),
            ..neutral
        };
        assert_eq!(
            evaluate_kpi(&view, &neutral_band, Some(&base_view))
                .unwrap()
                .status,
            KpiStatus::Warning
        );
    }

    #[test]
    fn incomplete_inputs_withhold_status_for_aggregates_only() {
        let (columns, rows) = sales();
        let view = OutputView::new(&columns, &rows, Completeness::Truncated { cap: 5 });
        let total = KpiSpec {
            value: ValueSource::Aggregate {
                column: "revenue".into(),
                aggregate: Aggregate::Sum,
            },
            comparison: Some(Comparison::Target { value: 400.0 }),
            direction: Direction::HigherIsBetter,
            thresholds: None,
            sparkline: None,
            format: None,
            allow_incomplete: false,
        };
        let kpi = evaluate_kpi(&view, &total, None).unwrap();
        assert!(kpi.inconclusive);
        assert_eq!(kpi.status, KpiStatus::Unknown);
        assert_eq!(kpi.value, 470.0);
        let allowed = KpiSpec {
            allow_incomplete: true,
            ..total.clone()
        };
        assert_eq!(
            evaluate_kpi(&view, &allowed, None).unwrap().status,
            KpiStatus::Good
        );
        let cell = KpiSpec {
            value: ValueSource::Cell {
                column: Some("revenue".into()),
                row: 0,
            },
            ..total
        };
        assert!(!evaluate_kpi(&view, &cell, None).unwrap().inconclusive);
    }

    #[test]
    fn formats_are_inferred_and_rendered() {
        let columns = vec![
            col("conversion_rate", "float"),
            col("orders", "int"),
            col("revenue", "money"),
        ];
        let rows = vec![vec![f(0.1234), i(12), f(1_234_567.891)]];
        let view = OutputView::complete(&columns, &rows);
        let rate = infer_format(
            &view,
            &ValueSource::Cell {
                column: Some("conversion_rate".into()),
                row: 0,
            },
            0.1234,
        );
        assert_eq!(format_value(0.1234, &rate), "12.3%");
        let orders = infer_format(
            &view,
            &ValueSource::Cell {
                column: Some("orders".into()),
                row: 0,
            },
            12.0,
        );
        assert_eq!(format_value(12_345.0, &orders), "12,345");
        let revenue = infer_format(
            &view,
            &ValueSource::Cell {
                column: Some("revenue".into()),
                row: 0,
            },
            1_234_567.891,
        );
        assert_eq!(revenue.style, NumberStyle::Currency);
        assert_eq!(format_value(1_234_567.891, &revenue), "1.2M");
        assert_eq!(
            format_value(
                -1_234.5,
                &FormatHint {
                    style: NumberStyle::Decimal,
                    decimals: 2,
                    compact: false
                }
            ),
            "-1,234.50"
        );
        assert_eq!(
            format_value(
                -0.001,
                &FormatHint {
                    style: NumberStyle::Decimal,
                    decimals: 2,
                    compact: false
                }
            ),
            "0.00"
        );
        assert_eq!(format_value(f64::NAN, &orders), "NaN");
        assert_eq!(
            infer_format(&view, &ValueSource::RowCount, 3.0).style,
            NumberStyle::Integer
        );
    }

    #[test]
    fn kpi_specs_round_trip_through_serde() {
        let spec = KpiSpec {
            value: ValueSource::Aggregate {
                column: "revenue".into(),
                aggregate: Aggregate::Sum,
            },
            comparison: Some(Comparison::PreviousPeriod {
                time_column: "day".into(),
                grain: None,
            }),
            direction: Direction::HigherIsBetter,
            thresholds: Some(Thresholds {
                basis: ThresholdBasis::DeltaPercent,
                warn: Some(-5.0),
                critical: Some(-20.0),
            }),
            sparkline: None,
            format: None,
            allow_incomplete: false,
        };
        let text = serde_json::to_string(&spec).unwrap();
        assert_eq!(serde_json::from_str::<KpiSpec>(&text).unwrap(), spec);
        let minimal: KpiSpec = serde_json::from_str(r#"{"value":{"kind":"row_count"}}"#).unwrap();
        assert_eq!(minimal.direction, Direction::HigherIsBetter);
    }
}
