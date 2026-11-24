//! Time-series and breakdown analytics: bucketed series, rolling anomaly
//! bands (optionally same-weekday seasonal), contribution decomposition
//! between two periods, Pareto, cohort retention, correlation matrices, and
//! a naive linear trend forecast.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ops::ControlFlow;

use chrono::{Datelike, NaiveDateTime};
use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::OutputView;
use super::kpi::{Aggregate, AggregateAccumulator};
use super::time::{
    TimeGrain, bucket_label, bucket_starts, cell_timestamp, choose_grain, diff_in, epoch_seconds,
    floor_to_grain, from_epoch_seconds, has_time_of_day, next_bucket,
};
use super::value::{DeclaredClass, cell_hash, cell_text, declared_class, finite_number};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeSeriesError {
    MissingColumn {
        column: String,
    },
    /// No row had a readable timestamp.
    NoTimestamps,
}

/// One bucket of a series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriesPoint {
    /// Bucket start, epoch seconds (UTC).
    pub t: i64,
    pub label: String,
    pub value: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeSeries {
    pub grain: TimeGrain,
    pub points: Vec<SeriesPoint>,
    /// Rows whose time cell was NULL or unreadable.
    pub skipped_rows: u64,
    /// Older buckets were dropped to fit `max_points`.
    pub truncated: bool,
}

fn require(view: &OutputView<'_>, name: &str) -> Result<usize, TimeSeriesError> {
    view.column_index(name)
        .ok_or_else(|| TimeSeriesError::MissingColumn {
            column: name.to_string(),
        })
}

