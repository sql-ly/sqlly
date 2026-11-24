//! Chart transforms: grouping rows into categories and series, date grain,
//! aggregation, null handling, sorting and top-N. Pure and deterministic
//! (insertion-ordered maps, total orderings on floats) so the same rows
//! always produce the same picture.

use std::collections::{BTreeSet, HashMap};

use sqlly_core::CellValue;

use crate::report::chart::{Aggregate, ChartSpec, ChartSpecError, Encoding, NullTreatment, SortBy};
use crate::report::doc::TableBlock;
use crate::report::format::{
    ColumnKind, ValueFormat, cell_instant, cell_number, cell_temporal, format_cell,
    format_temporal, grain_label, truncate_to_grain,
};
use crate::report::labels::ReportLabels;

/// Most series a chart draws; the rest fold into "Other".
pub const MAX_SERIES: usize = 24;
/// Most categories a chart draws; the rest fold into "Other".
pub const MAX_CATEGORIES: usize = 2000;
/// Most groups a box plot draws; the rest fold into "Other".
pub const MAX_GROUPS: usize = 200;
/// Most rows and columns a heatmap draws; the rest fold into "Other".
pub const MAX_MATRIX: usize = 400;

#[derive(Debug, Clone, PartialEq)]
pub enum ChartError {
    Invalid(Vec<ChartSpecError>),
    NoData,
    Withheld,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: String,
    pub values: Vec<Option<f64>>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CategoryData {
    pub categories: Vec<String>,
    /// Each category as a cross-filter labels it (`None` for "Other" and
    /// for row-number categories), so a click on its mark can select it.
    pub keys: Vec<Option<String>>,
    /// Bucket start times (seconds) when x is temporal.
    pub times: Option<Vec<f64>>,
    pub series: Vec<Series>,
    /// Expected range per category, from the anomaly band columns.
    pub band: Option<ExpectedRange>,
    /// Categories past [`MAX_CATEGORIES`] that were folded into "Other".
    pub capped: usize,
}

/// Lower and upper bounds per category.
pub type ExpectedRange = (Vec<Option<f64>>, Vec<Option<f64>>);

impl CategoryData {
    pub fn totals(&self) -> Vec<f64> {
        (0..self.categories.len())
            .map(|i| self.series.iter().filter_map(|s| s.values[i]).sum())
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.categories.is_empty()
            || self
                .series
                .iter()
                .all(|s| s.values.iter().all(Option::is_none))
    }
}

#[derive(Default, Clone)]
struct Acc {
    sum: f64,
    values: Vec<f64>,
    count: usize,
    distinct: BTreeSet<DistinctKey>,
}

/// A value as `CountDistinct` tells values apart: numbers by value (so
/// 1.2 and 1.4 stay two values whatever the label rounds them to), the rest
/// by their text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum DistinctKey {
    Number(u64),
    Text(String),
}

impl Acc {
    /// `other`'s rows added to these, so an aggregate over both is computed
    /// from the rows rather than from two finished results.
    fn merge(&mut self, other: &Acc) {
        self.sum += other.sum;
        self.values.extend_from_slice(&other.values);
        self.count += other.count;
        self.distinct.extend(other.distinct.iter().cloned());
    }

    fn finish(&self, aggregate: Aggregate) -> Option<f64> {
        match aggregate {
            Aggregate::Count => Some(self.count as f64),
            Aggregate::CountDistinct => Some(self.distinct.len() as f64),
            _ if self.values.is_empty() => None,
            Aggregate::None | Aggregate::Sum => Some(self.sum),
            Aggregate::Avg => Some(self.sum / self.values.len() as f64),
            Aggregate::Min => self.values.iter().copied().reduce(f64::min),
            Aggregate::Max => self.values.iter().copied().reduce(f64::max),
            Aggregate::Median => Some(quantile(&self.values, 0.5)),
        }
    }
}

/// Type-7 quantile (linear interpolation) of unsorted values.
pub fn quantile(values: &[f64], q: f64) -> f64 {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted.sort_by(f64::total_cmp);
    let pos = (sorted.len() - 1) as f64 * q.clamp(0.0, 1.0);
    let lo = pos.floor() as usize;
    let hi = pos.ceil() as usize;
    sorted[lo] + (sorted[hi] - sorted[lo]) * (pos - lo as f64)
}

pub struct Ctx<'a> {
    pub spec: &'a ChartSpec,
    pub table: &'a TableBlock,
    pub labels: &'a ReportLabels,
    /// Categories (or groups, or matrix rows and columns) folded into
    /// "Other" because there were too many to draw.
    pub capped: std::cell::Cell<usize>,
}

impl<'a> Ctx<'a> {
    pub fn new(spec: &'a ChartSpec, table: &'a TableBlock, labels: &'a ReportLabels) -> Self {
        Self {
            spec,
            table,
            labels,
            capped: std::cell::Cell::new(0),
        }
    }

