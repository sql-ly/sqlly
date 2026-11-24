//! Column profiler: one streaming pass over a result, bounded memory per
//! column, and the same answer however the rows are batched.
//!
//! Exact figures are kept while they fit ([`ProfileOptions`] caps); past a
//! cap the profiler switches to the sketches in [`super::sketch`] and flags
//! the affected figures approximate. Memory per column is bounded by
//! `exact_distinct_cap × (value_display_chars + ~64 B)`, plus
//! `2 × exact_quantile_cap × 8 B`, plus the HyperLogLog registers and the
//! KLL sketch (a few thousand values).

use std::collections::HashMap;
use std::ops::ControlFlow;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ResultColumn};

use super::sketch::{HyperLogLog, KllSketch, SpaceSaving};
use super::time::{
    TimeGrain, bucket_label, bucket_starts, choose_grain, epoch_seconds, has_time_of_day,
    parse_timestamp,
};
use super::value::{
    DeclaredClass, cell_hash, currency_value, declared_class, format_float, is_integer_literal,
    numeric_literal, truncate_chars,
};
use super::{Completeness, OutputView};

/// Non-finite floats serialize as JSON `null`; read `null` back as NaN so a
/// stored profile (a drift baseline) always deserializes.
mod nan_as_null {
    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
        Ok(Option::<f64>::deserialize(deserializer)?.unwrap_or(f64::NAN))
    }
}

/// Caps and sketch sizes for one profiling pass.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProfileOptions {
    /// Distinct values counted exactly before switching to HyperLogLog and
    /// Space-Saving.
    pub exact_distinct_cap: usize,
    /// Numeric (and date) values kept for exact percentiles and histograms
    /// before switching to the KLL sketch.
    pub exact_quantile_cap: usize,
    /// Counters in the Space-Saving top-value table after the exact cap.
    pub heavy_hitter_capacity: usize,
    /// Most frequent values reported per column.
    pub top_k: usize,
    /// Numeric histogram bins.
    pub histogram_bins: usize,
    /// Most date-histogram buckets; the grain is chosen to fit.
    pub date_histogram_buckets: usize,
    pub kll_k: usize,
    pub hll_precision: u8,
    /// Characters kept of each reported value.
    pub value_display_chars: usize,
    /// Text longer than this is not parsed as JSON.
    pub json_parse_max_bytes: usize,
    /// At most this many distinct values for a text column to read as
    /// categorical.
    pub categorical_max_distinct: u64,
}

impl Default for ProfileOptions {
    fn default() -> Self {
        Self {
            exact_distinct_cap: 5_000,
            exact_quantile_cap: 10_000,
            heavy_hitter_capacity: 256,
            top_k: 10,
            histogram_bins: 20,
            date_histogram_buckets: 60,
            kll_k: 256,
            hll_precision: 12,
            value_display_chars: 64,
            json_parse_max_bytes: 16 * 1024,
            categorical_max_distinct: 50,
        }
    }
}

/// What a column's values look like, beyond the declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticType {
    Integer,
    Decimal,
    Boolean,
    Date,
    DateTime,
    Time,
    Uuid,
    Email,
    Url,
    Json,
    Ip,
    CurrencyLike,
    Categorical,
    FreeText,
    IdLike,
    Text,
    Binary,
    /// Every value is NULL or blank.
    Empty,
}

impl SemanticType {
    pub fn is_numeric(self) -> bool {
        matches!(
            self,
            SemanticType::Integer | SemanticType::Decimal | SemanticType::CurrencyLike
        )
    }

    pub fn is_temporal(self) -> bool {
        matches!(self, SemanticType::Date | SemanticType::DateTime)
    }
}

/// A count that may be an estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    pub value: u64,
    pub approximate: bool,
}

/// Whether a column can identify rows on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCandidate {
    /// No NULLs and no repeated value in a complete input.
    Unique,
    /// No repeat seen, but the input was incomplete or the distinct count
    /// is an estimate.
    LikelyUnique,
    NotUnique,
}

/// The direction the non-NULL values move in row order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Monotonicity {
    StrictlyIncreasing,
    Increasing,
    StrictlyDecreasing,
    Decreasing,
    Constant,
    NotMonotonic,
    /// Fewer than two comparable values.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueCount {
    pub value: String,
    pub count: u64,
    /// Upper bound on how much `count` overstates the true count (0 when
    /// exact).
    pub error: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Percentiles {
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p1: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p5: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p25: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p50: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p75: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p95: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub p99: f64,
    pub approximate: bool,
}

/// Equal-width numeric bins: `counts[i]` covers `edges[i]..edges[i+1]`
/// (the last bin includes its upper edge).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Histogram {
    pub edges: Vec<f64>,
    pub counts: Vec<u64>,
    pub approximate: bool,
}