/// Bucket `view` by `time_column` and aggregate `value_column` (row count
/// when `None`) per bucket. `grain: None` picks the finest grain that fits
/// `max_points` (at most 366). Sums and counts fill empty buckets with 0;
/// other aggregates leave them out. Keeps the latest `max_points` buckets.
pub fn time_series(
    view: &OutputView<'_>,
    time_column: &str,
    value_column: Option<&str>,
    aggregate: Aggregate,
    grain: Option<TimeGrain>,
    max_points: usize,
) -> Result<TimeSeries, TimeSeriesError> {
    let time_index = require(view, time_column)?;
    let value_index = value_column.map(|name| require(view, name)).transpose()?;
    let aggregate = if value_index.is_none() {
        Aggregate::Count
    } else {
        aggregate
    };
    let max_points = max_points.max(1);

    let mut bounds: Option<(NaiveDateTime, NaiveDateTime)> = None;
    let mut date_only = true;
    view.for_each_row(|_, row| {
        if let Some(stamp) = row.get(time_index).and_then(cell_timestamp) {
            date_only &= !has_time_of_day(stamp);
            bounds = Some(match bounds {
                Some((min, max)) => (min.min(stamp), max.max(stamp)),
                None => (stamp, stamp),
            });
        }
        ControlFlow::Continue(())
    });
    let (min, max) = bounds.ok_or(TimeSeriesError::NoTimestamps)?;
    let grain = grain.unwrap_or_else(|| choose_grain(min, max, max_points.min(366), date_only));

    let mut buckets: BTreeMap<i64, AggregateAccumulator> = BTreeMap::new();
    let mut skipped = 0u64;
    view.for_each_row(|_, row| {
        let Some(stamp) = row.get(time_index).and_then(cell_timestamp) else {
            skipped += 1;
            return ControlFlow::Continue(());
        };
        let start = epoch_seconds(floor_to_grain(stamp, grain));
        let cell = match value_index {
            Some(index) => row.get(index).unwrap_or(&CellValue::Null),
            None => &CellValue::Bool(true),
        };
        buckets
            .entry(start)
            .or_insert_with(|| AggregateAccumulator::new(aggregate))
            .push(cell);
        ControlFlow::Continue(())
    });

    let mut points: Vec<SeriesPoint> = Vec::new();
    if aggregate.empty_is_zero() {
        let span_cap = max_points.saturating_add(buckets.len()).min(100_000);
        let starts = bucket_starts(min, max, grain, span_cap);
        for start in starts {
            let t = epoch_seconds(start);
            let value = buckets.get(&t).and_then(|acc| acc.finish()).unwrap_or(0.0);
            points.push(SeriesPoint {
                t,
                label: bucket_label(start, grain),
                value,
            });
        }
    } else {
        for (t, accumulator) in &buckets {
            if let (Some(value), Some(start)) = (accumulator.finish(), from_epoch_seconds(*t)) {
                points.push(SeriesPoint {
                    t: *t,
                    label: bucket_label(start, grain),
                    value,
                });
            }
        }
    }
    let truncated = points.len() > max_points;
    if truncated {
        points.drain(..points.len() - max_points);
    }
    Ok(TimeSeries {
        grain,
        points,
        skipped_rows: skipped,
        truncated,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Seasonality {
    #[default]
    None,
    /// Compare each point with earlier points on the same weekday.
    SameWeekday,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnomalyOptions {
    /// Earlier points in the baseline window.
    pub window: usize,
    /// Band half-width in standard deviations.
    pub k: f64,
    /// Fewest earlier points before a band is drawn.
    pub min_history: usize,
    pub seasonality: Seasonality,
}

impl Default for AnomalyOptions {
    fn default() -> Self {
        Self {
            window: 7,
            k: 3.0,
            min_history: 3,
            seasonality: Seasonality::None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnomalyDirection {
    Above,
    Below,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BandPoint {
    pub t: i64,
    pub label: String,
    pub value: f64,
    /// Baseline mean of the earlier window; `None` until enough history.
    pub expected: Option<f64>,
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    pub z_score: Option<f64>,
    pub anomaly: Option<AnomalyDirection>,
}

/// Rolling mean ± k·stddev bands. Each point is judged against the points
/// before it (never itself), so a spike does not widen its own band.
pub fn anomaly_bands(points: &[SeriesPoint], options: &AnomalyOptions) -> Vec<BandPoint> {
    let window = options.window.max(1);
    let min_history = options.min_history.clamp(1, window);
    points
        .iter()
        .enumerate()
        .map(|(index, point)| {
            let weekday = weekday_of(point.t);
            let history: Vec<f64> = points[..index]
                .iter()
                .rev()
                .filter(|earlier| match options.seasonality {
                    Seasonality::None => true,
                    Seasonality::SameWeekday => weekday_of(earlier.t) == weekday,
                })
                .filter(|earlier| earlier.value.is_finite())
                .take(window)
                .map(|earlier| earlier.value)
                .collect();
            if history.len() < min_history || !point.value.is_finite() {
                return BandPoint {
                    t: point.t,
                    label: point.label.clone(),
                    value: point.value,
                    expected: None,
                    lower: None,
                    upper: None,
                    z_score: None,
                    anomaly: None,
                };
            }
            let (mean, stddev) = mean_stddev(&history);
            let lower = mean - options.k * stddev;
            let upper = mean + options.k * stddev;
            let z_score = (stddev > 0.0).then(|| (point.value - mean) / stddev);
            let anomaly = if point.value > upper {
                Some(AnomalyDirection::Above)
            } else if point.value < lower {
                Some(AnomalyDirection::Below)
            } else {
                None
            };
            BandPoint {
                t: point.t,
                label: point.label.clone(),
                value: point.value,
                expected: Some(mean),
                lower: Some(lower),
                upper: Some(upper),
                z_score,
                anomaly,
            }
        })
        .collect()
}

fn weekday_of(t: i64) -> Option<u32> {
    from_epoch_seconds(t).map(|value| value.weekday().num_days_from_monday())
}

fn mean_stddev(values: &[f64]) -> (f64, f64) {
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    if values.len() < 2 {
        return (mean, 0.0);
    }
    let variance = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, variance.sqrt())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContributionItem {
    pub key: String,
    pub before: f64,
    pub after: f64,
    pub delta: f64,
    /// `delta` ÷ the total change; `None` when the total did not change.
    pub share_of_change: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contribution {
    pub dimension: String,
    pub before_total: f64,
    pub after_total: f64,
    pub delta: f64,
    /// Largest movers first.
    pub items: Vec<ContributionItem>,
    /// How concentrated the change is: the largest |delta| ÷ Σ|delta|
    /// (1.0 when one value explains all movement).
    pub concentration: f64,
    /// Values beyond `max_items` folded into one `"(other)"` item.
    pub folded_items: usize,
}

/// Which dimension values explain the change from `before` to `after`.
/// Values missing on one side count as 0 there.
pub fn decompose(
    dimension: &str,
    before: &BTreeMap<String, f64>,
    after: &BTreeMap<String, f64>,
    max_items: usize,
) -> Contribution {
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let before_total: f64 = before.values().sum();
    let after_total: f64 = after.values().sum();
    let delta = after_total - before_total;
    let mut items: Vec<ContributionItem> = keys
        .into_iter()
        .map(|key| {
            let b = before.get(key).copied().unwrap_or(0.0);
            let a = after.get(key).copied().unwrap_or(0.0);
            ContributionItem {
                key: key.clone(),
                before: b,
                after: a,
                delta: a - b,
                share_of_change: (delta != 0.0).then(|| (a - b) / delta),
            }
        })
        .collect();
    items.sort_by(|x, y| {
        y.delta
            .abs()
            .total_cmp(&x.delta.abs())
            .then_with(|| x.key.cmp(&y.key))
    });
    let movement: f64 = items.iter().map(|item| item.delta.abs()).sum();
    let concentration = match items.first() {
        Some(top) if movement > 0.0 => top.delta.abs() / movement,
        _ => 0.0,
    };
    let max_items = max_items.max(1);
    let mut folded_items = 0;
    if items.len() > max_items {
        let rest = items.split_off(max_items - 1);
        folded_items = rest.len();
        let (b, a) = rest
            .iter()
            .fold((0.0, 0.0), |(b, a), item| (b + item.before, a + item.after));
        items.push(ContributionItem {
            key: "(other)".to_string(),
            before: b,
            after: a,
            delta: a - b,
            share_of_change: (delta != 0.0).then(|| (a - b) / delta),
        });
    }
    Contribution {
        dimension: dimension.to_string(),
        before_total,
        after_total,
        delta,
        items,
        concentration,
        folded_items,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContributionSpec {
    pub period_column: String,
    /// Period values as text (`2024-01`, `2024-01-01`, `Q1`…). Timestamps
    /// match by instant, so `2024-01-01` matches `2024-01-01 00:00:00`.
    pub before: String,
    pub after: String,
    pub dimensions: Vec<String>,
    /// Row count per value when `None`.
    #[serde(default)]
    pub measure: Option<String>,
    #[serde(default = "default_contribution_aggregate")]
    pub aggregate: Aggregate,
    #[serde(default = "default_max_items")]
    pub max_items: usize,
}

fn default_contribution_aggregate() -> Aggregate {
    Aggregate::Sum
}

fn default_max_items() -> usize {
    20
}

/// Decompose the change between two periods along each dimension, most
/// concentrated (most explanatory) dimension first.
pub fn contribution_by_dimensions(
    view: &OutputView<'_>,
    spec: &ContributionSpec,
) -> Result<Vec<Contribution>, TimeSeriesError> {
    let period_index = require(view, &spec.period_column)?;
    let measure_index = spec
        .measure
        .as_deref()
        .map(|name| require(view, name))
        .transpose()?;
    let dimension_indexes = spec
        .dimensions
        .iter()
        .map(|name| require(view, name))
        .collect::<Result<Vec<_>, _>>()?;
    let aggregate = if measure_index.is_none() {
        Aggregate::Count
    } else {
        spec.aggregate
    };
    let before_stamp = super::time::parse_timestamp(&spec.before);
    let after_stamp = super::time::parse_timestamp(&spec.after);
    let matches = |cell: &CellValue, text: &str, stamp: Option<NaiveDateTime>| match (
        stamp,
        cell_timestamp(cell),
    ) {
        (Some(want), Some(have)) => want == have,
        _ => cell_text(cell).is_some_and(|value| value == text),
    };

    type Groups = Vec<(
        BTreeMap<String, AggregateAccumulator>,
        BTreeMap<String, AggregateAccumulator>,
    )>;
    let mut groups: Groups = dimension_indexes
        .iter()
        .map(|_| (BTreeMap::new(), BTreeMap::new()))
        .collect();
    view.for_each_row(|_, row| {
        let period = row.get(period_index).unwrap_or(&CellValue::Null);
        let side = if matches(period, &spec.before, before_stamp) {
            0
        } else if matches(period, &spec.after, after_stamp) {
            1
        } else {
            return ControlFlow::Continue(());
        };
        let cell = match measure_index {
            Some(index) => row.get(index).unwrap_or(&CellValue::Null),
            None => &CellValue::Bool(true),
        };
        for (group, dimension) in groups.iter_mut().zip(&dimension_indexes) {
            let key = row
                .get(*dimension)
                .and_then(cell_text)
                .map(|text| text.into_owned())
                .unwrap_or_else(|| "(null)".to_string());
            let map = if side == 0 {
                &mut group.0
            } else {
                &mut group.1
            };
            map.entry(key)
                .or_insert_with(|| AggregateAccumulator::new(aggregate))
                .push(cell);
        }
        ControlFlow::Continue(())
    });
    let finish = |map: &BTreeMap<String, AggregateAccumulator>| -> BTreeMap<String, f64> {
        map.iter()
            .filter_map(|(key, acc)| acc.finish().map(|value| (key.clone(), value)))
            .collect()
    };
    let mut contributions: Vec<Contribution> = groups
        .iter()
        .zip(&spec.dimensions)
        .map(|((before, after), name)| {
            decompose(name, &finish(before), &finish(after), spec.max_items)
        })
        .collect();
    contributions.sort_by(|a, b| {
        b.concentration
            .total_cmp(&a.concentration)
            .then_with(|| a.dimension.cmp(&b.dimension))
    });
    Ok(contributions)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParetoItem {
    pub key: String,
    pub value: f64,
    pub share: f64,
    pub cumulative_share: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pareto {
    pub total: f64,
    pub items: Vec<ParetoItem>,
    /// Items needed to reach `threshold` of the total.
    pub vital_few: usize,
    pub threshold: f64,
    /// Negative or non-finite values left out (Pareto needs shares).
    pub excluded: usize,
}

/// Rank items by value with cumulative shares; `threshold` is e.g. 0.8.
pub fn pareto(items: impl IntoIterator<Item = (String, f64)>, threshold: f64) -> Pareto {
    let mut excluded = 0;
    let mut kept: Vec<(String, f64)> = Vec::new();
    for (key, value) in items {
        if value.is_finite() && value >= 0.0 {
            kept.push((key, value));
        } else {
            excluded += 1;
        }
    }
    kept.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total: f64 = kept.iter().map(|(_, v)| v).sum();
    let mut cumulative = 0.0;
    let mut vital_few = 0;
    let items = kept
        .into_iter()
        .map(|(key, value)| {
            let share = if total > 0.0 { value / total } else { 0.0 };
            if total > 0.0 && cumulative < threshold - 1e-12 {
                vital_few += 1;
            }
            cumulative += share;
            ParetoItem {
                key,
                value,
                share,
                cumulative_share: cumulative.min(1.0),
            }
        })
        .collect();
    Pareto {
        total,
        items,
        vital_few,
        threshold,
        excluded,
    }
}

/// Pareto of `measure` (row count when `None`) grouped by `dimension`.
pub fn pareto_by(
    view: &OutputView<'_>,
    dimension: &str,
    measure: Option<&str>,
    aggregate: Aggregate,
    threshold: f64,
) -> Result<Pareto, TimeSeriesError> {
    let dimension_index = require(view, dimension)?;
    let measure_index = measure.map(|name| require(view, name)).transpose()?;
    let aggregate = if measure_index.is_none() {
        Aggregate::Count
    } else {
        aggregate
    };
    let mut groups: BTreeMap<String, AggregateAccumulator> = BTreeMap::new();
    view.for_each_row(|_, row| {
        let key = row
            .get(dimension_index)
            .and_then(cell_text)
            .map(|text| text.into_owned())
            .unwrap_or_else(|| "(null)".to_string());
        let cell = match measure_index {
            Some(index) => row.get(index).unwrap_or(&CellValue::Null),
            None => &CellValue::Bool(true),
        };
        groups
            .entry(key)
            .or_insert_with(|| AggregateAccumulator::new(aggregate))
            .push(cell);
        ControlFlow::Continue(())
    });
    Ok(pareto(
        groups
            .into_iter()
            .filter_map(|(key, acc)| acc.finish().map(|value| (key, value))),
        threshold,
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CohortSpec {
    pub entity_column: String,
    pub activity_column: String,
    /// The cohort date (e.g. signup); the entity's first activity when
    /// `None`.
    #[serde(default)]
    pub cohort_column: Option<String>,
    #[serde(default = "default_cohort_grain")]
    pub grain: TimeGrain,
    #[serde(default = "default_cohort_cap")]
    pub max_cohorts: usize,
    #[serde(default = "default_cohort_cap")]
    pub max_periods: usize,
}

fn default_cohort_grain() -> TimeGrain {
    TimeGrain::Month
}

fn default_cohort_cap() -> usize {
    36
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CohortRow {
    pub cohort: String,
    pub size: u64,
    /// Entities active `p` periods after their cohort start. A row stops at
    /// the last period the data reaches, so a period that has not happened
    /// yet is absent rather than 0% retained.
    pub retained: Vec<u64>,
    pub rates: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CohortMatrix {
    pub grain: TimeGrain,
    pub periods: usize,
    pub rows: Vec<CohortRow>,
    /// Older cohorts or later periods were left out by the caps.
    pub truncated: bool,
}

/// Build a cohort retention matrix: rows are cohorts (oldest first), columns
/// are periods since the cohort started.
pub fn cohort_retention(
    view: &OutputView<'_>,
    spec: &CohortSpec,
) -> Result<CohortMatrix, TimeSeriesError> {
    let entity_index = require(view, &spec.entity_column)?;
    let activity_index = require(view, &spec.activity_column)?;
    let cohort_index = spec
        .cohort_column
        .as_deref()
        .map(|name| require(view, name))
        .transpose()?;
    let grain = spec.grain;
    struct Entity {
        cohort: Option<NaiveDateTime>,
        activity: BTreeSet<NaiveDateTime>,
    }
    let mut entities: HashMap<u64, Entity> = HashMap::new();
    view.for_each_row(|_, row| {
        let entity = row.get(entity_index).unwrap_or(&CellValue::Null);
        if matches!(entity, CellValue::Null) {
            return ControlFlow::Continue(());
        }
        let state = entities.entry(cell_hash(entity, 7)).or_insert(Entity {
            cohort: None,
            activity: BTreeSet::new(),
        });
        if let Some(index) = cohort_index
            && let Some(stamp) = row.get(index).and_then(cell_timestamp)
        {
            let start = floor_to_grain(stamp, grain);
            state.cohort = Some(state.cohort.map_or(start, |c| c.min(start)));
        }
        if let Some(stamp) = row.get(activity_index).and_then(cell_timestamp) {
            state.activity.insert(floor_to_grain(stamp, grain));
        }
        ControlFlow::Continue(())
    });
    if entities.is_empty() {
        return Err(TimeSeriesError::NoTimestamps);
    }
    let latest = entities
        .values()
        .filter_map(|e| e.activity.last().copied())
        .max();
    let mut cohorts: BTreeMap<NaiveDateTime, (u64, BTreeMap<usize, u64>)> = BTreeMap::new();
    let mut truncated = false;
    for entity in entities.values() {
        let Some(cohort) = entity.cohort.or_else(|| entity.activity.first().copied()) else {
            continue;
        };
        let row = cohorts.entry(cohort).or_default();
        row.0 += 1;
        for activity in &entity.activity {
            let offset = diff_in(grain, cohort, *activity);
            if offset < 0 {
                continue;
            }
            let offset = offset as usize;
            if offset >= spec.max_periods.max(1) {
                truncated = true;
                continue;
            }
            *row.1.entry(offset).or_default() += 1;
        }
    }
    if cohorts.len() > spec.max_cohorts.max(1) {
        truncated = true;
        let drop = cohorts.len() - spec.max_cohorts.max(1);
        let keys: Vec<NaiveDateTime> = cohorts.keys().take(drop).copied().collect();
        for key in keys {
            cohorts.remove(&key);
        }
    }
    let periods = cohorts
        .values()
        .flat_map(|(_, offsets)| offsets.keys().copied())
        .max()
        .map_or(0, |max| max + 1);
    let rows = cohorts
        .into_iter()
        .map(|(start, (size, offsets))| {
            let observable = latest
                .map_or(periods, |latest| {
                    (diff_in(grain, start, latest).max(0) as usize) + 1
                })
                .min(periods);
            let retained: Vec<u64> = (0..observable)
                .map(|p| offsets.get(&p).copied().unwrap_or(0))
                .collect();
            let rates = retained
                .iter()
                .map(|count| {
                    if size == 0 {
                        0.0
                    } else {
                        *count as f64 / size as f64
                    }
                })
                .collect();
            CohortRow {
                cohort: bucket_label(start, grain),
                size,
                retained,
                rates,
            }
        })
        .collect();
    Ok(CohortMatrix {
        grain,
        periods,
        rows,
        truncated,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CorrelationMatrix {
    pub columns: Vec<String>,
    /// Pearson r per pair over rows where both are numbers; `None` when
    /// fewer than 3 pairs or a side is constant.
    pub values: Vec<Vec<Option<f64>>>,
    pub counts: Vec<Vec<u64>>,
}

/// Most columns a correlation matrix compares.
pub const MAX_CORRELATION_COLUMNS: usize = 40;

#[derive(Clone, Copy, Default)]
struct CoMoment {
    n: u64,
    mean_x: f64,
    mean_y: f64,
    m2_x: f64,
    m2_y: f64,
    c_xy: f64,
}

impl CoMoment {
    fn push(&mut self, x: f64, y: f64) {
        self.n += 1;
        let n = self.n as f64;
        let dx = x - self.mean_x;
        self.mean_x += dx / n;
        let dy = y - self.mean_y;
        self.mean_y += dy / n;
        self.m2_x += dx * (x - self.mean_x);
        self.m2_y += dy * (y - self.mean_y);
        self.c_xy += dx * (y - self.mean_y);
    }

    fn pearson(&self) -> Option<f64> {
        (self.n >= 3 && self.m2_x > 0.0 && self.m2_y > 0.0)
            .then(|| (self.c_xy / (self.m2_x.sqrt() * self.m2_y.sqrt())).clamp(-1.0, 1.0))
    }
}

/// Pairwise Pearson correlations. `columns: None` takes every numeric
/// column (declared numeric, or mostly numeric values), up to
/// [`MAX_CORRELATION_COLUMNS`].
pub fn correlation_matrix(
    view: &OutputView<'_>,
    columns: Option<&[String]>,
) -> Result<CorrelationMatrix, TimeSeriesError> {
    let indexes: Vec<usize> = match columns {
        Some(names) => names
            .iter()
            .map(|name| require(view, name))
            .collect::<Result<_, _>>()?,
        None => numeric_columns(view),
    };
    let indexes: Vec<usize> = indexes.into_iter().take(MAX_CORRELATION_COLUMNS).collect();
    let k = indexes.len();
    let mut moments = vec![CoMoment::default(); k * k];
    view.for_each_row(|_, row| {
        let values: Vec<Option<f64>> = indexes
            .iter()
            .map(|index| row.get(*index).and_then(finite_number))
            .collect();
        for a in 0..k {
            let Some(x) = values[a] else { continue };
            for b in a..k {
                if let Some(y) = values[b] {
                    moments[a * k + b].push(x, y);
                }
            }
        }
        ControlFlow::Continue(())
    });
    let mut values = vec![vec![None; k]; k];
    let mut counts = vec![vec![0u64; k]; k];
    for a in 0..k {
        for b in a..k {
            let moment = moments[a * k + b];
            let r = if a == b {
                (moment.n > 0 && moment.m2_x > 0.0).then_some(1.0)
            } else {
                moment.pearson()
            };
            values[a][b] = r;
            values[b][a] = r;
            counts[a][b] = moment.n;
            counts[b][a] = moment.n;
        }
    }
    Ok(CorrelationMatrix {
        columns: indexes
            .iter()
            .map(|index| view.columns[*index].name.clone())
            .collect(),
        values,
        counts,
    })
}

fn numeric_columns(view: &OutputView<'_>) -> Vec<usize> {
    let declared: Vec<DeclaredClass> = view
        .columns
        .iter()
        .map(|c| declared_class(&c.data_type))
        .collect();
    let mut numeric = vec![0u64; view.columns.len()];
    let mut non_null = vec![0u64; view.columns.len()];
    view.for_each_row(|_, row| {
        for (index, cell) in row.iter().enumerate().take(view.columns.len()) {
            if matches!(cell, CellValue::Null) {
                continue;
            }
            non_null[index] += 1;
            if finite_number(cell).is_some() {
                numeric[index] += 1;
            }
        }
        ControlFlow::Continue(())
    });
    (0..view.columns.len())
        .filter(|index| match declared[*index] {
            DeclaredClass::Integer | DeclaredClass::Decimal => true,
            DeclaredClass::Unknown | DeclaredClass::Text => {
                non_null[*index] > 0 && numeric[*index] as f64 >= 0.95 * non_null[*index] as f64
            }
            _ => false,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrendFit {
    /// Change per second.
    pub slope: f64,
    /// Value at the first point's time.
    pub intercept: f64,
    pub r_squared: f64,
    pub n: usize,
    pub residual_stddev: f64,
    /// Always `"naive_linear"`: ordinary least squares on time, with no
    /// seasonality, so callers label the forecast as naive.
    pub method: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ForecastPoint {
    pub t: i64,
    pub label: String,
    pub value: f64,
    pub lower: f64,
    pub upper: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Forecast {
    pub fit: TrendFit,
    pub confidence: f64,
    pub points: Vec<ForecastPoint>,
}

/// Ordinary least squares of value on time; `None` with fewer than two
/// finite points or a single instant.
pub fn linear_trend(points: &[SeriesPoint]) -> Option<TrendFit> {
    let finite: Vec<&SeriesPoint> = points.iter().filter(|p| p.value.is_finite()).collect();
    if finite.len() < 2 {
        return None;
    }
    let t0 = finite[0].t;
    let xs: Vec<f64> = finite.iter().map(|p| (p.t - t0) as f64).collect();
    let ys: Vec<f64> = finite.iter().map(|p| p.value).collect();
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let mean_y = ys.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    if sxx == 0.0 {
        return None;
    }
    let sxy: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum();
    let slope = sxy / sxx;
    let intercept = mean_y - slope * mean_x;
    let ss_res: f64 = xs
        .iter()
        .zip(&ys)
        .map(|(x, y)| (y - (intercept + slope * x)).powi(2))
        .sum();
    let ss_tot: f64 = ys.iter().map(|y| (y - mean_y).powi(2)).sum();
    let r_squared = if ss_tot == 0.0 {
        1.0
    } else {
        1.0 - ss_res / ss_tot
    };
    let residual_stddev = if finite.len() > 2 {
        (ss_res / (n - 2.0)).sqrt()
    } else {
        0.0
    };
    Some(TrendFit {
        slope,
        intercept,
        r_squared,
        n: finite.len(),
        residual_stddev,
        method: "naive_linear".to_string(),
    })
}

/// Extend a series `horizon` buckets with the linear trend and a prediction
/// interval at `confidence` (e.g. 0.95). Needs three or more points for an
/// interval; with two the interval collapses to the line.
pub fn forecast(series: &TimeSeries, horizon: usize, confidence: f64) -> Option<Forecast> {
    let fit = linear_trend(&series.points)?;
    let finite: Vec<&SeriesPoint> = series
        .points
        .iter()
        .filter(|p| p.value.is_finite())
        .collect();
    let t0 = finite[0].t;
    let xs: Vec<f64> = finite.iter().map(|p| (p.t - t0) as f64).collect();
    let n = xs.len() as f64;
    let mean_x = xs.iter().sum::<f64>() / n;
    let sxx: f64 = xs.iter().map(|x| (x - mean_x).powi(2)).sum();
    let critical = if fit.n > 2 {
        t_critical(fit.n - 2, confidence)
    } else {
        0.0
    };
    let mut start = from_epoch_seconds(series.points.last()?.t)?;
    let mut points = Vec::with_capacity(horizon);
    for _ in 0..horizon {
        start = next_bucket(start, series.grain);
        let t = epoch_seconds(start);
        let x = (t - t0) as f64;
        let value = fit.intercept + fit.slope * x;
        let spread =
            critical * fit.residual_stddev * (1.0 + 1.0 / n + (x - mean_x).powi(2) / sxx).sqrt();
        points.push(ForecastPoint {
            t,
            label: bucket_label(start, series.grain),
            value,
            lower: value - spread,
            upper: value + spread,
        });
    }
    Some(Forecast {
        fit,
        confidence,
        points,
    })
}

/// Two-sided Student-t critical value, from the normal quantile with a
/// Cornish-Fisher correction (within ~1% of tables for df ≥ 3).
fn t_critical(df: usize, confidence: f64) -> f64 {
    let z = normal_quantile(0.5 + confidence.clamp(0.5, 0.9999) / 2.0);
    let df = df.max(1) as f64;
    let z3 = z.powi(3);
    let z5 = z.powi(5);
    z + (z3 + z) / (4.0 * df) + (5.0 * z5 + 16.0 * z3 + 3.0 * z) / (96.0 * df * df)
}

/// Acklam's rational approximation to the standard normal quantile.
fn normal_quantile(p: f64) -> f64 {
    const A: [f64; 6] = [
        -3.969_683_028_665_376e1,
        2.209_460_984_245_205e2,
        -2.759_285_104_469_687e2,
        1.383_577_518_672_69e2,
        -3.066_479_806_614_716e1,
        2.506_628_277_459_239,
    ];
    const B: [f64; 5] = [
        -5.447_609_879_822_406e1,
        1.615_858_368_580_409e2,
        -1.556_989_798_598_866e2,
        6.680_131_188_771_972e1,
        -1.328_068_155_288_572e1,
    ];
    const C: [f64; 6] = [
        -7.784_894_002_430_293e-3,
        -3.223_964_580_411_365e-1,
        -2.400_758_277_161_838,
        -2.549_732_539_343_734,
        4.374_664_141_464_968,
        2.938_163_982_698_783,
    ];
    const D: [f64; 4] = [
        7.784_695_709_041_462e-3,
        3.224_671_290_700_398e-1,
        2.445_134_137_142_996,
        3.754_408_661_907_416,
    ];
    let p = p.clamp(1e-12, 1.0 - 1e-12);
    let low = 0.024_25;
    if p < low {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5])
            / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p <= 1.0 - low {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    } else {
        -normal_quantile(1.0 - p)
    }
}

#[cfg(test)]
#[path = "timeseries_tests.rs"]
mod tests;