    pub fn check(&self) -> Result<(), ChartError> {
        if self.table.data_withheld {
            return Err(ChartError::Withheld);
        }
        self.spec
            .validate(&self.table.columns)
            .map_err(ChartError::Invalid)?;
        if self.table.rows.is_empty() {
            return Err(ChartError::NoData);
        }
        Ok(())
    }

    pub fn index(&self, e: &Encoding) -> usize {
        self.table.column_index(&e.column).unwrap_or(0)
    }

    pub fn kind(&self, e: &Encoding) -> ColumnKind {
        self.table.columns[self.index(e)].kind()
    }

    pub fn format(&self, e: &Encoding) -> &ValueFormat {
        &self.table.columns[self.index(e)].format
    }

    /// The display label of a value, `(null)` for nulls.
    pub fn label_of(&self, e: &Encoding, value: &CellValue) -> String {
        if matches!(value, CellValue::Null) {
            return self.labels.null_value.clone();
        }
        let col = &self.table.columns[self.index(e)];
        let kind = col.kind();
        if kind.is_temporal()
            && let Some(t) = cell_temporal(value)
        {
            return format_temporal(t, &col.format);
        }
        format_cell(value, kind, &col.format)
    }

    fn measure(&self, e: &Encoding, value: &CellValue) -> Option<f64> {
        cell_number(value, self.kind(e))
    }

    fn distinct_key(&self, e: &Encoding, value: &CellValue) -> DistinctKey {
        let kind = self.kind(e);
        if (kind.is_numeric() || matches!(value, CellValue::I64(_) | CellValue::F64(_)))
            && let Some(n) = cell_number(value, kind)
        {
            let n = if n == 0.0 { 0.0 } else { n };
            return DistinctKey::Number(n.to_bits());
        }
        if let CellValue::String(text) = value {
            return DistinctKey::Text(text.clone());
        }
        DistinctKey::Text(self.label_of(e, value))
    }

    /// Records the most categories any transform had to fold away.
    fn note_capped(&self, count: usize) {
        self.capped.set(self.capped.get().max(count));
    }

    fn aggregate(&self, e: &Encoding) -> Aggregate {
        e.aggregate.unwrap_or(self.spec.aggregate)
    }

    /// One number per measure row for kinds that plot raw values.
    pub fn values(&self, e: &Encoding) -> Vec<f64> {
        let i = self.index(e);
        let kind = self.kind(e);
        self.table
            .rows
            .iter()
            .filter_map(|r| r.get(i).and_then(|v| cell_number(v, kind)))
            .collect()
    }