impl Histogram {
    /// The bin index for `value`, clamping values outside the edges into
    /// the first or last bin.
    pub fn bin_of(&self, value: f64) -> usize {
        let bins = self.counts.len();
        if bins <= 1 {
            return 0;
        }
        let position = self.edges[1..bins].partition_point(|edge| *edge <= value);
        position.min(bins - 1)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NumericStats {
    /// Finite numeric values.
    pub count: u64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub min: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub max: f64,
    pub sum: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub mean: f64,
    /// Sample variance (n − 1); 0 for a single value.
    pub variance: f64,
    pub stddev: f64,
    pub nan_count: u64,
    pub positive_infinity_count: u64,
    pub negative_infinity_count: u64,
    pub zero_count: u64,
    pub negative_count: u64,
    pub all_integral: bool,
    pub percentiles: Percentiles,
    pub histogram: Histogram,
    /// Values outside the 1.5 × IQR fences.
    pub outlier_count: u64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub outlier_lower_fence: f64,
    #[serde(deserialize_with = "nan_as_null::deserialize")]
    pub outlier_upper_fence: f64,
    pub outliers_approximate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DateBucket {
    pub start: String,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemporalStats {
    pub count: u64,
    /// ISO 8601, UTC.
    pub min: String,
    pub max: String,
    pub has_time: bool,
    pub span_seconds: i64,
    pub grain: TimeGrain,
    pub buckets: Vec<DateBucket>,
    pub buckets_approximate: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextStats {
    /// Text values (NULLs excluded).
    pub count: u64,
    /// Lengths in characters.
    pub min_length: u64,
    pub max_length: u64,
    pub mean_length: f64,
    pub empty_count: u64,
    /// Non-empty values made only of whitespace.
    pub whitespace_only_count: u64,
    /// Values with leading or trailing whitespace.
    pub padded_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BooleanStats {
    pub true_count: u64,
    pub false_count: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnProfile {
    pub name: String,
    pub data_type: String,
    pub declared: DeclaredClass,
    pub semantic_type: SemanticType,
    /// Share of non-blank values that match `semantic_type`'s pattern.
    pub semantic_confidence: f64,
    pub count: u64,
    pub null_count: u64,
    pub null_rate: f64,
    pub distinct: Estimate,
    /// Distinct values ÷ non-NULL values.
    pub uniqueness: f64,
    pub candidate_key: KeyCandidate,
    pub top_values: Vec<ValueCount>,
    pub top_values_approximate: bool,
    pub monotonicity: Monotonicity,
    pub numeric: Option<NumericStats>,
    pub temporal: Option<TemporalStats>,
    pub text: Option<TextStats>,
    pub boolean: Option<BooleanStats>,
}

impl ColumnProfile {
    pub fn non_null_count(&self) -> u64 {
        self.count - self.null_count
    }
}

/// The profile of a whole result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultProfile {
    pub row_count: u64,
    pub completeness: Completeness,
    pub columns: Vec<ColumnProfile>,
}

impl ResultProfile {
    pub fn column(&self, name: &str) -> Option<&ColumnProfile> {
        self.columns
            .iter()
            .find(|column| column.name == name)
            .or_else(|| {
                self.columns
                    .iter()
                    .find(|column| column.name.eq_ignore_ascii_case(name))
            })
    }
}

/// Profile every row a view holds.
pub fn profile_output(view: &OutputView<'_>, options: &ProfileOptions) -> ResultProfile {
    let mut profiler = Profiler::new(view.columns, options.clone());
    view.for_each_batch(&mut |batch| {
        profiler.push_batch(batch);
        ControlFlow::Continue(())
    });
    profiler.finish(view.completeness)
}

/// Profile in-memory rows.
pub fn profile_rows(
    columns: &[ResultColumn],
    rows: &[Vec<CellValue>],
    completeness: Completeness,
    options: &ProfileOptions,
) -> ResultProfile {
    profile_output(&OutputView::new(columns, rows, completeness), options)
}

/// Incremental profiler: push rows or batches as they arrive, then
/// [`Profiler::finish`].
pub struct Profiler {
    options: ProfileOptions,
    rows: u64,
    columns: Vec<ColumnAccumulator>,
}

impl Profiler {
    pub fn new(columns: &[ResultColumn], options: ProfileOptions) -> Self {
        let accumulators = columns
            .iter()
            .map(|column| ColumnAccumulator::new(column, &options))
            .collect();
        Self {
            options,
            rows: 0,
            columns: accumulators,
        }
    }

    pub fn push_row(&mut self, row: &[CellValue]) {
        self.rows += 1;
        for (index, column) in self.columns.iter_mut().enumerate() {
            column.push(row.get(index).unwrap_or(&CellValue::Null), &self.options);
        }
    }

    pub fn push_batch(&mut self, rows: &[Vec<CellValue>]) {
        for row in rows {
            self.push_row(row);
        }
    }

    pub fn rows_seen(&self) -> u64 {
        self.rows
    }

    pub fn finish(self, completeness: Completeness) -> ResultProfile {
        let options = self.options;
        ResultProfile {
            row_count: self.rows,
            completeness,
            columns: self
                .columns
                .into_iter()
                .map(|column| column.finish(&options, completeness))
                .collect(),
        }
    }
}

const HASH_SEED: u64 = 0x5011_7A11;

#[derive(Default)]
struct PatternCounts {
    /// Non-NULL, non-blank values: the denominator for every share.
    base: u64,
    integer: u64,
    decimal: u64,
    boolean: u64,
    date: u64,
    datetime: u64,
    time: u64,
    uuid: u64,
    email: u64,
    url: u64,
    json: u64,
    ip: u64,
    currency: u64,
    binary: u64,
    spaced: u64,
}

struct ExactEntry {
    display: String,
    count: u64,
    first_seen: u64,
}

enum Distinct {
    Exact(HashMap<u64, ExactEntry>),
    Heavy {
        table: SpaceSaving<u64>,
        displays: HashMap<u64, String>,
    },
}

struct Welford {
    count: u64,
    mean: f64,
    m2: f64,
}

impl Welford {
    fn push(&mut self, value: f64) {
        self.count += 1;
        let delta = value - self.mean;
        self.mean += delta / self.count as f64;
        self.m2 += delta * (value - self.mean);
    }

    fn variance(&self) -> f64 {
        if self.count < 2 {
            0.0
        } else {
            (self.m2 / (self.count - 1) as f64).max(0.0)
        }
    }
}

/// Values kept exactly up to a cap, then folded into a KLL sketch.
struct QuantileStore {
    exact: Option<Vec<f64>>,
    sketch: Option<KllSketch>,
    cap: usize,
    k: usize,
}

impl QuantileStore {
    fn new(cap: usize, k: usize) -> Self {
        Self {
            exact: Some(Vec::new()),
            sketch: None,
            cap,
            k,
        }
    }

    fn push(&mut self, value: f64) {
        if let Some(exact) = &mut self.exact {
            if exact.len() < self.cap {
                exact.push(value);
                return;
            }
            let mut sketch = KllSketch::new(self.k);
            for buffered in exact.drain(..) {
                sketch.insert(buffered);
            }
            self.exact = None;
            self.sketch = Some(sketch);
        }
        if let Some(sketch) = &mut self.sketch {
            sketch.insert(value);
        }
    }

    fn is_approximate(&self) -> bool {
        self.exact.is_none()
    }

    fn sorted_exact(&self) -> Option<Vec<f64>> {
        self.exact.as_ref().map(|values| {
            let mut sorted = values.clone();
            sorted.sort_by(f64::total_cmp);
            sorted
        })
    }
}

/// Linear-interpolated quantile (the "type 7" definition spreadsheets and
/// NumPy use) over sorted values.
fn exact_quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let position = q.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    let weight = position - lower as f64;
    sorted[lower] + (sorted[upper] - sorted[lower]) * weight
}

#[derive(Clone, PartialEq)]
enum OrderKey {
    Number(f64),
    Time(NaiveDateTime),
    Text(String),
}

struct MonotonicTracker {
    previous: Option<OrderKey>,
    comparisons: u64,
    increasing: bool,
    decreasing: bool,
    strict_increasing: bool,
    strict_decreasing: bool,
    broken: bool,
}

impl MonotonicTracker {
    fn new() -> Self {
        Self {
            previous: None,
            comparisons: 0,
            increasing: true,
            decreasing: true,
            strict_increasing: true,
            strict_decreasing: true,
            broken: false,
        }
    }

    fn push(&mut self, key: Option<OrderKey>) {
        if self.broken {
            return;
        }
        let Some(key) = key else {
            self.broken = true;
            return;
        };
        if let Some(previous) = &self.previous {
            let ordering = match (previous, &key) {
                (OrderKey::Number(a), OrderKey::Number(b)) => a.partial_cmp(b),
                (OrderKey::Time(a), OrderKey::Time(b)) => Some(a.cmp(b)),
                (OrderKey::Text(a), OrderKey::Text(b)) => Some(a.cmp(b)),
                _ => None,
            };
            let Some(ordering) = ordering else {
                self.broken = true;
                return;
            };
            self.comparisons += 1;
            match ordering {
                std::cmp::Ordering::Less => {
                    self.decreasing = false;
                    self.strict_decreasing = false;
                }
                std::cmp::Ordering::Greater => {
                    self.increasing = false;
                    self.strict_increasing = false;
                }
                std::cmp::Ordering::Equal => {
                    self.strict_increasing = false;
                    self.strict_decreasing = false;
                }
            }
        }
        self.previous = Some(key);
    }

    fn result(&self) -> Monotonicity {
        if self.broken {
            return Monotonicity::NotMonotonic;
        }
        if self.comparisons == 0 {
            return Monotonicity::Unknown;
        }
        match (
            self.increasing,
            self.decreasing,
            self.strict_increasing,
            self.strict_decreasing,
        ) {
            (true, true, _, _) => Monotonicity::Constant,
            (true, _, true, _) => Monotonicity::StrictlyIncreasing,
            (true, _, _, _) => Monotonicity::Increasing,
            (_, true, _, true) => Monotonicity::StrictlyDecreasing,
            (_, true, _, _) => Monotonicity::Decreasing,
            _ => Monotonicity::NotMonotonic,
        }
    }
}

struct ColumnAccumulator {
    name: String,
    data_type: String,
    declared: DeclaredClass,
    count: u64,
    nulls: u64,
    hll: HyperLogLog,
    distinct: Distinct,
    patterns: PatternCounts,
    welford: Welford,
    numeric_min: f64,
    numeric_max: f64,
    numeric_sum: f64,
    nan_count: u64,
    pos_inf: u64,
    neg_inf: u64,
    zero_count: u64,
    negative_count: u64,
    all_integral: bool,
    numbers: QuantileStore,
    time_count: u64,
    time_min: Option<NaiveDateTime>,
    time_max: Option<NaiveDateTime>,
    has_time: bool,
    times: QuantileStore,
    text_count: u64,
    text_min_len: u64,
    text_max_len: u64,
    text_len_sum: u64,
    empty_count: u64,
    whitespace_only: u64,
    padded: u64,
    true_count: u64,
    false_count: u64,
    monotonic: MonotonicTracker,
}

impl ColumnAccumulator {
    fn new(column: &ResultColumn, options: &ProfileOptions) -> Self {
        Self {
            name: column.name.clone(),
            data_type: column.data_type.clone(),
            declared: declared_class(&column.data_type),
            count: 0,
            nulls: 0,
            hll: HyperLogLog::new(options.hll_precision),
            distinct: Distinct::Exact(HashMap::new()),
            patterns: PatternCounts::default(),
            welford: Welford {
                count: 0,
                mean: 0.0,
                m2: 0.0,
            },
            numeric_min: f64::INFINITY,
            numeric_max: f64::NEG_INFINITY,
            numeric_sum: 0.0,
            nan_count: 0,
            pos_inf: 0,
            neg_inf: 0,
            zero_count: 0,
            negative_count: 0,
            all_integral: true,
            numbers: QuantileStore::new(options.exact_quantile_cap, options.kll_k),
            time_count: 0,
            time_min: None,
            time_max: None,
            has_time: false,
            times: QuantileStore::new(options.exact_quantile_cap, options.kll_k),
            text_count: 0,
            text_min_len: u64::MAX,
            text_max_len: 0,
            text_len_sum: 0,
            empty_count: 0,
            whitespace_only: 0,
            padded: 0,
            true_count: 0,
            false_count: 0,
            monotonic: MonotonicTracker::new(),
        }
    }

    fn push(&mut self, cell: &CellValue, options: &ProfileOptions) {
        self.count += 1;
        if matches!(cell, CellValue::Null) {
            self.nulls += 1;
            return;
        }
        let hash = cell_hash(cell, HASH_SEED);
        self.hll.insert_hash(hash);
        self.count_distinct(hash, cell, options);
        let order_key = match cell {
            CellValue::Null => unreachable!("handled above"),
            CellValue::Bool(b) => {
                self.patterns.base += 1;
                self.patterns.boolean += 1;
                self.count_bool(*b);
                Some(OrderKey::Number(f64::from(u8::from(*b))))
            }
            CellValue::I64(n) => {
                self.patterns.base += 1;
                self.patterns.integer += 1;
                if self.declared == DeclaredClass::Boolean && (*n == 0 || *n == 1) {
                    self.count_bool(*n == 1);
                }
                self.push_number(*n as f64);
                Some(OrderKey::Number(*n as f64))
            }
            CellValue::F64(value) => {
                self.patterns.base += 1;
                self.patterns.decimal += 1;
                self.push_number(*value);
                (!value.is_nan()).then_some(OrderKey::Number(*value))
            }
            CellValue::Bytes(_) => {
                self.patterns.base += 1;
                self.patterns.binary += 1;
                None
            }
            CellValue::String(text) => self.push_text(text, options),
        };
        self.monotonic.push(order_key);
    }

    fn count_bool(&mut self, value: bool) {
        if value {
            self.true_count += 1;
        } else {
            self.false_count += 1;
        }
    }

    fn count_distinct(&mut self, hash: u64, cell: &CellValue, options: &ProfileOptions) {
        let ordinal = self.count;
        match &mut self.distinct {
            Distinct::Exact(map) => {
                if let Some(entry) = map.get_mut(&hash) {
                    entry.count += 1;
                    return;
                }
                if map.len() < options.exact_distinct_cap {
                    map.insert(
                        hash,
                        ExactEntry {
                            display: display_value(cell, options),
                            count: 1,
                            first_seen: ordinal,
                        },
                    );
                    return;
                }
                let mut entries: Vec<(u64, ExactEntry)> = map.drain().collect();
                entries.sort_by(|a, b| {
                    b.1.count
                        .cmp(&a.1.count)
                        .then(a.1.first_seen.cmp(&b.1.first_seen))
                });
                let mut table = SpaceSaving::new(options.heavy_hitter_capacity);
                let mut displays = HashMap::new();
                for (key, entry) in entries.into_iter().take(options.heavy_hitter_capacity) {
                    table.insert_weighted(key, entry.count, 0);
                    displays.insert(key, entry.display);
                }
                self.distinct = Distinct::Heavy { table, displays };
                self.count_distinct(hash, cell, options);
            }
            Distinct::Heavy { table, displays } => {
                let tracked = table.contains(&hash);
                if let Some(evicted) = table.insert(hash) {
                    displays.remove(&evicted);
                }
                if !tracked {
                    displays.insert(hash, display_value(cell, options));
                }
            }
        }
    }

    fn push_number(&mut self, value: f64) {
        if value.is_nan() {
            self.nan_count += 1;
            return;
        }
        if value.is_infinite() {
            if value > 0.0 {
                self.pos_inf += 1;
            } else {
                self.neg_inf += 1;
            }
            return;
        }
        self.welford.push(value);
        self.numeric_min = self.numeric_min.min(value);
        self.numeric_max = self.numeric_max.max(value);
        self.numeric_sum += value;
        if value == 0.0 {
            self.zero_count += 1;
        } else if value < 0.0 {
            self.negative_count += 1;
        }
        if value.fract() != 0.0 {
            self.all_integral = false;
        }
        self.numbers.push(value);
    }

    fn push_time(&mut self, value: NaiveDateTime) {
        self.time_count += 1;
        self.time_min = Some(self.time_min.map_or(value, |min| min.min(value)));
        self.time_max = Some(self.time_max.map_or(value, |max| max.max(value)));
        if has_time_of_day(value) {
            self.has_time = true;
        }
        self.times.push(epoch_seconds(value) as f64);
    }

    fn push_text(&mut self, text: &str, options: &ProfileOptions) -> Option<OrderKey> {
        let length = text.chars().count() as u64;
        self.text_count += 1;
        self.text_min_len = self.text_min_len.min(length);
        self.text_max_len = self.text_max_len.max(length);
        self.text_len_sum += length;
        if text.is_empty() {
            self.empty_count += 1;
            return Some(OrderKey::Text(String::new()));
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            self.whitespace_only += 1;
            return Some(OrderKey::Text(text.to_string()));
        }
        if trimmed.len() != text.len() {
            self.padded += 1;
        }
        self.patterns.base += 1;
        if trimmed.contains(char::is_whitespace) {
            self.patterns.spaced += 1;
        }

        if let Some(number) = numeric_literal(trimmed) {
            if is_integer_literal(trimmed) {
                self.patterns.integer += 1;
                if self.declared == DeclaredClass::Boolean && (trimmed == "0" || trimmed == "1") {
                    self.count_bool(trimmed == "1");
                }
            } else {
                self.patterns.decimal += 1;
            }
            self.push_number(number);
            return Some(OrderKey::Number(number));
        }
        if let Some(value) = bool_word(trimmed) {
            self.patterns.boolean += 1;
            self.count_bool(value);
            return Some(OrderKey::Number(f64::from(u8::from(value))));
        }
        if let Some(stamp) = parse_timestamp(trimmed) {
            if has_time_of_day(stamp) || trimmed.len() > 10 {
                self.patterns.datetime += 1;
            } else {
                self.patterns.date += 1;
            }
            self.push_time(stamp);
            return Some(OrderKey::Time(stamp));
        }
        if let Some(amount) = currency_value(trimmed) {
            self.patterns.currency += 1;
            self.push_number(amount);
            return Some(OrderKey::Number(amount));
        }
        if is_time_of_day(trimmed) {
            self.patterns.time += 1;
        } else if is_uuid(trimmed) {
            self.patterns.uuid += 1;
        } else if is_email(trimmed) {
            self.patterns.email += 1;
        } else if is_url(trimmed) {
            self.patterns.url += 1;
        } else if trimmed.parse::<std::net::IpAddr>().is_ok() {
            self.patterns.ip += 1;
        } else if is_json(trimmed, options.json_parse_max_bytes) {
            self.patterns.json += 1;
        }
        Some(OrderKey::Text(text.to_string()))
    }

    fn finish(self, options: &ProfileOptions, completeness: Completeness) -> ColumnProfile {
        let non_null = self.count - self.nulls;
        let (distinct, top_values, top_approx) = match &self.distinct {
            Distinct::Exact(map) => {
                let mut entries: Vec<&ExactEntry> = map.values().collect();
                entries.sort_by(|a, b| b.count.cmp(&a.count).then(a.first_seen.cmp(&b.first_seen)));
                let top = entries
                    .into_iter()
                    .take(options.top_k)
                    .map(|entry| ValueCount {
                        value: entry.display.clone(),
                        count: entry.count,
                        error: 0,
                    })
                    .collect();
                (
                    Estimate {
                        value: map.len() as u64,
                        approximate: false,
                    },
                    top,
                    false,
                )
            }
            Distinct::Heavy { table, displays } => {
                // The sketch took over when a value past the exact cap
                // arrived, so there are at least that many.
                let estimate = (self.hll.estimate().round() as u64)
                    .max(options.exact_distinct_cap as u64 + 1)
                    .min(non_null);
                let top = table
                    .top(options.top_k)
                    .into_iter()
                    .map(|hitter| ValueCount {
                        value: displays.get(&hitter.key).cloned().unwrap_or_default(),
                        count: hitter.count,
                        error: hitter.error,
                    })
                    .collect();
                (
                    Estimate {
                        value: estimate,
                        approximate: true,
                    },
                    top,
                    true,
                )
            }
        };
        let uniqueness = if non_null == 0 {
            0.0
        } else {
            (distinct.value as f64 / non_null as f64).min(1.0)
        };
        let candidate_key = if self.nulls > 0 || non_null == 0 {
            KeyCandidate::NotUnique
        } else if !distinct.approximate {
            if distinct.value < non_null {
                KeyCandidate::NotUnique
            } else if completeness.is_complete() {
                KeyCandidate::Unique
            } else {
                KeyCandidate::LikelyUnique
            }
        } else if uniqueness >= 0.97 {
            KeyCandidate::LikelyUnique
        } else {
            KeyCandidate::NotUnique
        };

        let mean_length = if self.text_count == 0 {
            0.0
        } else {
            self.text_len_sum as f64 / self.text_count as f64
        };
        let (semantic_type, semantic_confidence) =
            self.semantic(distinct, uniqueness, mean_length, options);

        let keep_numeric = self.welford.count + self.nan_count + self.pos_inf + self.neg_inf > 0
            && (self.declared.is_numeric()
                || semantic_type.is_numeric()
                || (semantic_type == SemanticType::IdLike
                    && self.welford.count >= self.patterns.base));
        let numeric = keep_numeric.then(|| self.numeric_stats(options));
        let keep_temporal =
            self.time_count > 0 && (self.declared.is_temporal() || semantic_type.is_temporal());
        let temporal = if keep_temporal {
            self.temporal_stats(options)
        } else {
            None
        };
        let text = (self.text_count > 0).then_some(TextStats {
            count: self.text_count,
            min_length: self.text_min_len,
            max_length: self.text_max_len,
            mean_length,
            empty_count: self.empty_count,
            whitespace_only_count: self.whitespace_only,
            padded_count: self.padded,
        });
        let boolean = (semantic_type == SemanticType::Boolean).then_some(BooleanStats {
            true_count: self.true_count,
            false_count: self.false_count,
        });

        ColumnProfile {
            name: self.name,
            data_type: self.data_type,
            declared: self.declared,
            semantic_type,
            semantic_confidence,
            count: self.count,
            null_count: self.nulls,
            null_rate: if self.count == 0 {
                0.0
            } else {
                self.nulls as f64 / self.count as f64
            },
            distinct,
            uniqueness,
            candidate_key,
            top_values,
            top_values_approximate: top_approx,
            monotonicity: self.monotonic.result(),
            numeric,
            temporal,
            text,
            boolean,
        }
    }

    fn semantic(
        &self,
        distinct: Estimate,
        uniqueness: f64,
        mean_length: f64,
        options: &ProfileOptions,
    ) -> (SemanticType, f64) {
        let p = &self.patterns;
        if p.base == 0 {
            return (SemanticType::Empty, 1.0);
        }
        let base = p.base as f64;
        let share = |n: u64| n as f64 / base;
        let numeric = p.integer + p.decimal;
        let temporal = p.date + p.datetime;

        let declared = match self.declared {
            DeclaredClass::Boolean => Some((SemanticType::Boolean, share(p.boolean + p.integer))),
            DeclaredClass::Integer => Some((SemanticType::Integer, share(p.integer))),
            DeclaredClass::Decimal => {
                let money = self.data_type.to_ascii_lowercase().contains("money");
                let kind = if money {
                    SemanticType::CurrencyLike
                } else {
                    SemanticType::Decimal
                };
                Some((kind, share(numeric)))
            }
            DeclaredClass::Date => Some((SemanticType::Date, share(temporal))),
            DeclaredClass::DateTime => Some((SemanticType::DateTime, share(temporal))),
            DeclaredClass::Time => Some((SemanticType::Time, share(p.time))),
            DeclaredClass::Uuid => Some((SemanticType::Uuid, share(p.uuid))),
            DeclaredClass::Json => Some((SemanticType::Json, share(p.json))),
            DeclaredClass::Binary => Some((SemanticType::Binary, share(p.binary))),
            DeclaredClass::Text | DeclaredClass::Unknown => None,
        };
        let (base_type, confidence) = declared.unwrap_or_else(|| {
            const THRESHOLD: f64 = 0.95;
            let candidates = [
                (SemanticType::Binary, p.binary),
                (SemanticType::Boolean, p.boolean),
                (SemanticType::Integer, p.integer),
                (
                    if p.decimal > 0 {
                        SemanticType::Decimal
                    } else {
                        SemanticType::Integer
                    },
                    numeric,
                ),
                (SemanticType::CurrencyLike, p.currency + numeric),
                (
                    if p.datetime > 0 {
                        SemanticType::DateTime
                    } else {
                        SemanticType::Date
                    },
                    temporal,
                ),
                (SemanticType::Time, p.time),
                (SemanticType::Uuid, p.uuid),
                (SemanticType::Email, p.email),
                (SemanticType::Url, p.url),
                (SemanticType::Ip, p.ip),
                (SemanticType::Json, p.json),
            ];
            candidates
                .into_iter()
                .find(|(kind, n)| {
                    share(*n) >= THRESHOLD
                        && (*kind != SemanticType::CurrencyLike || p.currency > 0)
                })
                .map(|(kind, n)| (kind, share(n)))
                .unwrap_or_else(|| {
                    if distinct.value <= options.categorical_max_distinct
                        && (distinct.value as f64) <= 0.5 * base
                    {
                        (SemanticType::Categorical, 1.0)
                    } else if mean_length >= 24.0 && share(p.spaced) >= 0.5 {
                        (SemanticType::FreeText, share(p.spaced))
                    } else {
                        (SemanticType::Text, 1.0)
                    }
                })
        });

        let id_capable = matches!(
            base_type,
            SemanticType::Integer | SemanticType::Uuid | SemanticType::Text
        );
        if id_capable && p.base >= 2 && uniqueness >= 0.99 && name_looks_like_id(&self.name) {
            return (SemanticType::IdLike, uniqueness);
        }
        (base_type, confidence)
    }

    fn numeric_stats(&self, options: &ProfileOptions) -> NumericStats {
        let count = self.welford.count;
        let (min, max) = if count == 0 {
            (f64::NAN, f64::NAN)
        } else {
            (self.numeric_min, self.numeric_max)
        };
        let approximate = self.numbers.is_approximate();
        let sorted = self.numbers.sorted_exact();
        let qs = [0.01, 0.05, 0.25, 0.5, 0.75, 0.95, 0.99];
        let values: Vec<f64> = match (&sorted, &self.numbers.sketch) {
            (Some(sorted), _) => qs.iter().map(|q| exact_quantile(sorted, *q)).collect(),
            (None, Some(sketch)) => sketch
                .quantiles(&qs)
                .into_iter()
                .map(|v| v.unwrap_or(f64::NAN))
                .collect(),
            (None, None) => vec![f64::NAN; qs.len()],
        };
        let percentiles = Percentiles {
            p1: values[0],
            p5: values[1],
            p25: values[2],
            p50: values[3],
            p75: values[4],
            p95: values[5],
            p99: values[6],
            approximate,
        };
        let iqr = percentiles.p75 - percentiles.p25;
        let lower_fence = percentiles.p25 - 1.5 * iqr;
        let upper_fence = percentiles.p75 + 1.5 * iqr;
        let outlier_count = if count == 0 {
            0
        } else if let Some(sorted) = &sorted {
            sorted
                .iter()
                .filter(|v| **v < lower_fence || **v > upper_fence)
                .count() as u64
        } else if let Some(sketch) = &self.numbers.sketch {
            let share = sketch.cdf_exclusive(lower_fence) + (1.0 - sketch.cdf(upper_fence));
            (share * count as f64).round() as u64
        } else {
            0
        };
        let histogram = numeric_histogram(
            min,
            max,
            count,
            self.all_integral,
            options.histogram_bins,
            sorted.as_deref(),
            self.numbers.sketch.as_ref(),
        );
        let variance = self.welford.variance();
        NumericStats {
            count,
            min,
            max,
            sum: self.numeric_sum,
            mean: if count == 0 {
                f64::NAN
            } else {
                self.welford.mean
            },
            variance,
            stddev: variance.sqrt(),
            nan_count: self.nan_count,
            positive_infinity_count: self.pos_inf,
            negative_infinity_count: self.neg_inf,
            zero_count: self.zero_count,
            negative_count: self.negative_count,
            all_integral: self.all_integral,
            percentiles,
            histogram,
            outlier_count,
            outlier_lower_fence: lower_fence,
            outlier_upper_fence: upper_fence,
            outliers_approximate: approximate,
        }
    }

    fn temporal_stats(&self, options: &ProfileOptions) -> Option<TemporalStats> {
        let (min, max) = (self.time_min?, self.time_max?);
        let grain = choose_grain(min, max, options.date_histogram_buckets, !self.has_time);
        let starts = bucket_starts(min, max, grain, options.date_histogram_buckets.max(1) * 2);
        let start_secs: Vec<f64> = starts.iter().map(|s| epoch_seconds(*s) as f64).collect();
        let mut counts = vec![0u64; starts.len()];
        let approximate = self.times.is_approximate();
        if let Some(exact) = &self.times.exact {
            for value in exact {
                let last = counts.len() - 1;
                let index = start_secs.partition_point(|start| start <= value);
                counts[index.saturating_sub(1).min(last)] += 1;
            }
        } else if let Some(sketch) = &self.times.sketch {
            let total = self.time_count;
            let mut previous = 0u64;
            for (index, start) in start_secs.iter().enumerate().skip(1) {
                let cumulative = (sketch.cdf_exclusive(*start) * total as f64).round() as u64;
                counts[index - 1] = cumulative.saturating_sub(previous);
                previous = cumulative.max(previous);
            }
            let last = counts.len() - 1;
            counts[last] = total.saturating_sub(previous);
        }
        let format = |value: NaiveDateTime| {
            if self.has_time {
                value.format("%Y-%m-%dT%H:%M:%S").to_string()
            } else {
                value.format("%Y-%m-%d").to_string()
            }
        };
        Some(TemporalStats {
            count: self.time_count,
            min: format(min),
            max: format(max),
            has_time: self.has_time,
            span_seconds: (max - min).num_seconds(),
            grain,
            buckets: starts
                .iter()
                .zip(counts)
                .map(|(start, count)| DateBucket {
                    start: bucket_label(*start, grain),
                    count,
                })
                .collect(),
            buckets_approximate: approximate,
        })
    }
}

fn numeric_histogram(
    min: f64,
    max: f64,
    count: u64,
    all_integral: bool,
    bins: usize,
    sorted: Option<&[f64]>,
    sketch: Option<&KllSketch>,
) -> Histogram {
    let approximate = sorted.is_none();
    if count == 0 || !min.is_finite() || !max.is_finite() {
        return Histogram {
            edges: Vec::new(),
            counts: Vec::new(),
            approximate: false,
        };
    }
    let bins = bins.max(1);
    let edges: Vec<f64> = if min == max {
        vec![min, max]
    } else if all_integral && max - min < bins as f64 {
        let width = (max - min) as usize + 1;
        (0..=width).map(|step| min + step as f64).collect()
    } else {
        let width = (max - min) / bins as f64;
        (0..=bins)
            .map(|step| {
                if step == bins {
                    max
                } else {
                    min + width * step as f64
                }
            })
            .collect()
    };
    let bin_count = edges.len() - 1;
    let mut counts = vec![0u64; bin_count];
    let histogram_bin = |value: f64| {
        edges[1..bin_count]
            .partition_point(|edge| *edge <= value)
            .min(bin_count - 1)
    };
    if let Some(sorted) = sorted {
        for value in sorted {
            counts[histogram_bin(*value)] += 1;
        }
    } else if let Some(sketch) = sketch {
        let mut previous = 0u64;
        for index in 1..bin_count {
            let cumulative = (sketch.cdf_exclusive(edges[index]) * count as f64).round() as u64;
            counts[index - 1] = cumulative.saturating_sub(previous);
            previous = cumulative.max(previous);
        }
        counts[bin_count - 1] = count.saturating_sub(previous);
    }
    Histogram {
        edges,
        counts,
        approximate,
    }
}

fn display_value(cell: &CellValue, options: &ProfileOptions) -> String {
    match cell {
        CellValue::F64(value) => format_float(*value),
        _ => super::value::cell_text(cell)
            .map(|text| truncate_chars(&text, options.value_display_chars).into_owned())
            .unwrap_or_default(),
    }
}

fn bool_word(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "y" => Some(true),
        "false" | "f" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn is_time_of_day(text: &str) -> bool {
    let bytes = text.as_bytes();
    (bytes.len() == 5 || bytes.len() >= 8)
        && bytes.len() <= 18
        && bytes[2] == b':'
        && chrono::NaiveTime::parse_from_str(text, "%H:%M:%S%.f")
            .or_else(|_| chrono::NaiveTime::parse_from_str(text, "%H:%M"))
            .is_ok()
}

fn is_uuid(text: &str) -> bool {
    let inner = text
        .strip_prefix('{')
        .and_then(|t| t.strip_suffix('}'))
        .unwrap_or(text);
    let bytes = inner.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

fn is_email(text: &str) -> bool {
    if text.len() > 254 || text.contains(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = text.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains("..")
}

fn is_url(text: &str) -> bool {
    let lower = text.get(..8).unwrap_or(text).to_ascii_lowercase();
    let prefix = ["https://", "http://", "ftp://"]
        .iter()
        .find(|prefix| lower.starts_with(**prefix));
    prefix.is_some_and(|prefix| text.len() > prefix.len() && !text.contains(char::is_whitespace))
}

fn is_json(text: &str, max_bytes: usize) -> bool {
    let bytes = text.as_bytes();
    text.len() <= max_bytes
        && bytes.len() >= 2
        && matches!(
            (bytes[0], bytes[bytes.len() - 1]),
            (b'{', b'}') | (b'[', b']')
        )
        && serde_json::from_str::<serde::de::IgnoredAny>(text).is_ok()
}

/// `id`, `customer_id`, `CustomerId`, `order_key`, `guid`, `uuid`, `pk`.
fn name_looks_like_id(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower == "id"
        || lower == "pk"
        || lower == "key"
        || lower.ends_with("_id")
        || lower.ends_with("-id")
        || lower.ends_with(" id")
        || lower.ends_with("_key")
        || lower.ends_with("guid")
        || lower.ends_with("uuid")
        || name.ends_with("Id")
        || name.ends_with("ID")
}

#[cfg(test)]
#[path = "profile_tests.rs"]
mod tests;