    /// Rows grouped by x (and the series column), measures aggregated.
    pub fn categories(&self) -> CategoryData {
        let spec = self.spec;
        let x = spec.x.as_ref();
        let x_index = x.map(|e| self.index(e));
        let x_kind = x.map(|e| self.kind(e));
        let temporal = x_kind.is_some_and(ColumnKind::is_temporal);
        let numeric = x_kind.is_some_and(ColumnKind::is_numeric);
        let series_index = spec.series.as_ref().map(|e| self.index(e));

        let mut cat_keys: Vec<(String, f64)> = Vec::new();
        let mut filter_keys: Vec<Option<String>> = Vec::new();
        let mut cat_lookup: HashMap<String, usize> = HashMap::new();
        let mut series_names: Vec<String> = Vec::new();
        let mut series_lookup: HashMap<String, usize> = HashMap::new();
        let mut cells: HashMap<(usize, usize), Acc> = HashMap::new();
        let mut band_acc: HashMap<usize, (Acc, Acc)> = HashMap::new();

        for (row_number, row) in self.table.rows.iter().enumerate() {
            let (label, sort_key) = match (x, x_index) {
                (Some(e), Some(i)) => {
                    let value = row.get(i).unwrap_or(&CellValue::Null);
                    if temporal {
                        match cell_instant(value) {
                            Some(t) => match spec.date_grain {
                                Some(grain) => {
                                    let start = truncate_to_grain(t.as_datetime(), grain);
                                    (
                                        grain_label(start, grain),
                                        start.and_utc().timestamp() as f64,
                                    )
                                }
                                None => (self.label_of(e, value), t.timestamp()),
                            },
                            None => (self.label_of(e, value), f64::INFINITY),
                        }
                    } else if numeric {
                        let n = cell_number(value, x_kind.unwrap_or(ColumnKind::Float));
                        (self.label_of(e, value), n.unwrap_or(f64::INFINITY))
                    } else {
                        (self.label_of(e, value), row_number as f64)
                    }
                }
                _ => ((row_number + 1).to_string(), row_number as f64),
            };
            let cat = match cat_lookup.get(&label) {
                Some(&c) => c,
                None => {
                    cat_keys.push((label.clone(), sort_key));
                    filter_keys.push(x_index.map(|i| {
                        crate::notebook::cells::spec::category_label(
                            row.get(i).unwrap_or(&CellValue::Null),
                            spec.date_grain,
                        )
                    }));
                    cat_lookup.insert(label, cat_keys.len() - 1);
                    cat_keys.len() - 1
                }
            };
            let split = series_index.map(|i| {
                let value = row.get(i).unwrap_or(&CellValue::Null);
                spec.series
                    .as_ref()
                    .map(|e| self.label_of(e, value))
                    .unwrap_or_default()
            });
            for measure in &spec.y {
                let name = match &split {
                    Some(s) if spec.y.len() > 1 => format!("{} · {s}", measure.label()),
                    Some(s) => s.clone(),
                    None => measure.label().to_string(),
                };
                let s = match series_lookup.get(&name) {
                    Some(&s) => s,
                    None => {
                        series_names.push(name.clone());
                        series_lookup.insert(name, series_names.len() - 1);
                        series_names.len() - 1
                    }
                };
                let value = row.get(self.index(measure)).unwrap_or(&CellValue::Null);
                let acc = cells.entry((cat, s)).or_default();
                match self.aggregate(measure) {
                    Aggregate::Count => {
                        if !matches!(value, CellValue::Null) {
                            acc.count += 1;
                        }
                    }
                    Aggregate::CountDistinct => {
                        if !matches!(value, CellValue::Null) {
                            acc.distinct.insert(self.distinct_key(measure, value));
                        }
                    }
                    _ => {
                        let n = match (self.measure(measure, value), spec.nulls) {
                            (Some(n), _) => Some(n),
                            (None, NullTreatment::Zero) => Some(0.0),
                            (None, _) => None,
                        };
                        if let Some(n) = n {
                            acc.sum += n;
                            acc.values.push(n);
                            acc.count += 1;
                        }
                    }
                }
            }
            if let Some(band) = &spec.anomaly_band {
                let lo = self.table.column_index(&band.lower);
                let hi = self.table.column_index(&band.upper);
                let entry = band_acc.entry(cat).or_default();
                for (index, acc) in [(lo, &mut entry.0), (hi, &mut entry.1)] {
                    if let Some(n) = index
                        .and_then(|i| row.get(i))
                        .and_then(|v| cell_number(v, ColumnKind::Float))
                    {
                        acc.sum += n;
                        acc.values.push(n);
                    }
                }
            }
        }

        let mut aggregates: Vec<Aggregate> = series_names
            .iter()
            .map(|name| {
                spec.y
                    .iter()
                    .find(|m| name == m.label() || name.starts_with(&format!("{} · ", m.label())))
                    .map(|m| self.aggregate(m))
                    .unwrap_or(spec.aggregate)
            })
            .collect();
        // Series past the cap fold into "Other" by merging their rows, so
        // its average or maximum is over those rows.
        if series_names.len() > MAX_SERIES {
            let keep = MAX_SERIES - 1;
            let folded = &aggregates[keep..];
            let aggregate = if folded.iter().all(|a| *a == folded[0]) {
                folded[0]
            } else {
                spec.aggregate
            };
            for c in 0..cat_keys.len() {
                let mut merged: Option<Acc> = None;
                for s in keep..series_names.len() {
                    if let Some(acc) = cells.remove(&(c, s)) {
                        merged.get_or_insert_with(Acc::default).merge(&acc);
                    }
                }
                if let Some(acc) = merged {
                    cells.insert((c, keep), acc);
                }
            }
            series_names.truncate(keep);
            series_names.push(self.labels.other.clone());
            aggregates.truncate(keep);
            aggregates.push(aggregate);
        }
        let mut series: Vec<Series> = series_names
            .iter()
            .enumerate()
            .map(|(s, name)| Series {
                name: name.clone(),
                values: (0..cat_keys.len())
                    .map(|c| cells.get(&(c, s)).and_then(|a| a.finish(aggregates[s])))
                    .collect(),
            })
            .collect();
        if spec.nulls == NullTreatment::Zero {
            for s in &mut series {
                for v in &mut s.values {
                    v.get_or_insert(0.0);
                }
            }
        }
        let band = spec.anomaly_band.as_ref().map(|_| {
            let pick = |c: usize, upper: bool| {
                band_acc.get(&c).and_then(|(lo, hi)| {
                    let acc = if upper { hi } else { lo };
                    acc.finish(Aggregate::Avg)
                })
            };
            (
                (0..cat_keys.len()).map(|c| pick(c, false)).collect(),
                (0..cat_keys.len()).map(|c| pick(c, true)).collect(),
            )
        });
        let mut data = CategoryData {
            categories: cat_keys.iter().map(|(l, _)| l.clone()).collect(),
            keys: filter_keys,
            times: temporal.then(|| cat_keys.iter().map(|(_, t)| *t).collect()),
            series,
            band,
            capped: 0,
        };

        let mut order: Vec<usize> = (0..data.categories.len()).collect();
        let totals = data.totals();
        match spec.sort.by {
            SortBy::Natural if temporal || numeric => {
                order.sort_by(|a, b| cat_keys[*a].1.total_cmp(&cat_keys[*b].1).then(a.cmp(b)));
            }
            SortBy::Natural => {}
            SortBy::Category => order.sort_by(|a, b| {
                natural_cmp(&data.categories[*a], &data.categories[*b]).then(a.cmp(b))
            }),
            SortBy::Value => order.sort_by(|a, b| totals[*a].total_cmp(&totals[*b]).then(a.cmp(b))),
        }
        if spec.sort.descending {
            order.reverse();
        }
        let mut dropped: Vec<usize> = Vec::new();
        if let Some(top) = spec.top_n
            && order.len() > top.n
        {
            let mut ranked = order.clone();
            ranked.sort_by(|a, b| totals[*b].total_cmp(&totals[*a]).then(a.cmp(b)));
            let keep: BTreeSet<usize> = ranked.iter().take(top.n).copied().collect();
            if top.other {
                dropped = order
                    .iter()
                    .copied()
                    .filter(|c| !keep.contains(c))
                    .collect();
            }
            order.retain(|c| keep.contains(c));
        }
        if order.len() > MAX_CATEGORIES {
            let rest = order.split_off(MAX_CATEGORIES - 1);
            data.capped = rest.len();
            self.note_capped(rest.len());
            dropped.extend(rest);
        }
        let other: Option<Vec<Option<f64>>> = (!dropped.is_empty()).then(|| {
            (0..data.series.len())
                .map(|s| {
                    let mut merged: Option<Acc> = None;
                    for c in &dropped {
                        if let Some(acc) = cells.get(&(*c, s)) {
                            merged.get_or_insert_with(Acc::default).merge(acc);
                        }
                    }
                    let value = merged.and_then(|acc| acc.finish(aggregates[s]));
                    match (value, spec.nulls) {
                        (None, NullTreatment::Zero) => Some(0.0),
                        (value, _) => value,
                    }
                })
                .collect()
        });
        let capped = data.capped;
        data = reorder(data, &order);
        data.capped = capped;
        if let Some(other) = other {
            data.categories.push(self.labels.other.clone());
            data.keys.push(None);
            if let Some(times) = &mut data.times {
                times.push(f64::INFINITY);
            }
            for (s, v) in data.series.iter_mut().zip(other) {
                s.values.push(v);
            }
            if let Some((lo, hi)) = &mut data.band {
                lo.push(None);
                hi.push(None);
            }
        }
        data
    }
}

fn reorder(data: CategoryData, order: &[usize]) -> CategoryData {
    CategoryData {
        categories: order.iter().map(|&i| data.categories[i].clone()).collect(),
        keys: order.iter().map(|&i| data.keys[i].clone()).collect(),
        times: data.times.map(|t| order.iter().map(|&i| t[i]).collect()),
        series: data
            .series
            .into_iter()
            .map(|s| Series {
                name: s.name,
                values: order.iter().map(|&i| s.values[i]).collect(),
            })
            .collect(),
        band: data.band.map(|(lo, hi)| {
            (
                order.iter().map(|&i| lo[i]).collect(),
                order.iter().map(|&i| hi[i]).collect(),
            )
        }),
        capped: data.capped,
    }
}

/// Orders `a2` before `a10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let chunks = |s: &str| {
        let mut out: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let digit = c.is_ascii_digit();
            match out.last_mut() {
                Some((d, text)) if *d == digit => text.push(c),
                _ => out.push((digit, c.to_string())),
            }
        }
        out
    };
    let (ca, cb) = (chunks(a), chunks(b));
    for ((da, ta), (db, tb)) in ca.iter().zip(cb.iter()) {
        let ord = if *da && *db {
            let (na, nb) = (ta.trim_start_matches('0'), tb.trim_start_matches('0'));
            na.len().cmp(&nb.len()).then_with(|| na.cmp(nb))
        } else {
            ta.to_lowercase().cmp(&tb.to_lowercase())
        };
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
    }
    ca.len().cmp(&cb.len()).then_with(|| a.cmp(b))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
    pub size: Option<f64>,
    pub group: usize,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct XyData {
    pub points: Vec<Point>,
    pub groups: Vec<String>,
    pub x_temporal: bool,
}

/// Most points a scatter draws.
pub const MAX_POINTS: usize = 20_000;

impl Ctx<'_> {
    pub fn points(&self) -> XyData {
        let spec = self.spec;
        let Some(x) = spec.x.as_ref() else {
            return XyData::default();
        };
        let xi = self.index(x);
        let x_kind = self.kind(x);
        let x_temporal = x_kind.is_temporal();
        let mut groups: Vec<String> = Vec::new();
        let mut lookup: HashMap<String, usize> = HashMap::new();
        let mut points = Vec::new();
        for row in &self.table.rows {
            let xv = row.get(xi).unwrap_or(&CellValue::Null);
            let Some(xn) = (if x_temporal {
                cell_instant(xv).map(|t| t.timestamp())
            } else {
                cell_number(xv, x_kind)
            }) else {
                continue;
            };
            for measure in &spec.y {
                let Some(yn) = cell_number(
                    row.get(self.index(measure)).unwrap_or(&CellValue::Null),
                    self.kind(measure),
                ) else {
                    continue;
                };
                let group_name = match &spec.color {
                    Some(c) => self.label_of(c, row.get(self.index(c)).unwrap_or(&CellValue::Null)),
                    None => measure.label().to_string(),
                };
                let group = match lookup.get(&group_name) {
                    Some(&g) => g,
                    None => {
                        groups.push(group_name.clone());
                        lookup.insert(group_name, groups.len() - 1);
                        groups.len() - 1
                    }
                };
                let size = spec.size.as_ref().and_then(|s| {
                    cell_number(
                        row.get(self.index(s)).unwrap_or(&CellValue::Null),
                        self.kind(s),
                    )
                });
                points.push(Point {
                    x: xn,
                    y: yn,
                    size,
                    group: group.min(MAX_SERIES - 1),
                });
                if points.len() >= MAX_POINTS {
                    break;
                }
            }
            if points.len() >= MAX_POINTS {
                break;
            }
        }
        if groups.len() > MAX_SERIES {
            groups.truncate(MAX_SERIES - 1);
            groups.push(self.labels.other.clone());
        }
        XyData {
            points,
            groups,
            x_temporal,
        }
    }

    /// Values grouped by the x column (one unnamed group without x).
    pub fn groups(&self, measure: &Encoding) -> Vec<(String, Vec<f64>)> {
        let mi = self.index(measure);
        let kind = self.kind(measure);
        let mut out: Vec<(String, Vec<f64>)> = Vec::new();
        let mut lookup: HashMap<String, usize> = HashMap::new();
        for row in &self.table.rows {
            let Some(v) = row.get(mi).and_then(|v| cell_number(v, kind)) else {
                continue;
            };
            let label = match &self.spec.x {
                Some(x) => self.label_of(x, row.get(self.index(x)).unwrap_or(&CellValue::Null)),
                None => measure.label().to_string(),
            };
            let g = match lookup.get(&label) {
                Some(&g) => g,
                None => {
                    out.push((label.clone(), Vec::new()));
                    lookup.insert(label, out.len() - 1);
                    out.len() - 1
                }
            };
            out[g].1.push(v);
        }
        if self.spec.sort.by == SortBy::Category {
            out.sort_by(|a, b| natural_cmp(&a.0, &b.0));
        }
        if self.spec.sort.descending {
            out.reverse();
        }
        if out.len() > MAX_GROUPS {
            let rest = out.split_off(MAX_GROUPS - 1);
            self.note_capped(rest.len());
            let values = rest.into_iter().flat_map(|(_, v)| v).collect();
            out.push((self.labels.other.clone(), values));
        }
        out
    }

    /// A matrix keyed by x (columns) and the series column (rows).
    pub fn matrix(&self) -> Matrix {
        let mut spec = self.spec.clone();
        let rows_enc = spec.series.take();
        let x = spec.x.clone();
        let mut cols: Vec<(String, f64)> = Vec::new();
        let mut rows: Vec<(String, f64)> = Vec::new();
        let mut col_lookup: HashMap<String, usize> = HashMap::new();
        let mut row_lookup: HashMap<String, usize> = HashMap::new();
        let mut cells: HashMap<(usize, usize), Acc> = HashMap::new();
        let Some(measure) = spec.y.first().cloned() else {
            return Matrix::default();
        };
        let key = |e: &Encoding, v: &CellValue, n: usize| -> (String, f64) {
            let kind = self.kind(e);
            let sort = if kind.is_temporal() {
                cell_instant(v)
                    .map(|t| t.timestamp())
                    .unwrap_or(f64::INFINITY)
            } else if kind.is_numeric() {
                cell_number(v, kind).unwrap_or(f64::INFINITY)
            } else {
                n as f64
            };
            let label = match (kind.is_temporal(), self.spec.date_grain, cell_instant(v)) {
                (true, Some(g), Some(t)) => grain_label(truncate_to_grain(t.as_datetime(), g), g),
                _ => self.label_of(e, v),
            };
            (label, sort)
        };
        for (n, row) in self.table.rows.iter().enumerate() {
            let get = |e: &Encoding| row.get(self.index(e)).unwrap_or(&CellValue::Null);
            let (Some(xe), Some(re)) = (&x, &rows_enc) else {
                break;
            };
            let (cl, cs) = key(xe, get(xe), n);
            let (rl, rs) = key(re, get(re), n);
            let c = *col_lookup.entry(cl.clone()).or_insert_with(|| {
                cols.push((cl, cs));
                cols.len() - 1
            });
            let r = *row_lookup.entry(rl.clone()).or_insert_with(|| {
                rows.push((rl, rs));
                rows.len() - 1
            });
            let acc = cells.entry((r, c)).or_default();
            let value = get(&measure);
            match self.aggregate(&measure) {
                Aggregate::Count => acc.count += usize::from(!matches!(value, CellValue::Null)),
                Aggregate::CountDistinct => {
                    if !matches!(value, CellValue::Null) {
                        acc.distinct.insert(self.distinct_key(&measure, value));
                    }
                }
                _ => {
                    if let Some(v) = self.measure(&measure, value) {
                        acc.sum += v;
                        acc.values.push(v);
                        acc.count += 1;
                    }
                }
            }
        }
        let aggregate = self.aggregate(&measure);
        let sort_axis = |v: &mut Vec<(String, f64)>| -> Vec<usize> {
            let mut order: Vec<usize> = (0..v.len()).collect();
            order.sort_by(|a, b| v[*a].1.total_cmp(&v[*b].1).then(a.cmp(b)));
            order
        };
        let col_order = sort_axis(&mut cols);
        let row_order = sort_axis(&mut rows);
        // Past the cap, the remaining rows (and columns) share one "Other"
        // line whose cells merge the folded rows' values.
        let fold = |order: Vec<usize>| -> (Vec<usize>, Vec<usize>) {
            if order.len() <= MAX_MATRIX {
                return (order, Vec::new());
            }
            let mut order = order;
            let rest = order.split_off(MAX_MATRIX - 1);
            (order, rest)
        };
        let (col_order, col_rest) = fold(col_order);
        let (row_order, row_rest) = fold(row_order);
        self.note_capped(col_rest.len().max(row_rest.len()));
        let mut col_groups: Vec<Vec<usize>> = col_order.iter().map(|&c| vec![c]).collect();
        let mut column_names: Vec<String> = col_order.iter().map(|&c| cols[c].0.clone()).collect();
        if !col_rest.is_empty() {
            col_groups.push(col_rest);
            column_names.push(self.labels.other.clone());
        }
        let mut row_groups: Vec<Vec<usize>> = row_order.iter().map(|&r| vec![r]).collect();
        let mut row_names: Vec<String> = row_order.iter().map(|&r| rows[r].0.clone()).collect();
        if !row_rest.is_empty() {
            row_groups.push(row_rest);
            row_names.push(self.labels.other.clone());
        }
        let cell_value = |rs: &[usize], cs: &[usize]| -> Option<f64> {
            if let ([r], [c]) = (rs, cs) {
                return cells.get(&(*r, *c)).and_then(|a| a.finish(aggregate));
            }
            let mut merged: Option<Acc> = None;
            for r in rs {
                for c in cs {
                    if let Some(acc) = cells.get(&(*r, *c)) {
                        merged.get_or_insert_with(Acc::default).merge(acc);
                    }
                }
            }
            merged.and_then(|a| a.finish(aggregate))
        };
        Matrix {
            columns: column_names,
            rows: row_names,
            values: row_groups
                .iter()
                .map(|rs| col_groups.iter().map(|cs| cell_value(rs, cs)).collect())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Matrix {
    pub columns: Vec<String>,
    pub rows: Vec<String>,
    /// `values[row][column]`.
    pub values: Vec<Vec<Option<f64>>>,
}

impl Matrix {
    pub fn range(&self) -> Option<(f64, f64)> {
        let vals = self.values.iter().flatten().flatten().copied();
        let min = vals.clone().reduce(f64::min)?;
        let max = vals.reduce(f64::max)?;
        Some((min, max))
    }
}

/// Pearson correlation of two equal-length samples.
pub fn pearson(a: &[f64], b: &[f64]) -> Option<f64> {
    let n = a.len().min(b.len());
    if n < 2 {
        return None;
    }
    let (ma, mb) = (
        a[..n].iter().sum::<f64>() / n as f64,
        b[..n].iter().sum::<f64>() / n as f64,
    );
    let mut cov = 0.0;
    let mut va = 0.0;
    let mut vb = 0.0;
    for i in 0..n {
        let (da, db) = (a[i] - ma, b[i] - mb);
        cov += da * db;
        va += da * da;
        vb += db * db;
    }
    let denom = (va * vb).sqrt();
    (denom > 0.0).then(|| (cov / denom).clamp(-1.0, 1.0))
}

impl Ctx<'_> {
    pub fn correlation(&self) -> Matrix {
        let measures = &self.spec.y;
        let names: Vec<String> = measures.iter().map(|m| m.label().to_string()).collect();
        let indices: Vec<(usize, ColumnKind)> = measures
            .iter()
            .map(|m| (self.index(m), self.kind(m)))
            .collect();
        let mut values = vec![vec![None; measures.len()]; measures.len()];
        for i in 0..measures.len() {
            for j in 0..measures.len() {
                if j < i {
                    values[i][j] = values[j][i];
                    continue;
                }
                let mut a = Vec::new();
                let mut b = Vec::new();
                for row in &self.table.rows {
                    let va = row
                        .get(indices[i].0)
                        .and_then(|v| cell_number(v, indices[i].1));
                    let vb = row
                        .get(indices[j].0)
                        .and_then(|v| cell_number(v, indices[j].1));
                    if let (Some(va), Some(vb)) = (va, vb) {
                        a.push(va);
                        b.push(vb);
                    }
                }
                // A column with no spread has no correlation, not even with
                // itself.
                values[i][j] = pearson(&a, &b).map(|r| if i == j { 1.0 } else { r });
            }
        }
        Matrix {
            columns: names.clone(),
            rows: names,
            values,
        }
    }

    /// Per-day totals for a calendar heatmap, sorted by date.
    pub fn daily(&self) -> Vec<(chrono::NaiveDate, f64)> {
        let (Some(x), Some(measure)) = (self.spec.x.as_ref(), self.spec.y.first()) else {
            return Vec::new();
        };
        let xi = self.index(x);
        let mut by_day: std::collections::BTreeMap<chrono::NaiveDate, Acc> = Default::default();
        for row in &self.table.rows {
            let Some(t) = row.get(xi).and_then(cell_instant) else {
                continue;
            };
            let value = row.get(self.index(measure)).unwrap_or(&CellValue::Null);
            let acc = by_day.entry(t.as_datetime().date()).or_default();
            match self.aggregate(measure) {
                Aggregate::Count => acc.count += usize::from(!matches!(value, CellValue::Null)),
                _ => {
                    if let Some(v) = self.measure(measure, value) {
                        acc.sum += v;
                        acc.values.push(v);
                        acc.count += 1;
                    }
                }
            }
        }
        let aggregate = self.aggregate(measure);
        by_day
            .into_iter()
            .filter_map(|(d, acc)| acc.finish(aggregate).map(|v| (d, v)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::chart::{ChartKind, DateGrain, Sort, TopN};
    use crate::report::doc::TableColumn;

    fn table() -> TableBlock {
        let row = |d: &str, r: &str, v: &str| {
            vec![
                CellValue::String(d.into()),
                CellValue::String(r.into()),
                CellValue::String(v.into()),
            ]
        };
        TableBlock::new(
            vec![
                TableColumn::new("day", "date"),
                TableColumn::new("region", "varchar"),
                TableColumn::new("revenue", "decimal(10,2)"),
            ],
            vec![
                row("2026-02-03", "West", "10.50"),
                row("2026-01-05", "East", "4"),
                row("2026-01-20", "West", "1.5"),
                row("2026-02-10", "East", "6"),
                vec![
                    CellValue::String("2026-03-01".into()),
                    CellValue::Null,
                    CellValue::Null,
                ],
            ],
        )
    }

    #[test]
    fn grain_groups_and_sorts_by_date() {
        let mut spec = ChartSpec::new(ChartKind::Bar)
            .x("day")
            .y("revenue")
            .series("region");
        spec.date_grain = Some(DateGrain::Month);
        let labels = ReportLabels::default();
        let t = table();
        let data = Ctx::new(&spec, &t, &labels).categories();
        assert_eq!(data.categories, vec!["2026-01", "2026-02", "2026-03"]);
        assert_eq!(data.series[0].name, "West");
        assert_eq!(data.series[0].values, vec![Some(1.5), Some(10.5), None]);
        assert_eq!(data.series[1].values, vec![Some(4.0), Some(6.0), None]);
        assert_eq!(data.series[2].name, "(null)");
    }

    #[test]
    fn top_n_folds_the_rest_into_other() {
        let mut spec = ChartSpec::new(ChartKind::Bar).x("region").y("revenue");
        spec.sort = Sort {
            by: SortBy::Value,
            descending: true,
        };
        spec.top_n = Some(TopN { n: 1, other: true });
        spec.nulls = NullTreatment::Zero;
        let labels = ReportLabels::default();
        let t = table();
        let data = Ctx::new(&spec, &t, &labels).categories();
        assert_eq!(data.categories, vec!["West", "Other"]);
        assert_eq!(data.series[0].values, vec![Some(12.0), Some(10.0)]);
    }

    #[test]
    fn aggregates_are_exact() {
        let values = [3.0, 1.0, 2.0, 10.0];
        assert_eq!(quantile(&values, 0.5), 2.5);
        assert_eq!(quantile(&values, 0.0), 1.0);
        assert_eq!(pearson(&[1.0, 2.0, 3.0], &[2.0, 4.0, 6.0]), Some(1.0));
        assert_eq!(pearson(&[1.0, 1.0], &[2.0, 3.0]), None);
        assert_eq!(natural_cmp("a2", "a10"), std::cmp::Ordering::Less);
    }

    fn cat_table(rows: &[(&str, f64)]) -> TableBlock {
        TableBlock::new(
            vec![
                TableColumn::new("cat", "varchar"),
                TableColumn::new("v", "double"),
            ],
            rows.iter()
                .map(|(c, v)| vec![CellValue::String((*c).into()), CellValue::F64(*v)])
                .collect(),
        )
    }

    #[test]
    fn other_aggregates_the_folded_rows_not_their_results() {
        let t = cat_table(&[
            ("a", 100.0),
            ("a", 110.0),
            ("a", 120.0),
            ("b", 90.0),
            ("b", 95.0),
            ("b", 97.0),
            ("c", 10.0),
            ("c", 20.0),
            ("d", 30.0),
            ("e", 40.0),
        ]);
        let labels = ReportLabels::default();
        let other = |aggregate: Aggregate| {
            let mut spec = ChartSpec::new(ChartKind::Bar).x("cat").y("v");
            spec.aggregate = aggregate;
            spec.top_n = Some(TopN { n: 2, other: true });
            let data = Ctx::new(&spec, &t, &labels).categories();
            assert_eq!(data.categories, vec!["a", "b", "Other"]);
            data.series[0].values[2]
        };
        assert_eq!(other(Aggregate::Avg), Some(25.0));
        assert_eq!(other(Aggregate::Sum), Some(100.0));
        assert_eq!(other(Aggregate::Min), Some(10.0));
        assert_eq!(other(Aggregate::Max), Some(40.0));
        assert_eq!(other(Aggregate::Median), Some(25.0));
        assert_eq!(other(Aggregate::Count), Some(4.0));
        assert_eq!(other(Aggregate::CountDistinct), Some(4.0));
    }

    #[test]
    fn categories_past_the_cap_fold_into_other() {
        let names: Vec<String> = (0..2_500).map(|n| format!("c{n:04}")).collect();
        let rows: Vec<(&str, f64)> = names.iter().map(|n| (n.as_str(), 1.0)).collect();
        let t = cat_table(&rows);
        let labels = ReportLabels::default();
        let total = |data: &CategoryData| data.totals().iter().sum::<f64>();

        let mut spec = ChartSpec::new(ChartKind::Bar).x("cat").y("v");
        spec.top_n = Some(TopN { n: 10, other: true });
        let data = Ctx::new(&spec, &t, &labels).categories();
        assert_eq!(data.categories.len(), 11);
        assert_eq!(total(&data), 2_500.0, "no row is dropped before top-N");

        let spec = ChartSpec::new(ChartKind::Bar).x("cat").y("v");
        let ctx = Ctx::new(&spec, &t, &labels);
        let data = ctx.categories();
        assert_eq!(data.categories.len(), MAX_CATEGORIES);
        assert_eq!(data.categories.last().map(String::as_str), Some("Other"));
        assert_eq!(data.capped, 501);
        assert_eq!(ctx.capped.get(), 501);
        assert_eq!(total(&data), 2_500.0);

        let mut spec = ChartSpec::new(ChartKind::BoxPlot).x("cat").y("v");
        spec.sort = Sort::default();
        let ctx = Ctx::new(&spec, &t, &labels);
        let groups = ctx.groups(&spec.y[0]);
        assert_eq!(groups.len(), MAX_GROUPS);
        assert_eq!(
            groups.last().map(|g| g.1.len()),
            Some(2_500 - (MAX_GROUPS - 1))
        );
        assert_eq!(ctx.capped.get(), 2_500 - (MAX_GROUPS - 1));
    }

    #[test]
    fn count_distinct_counts_values_not_rounded_labels() {
        let mut t = cat_table(&[("a", 1.2), ("a", 1.4), ("a", 1.4)]);
        t.columns[1] = TableColumn::new("v", "double").with_format(ValueFormat::decimals(0));
        let mut spec = ChartSpec::new(ChartKind::Bar).x("cat").y("v");
        spec.aggregate = Aggregate::CountDistinct;
        let labels = ReportLabels::default();
        let data = Ctx::new(&spec, &t, &labels).categories();
        assert_eq!(data.series[0].values, vec![Some(2.0)]);
    }

    #[test]
    fn a_constant_column_has_no_self_correlation() {
        let t = TableBlock::new(
            vec![
                TableColumn::new("a", "double"),
                TableColumn::new("b", "double"),
            ],
            vec![
                vec![CellValue::F64(5.0), CellValue::F64(1.0)],
                vec![CellValue::F64(5.0), CellValue::F64(2.0)],
                vec![CellValue::F64(5.0), CellValue::F64(4.0)],
            ],
        );
        let spec = ChartSpec::new(ChartKind::CorrelationMatrix).y("a").y("b");
        let labels = ReportLabels::default();
        let m = Ctx::new(&spec, &t, &labels).correlation();
        assert_eq!(m.values[0][0], None);
        assert_eq!(m.values[1][1], Some(1.0));
        assert_eq!(m.values[0][1], None);
    }

    #[test]
    fn charts_and_analytics_bucket_zoned_times_alike() {
        use crate::analytics::time::{TimeGrain, bucket_label, floor_to_grain, parse_timestamp};
        let text = "2026-11-01 01:00:00+05:00";
        let t = TableBlock::new(
            vec![
                TableColumn::new("at", "timestamptz"),
                TableColumn::new("v", "double"),
            ],
            vec![vec![CellValue::String(text.into()), CellValue::F64(1.0)]],
        );
        let mut spec = ChartSpec::new(ChartKind::Bar).x("at").y("v");
        spec.date_grain = Some(DateGrain::Month);
        let labels = ReportLabels::default();
        let data = Ctx::new(&spec, &t, &labels).categories();
        let analytics = bucket_label(
            floor_to_grain(parse_timestamp(text).unwrap(), TimeGrain::Month),
            TimeGrain::Month,
        );
        assert_eq!(data.categories, vec!["2026-10"]);
        assert_eq!(analytics, "2026-10");
        let display = crate::report::format::parse_temporal(text).unwrap();
        assert_eq!(
            display.as_datetime().to_string(),
            "2026-11-01 01:00:00",
            "shown as written"
        );
    }
}
