//! Declarative assertions over notebook outputs: row counts, uniqueness,
//! NULL rates, ranges, accepted values, patterns, referential integrity,
//! distribution drift against a baseline profile, freshness, duration and
//! KPI thresholds.
//!
//! Every outcome carries evidence (observed metrics plus a capped sample of
//! failing rows and values). An assertion over an incomplete input
//! (truncated, sampled, partial) reports `Inconclusive` rather than risk a
//! false alarm, unless the spec sets `allow_incomplete`; the evidence is
//! still filled in from the rows that were read.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::ControlFlow;

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sqlly_core::CellValue;

use super::decimal::Decimal;
use super::kpi::{KpiError, ValueSource, evaluate_value};
use super::profile::{ColumnProfile, ResultProfile};
use super::time::{epoch_seconds, parse_zoned};
use super::value::{cell_hash, cell_text, cells_hash, finite_number, truncate_chars};
use super::{Completeness, OutputView};

/// Failing row indexes kept as evidence.
pub const FAILING_ROW_CAP: usize = 20;
/// Distinct failing values kept as evidence.
pub const FAILING_VALUE_CAP: usize = 10;
/// Longest regex pattern accepted, in characters.
pub const MAX_PATTERN_CHARS: usize = 1_024;
/// Compiled-program and lazy-DFA size limits for a pattern, so a pattern
/// like `(a{1000}){1000}` is refused instead of exhausting memory. Matching
/// itself is linear time (the regex engine never backtracks).
pub const PATTERN_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompareOp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl CompareOp {
    pub fn holds(self, value: f64, threshold: f64) -> bool {
        let close = (value - threshold).abs() <= 1e-9 * value.abs().max(threshold.abs()).max(1.0);
        match self {
            CompareOp::Lt => value < threshold && !close,
            CompareOp::Le => value <= threshold || close,
            CompareOp::Gt => value > threshold && !close,
            CompareOp::Ge => value >= threshold || close,
            CompareOp::Eq => close,
            CompareOp::Ne => !close,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DriftMetric {
    /// Population stability index over the baseline's histogram bins (or
    /// its top values for categorical columns).
    #[default]
    Psi,
    /// Kolmogorov–Smirnov distance between the baseline histogram's CDF and
    /// the current values' CDF, at the baseline bin edges.
    Ks,
    /// |current mean − baseline mean| in baseline standard deviations.
    MeanShift,
}

impl DriftMetric {
    /// Conventional (warn, fail) levels.
    pub fn default_levels(self) -> (f64, f64) {
        match self {
            DriftMetric::Psi => (0.1, 0.25),
            DriftMetric::Ks => (0.1, 0.2),
            DriftMetric::MeanShift => (1.0, 2.0),
        }
    }
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Assertion {
    RowCount {
        #[serde(default)]
        min: Option<u64>,
        #[serde(default)]
        max: Option<u64>,
    },
    /// No two rows share these columns' values. Rows with a NULL in any of
    /// the columns are not compared (as a SQL UNIQUE constraint).
    Unique { columns: Vec<String> },
    NotNull {
        column: String,
        /// Highest acceptable share of NULLs, 0 by default.
        #[serde(default)]
        max_null_rate: f64,
    },
    /// Every non-NULL value is a number within the bounds.
    ValueRange {
        column: String,
        #[serde(default)]
        min: Option<f64>,
        #[serde(default)]
        max: Option<f64>,
        /// Exclude the bounds themselves.
        #[serde(default)]
        exclusive: bool,
    },
    AcceptedValues {
        column: String,
        values: Vec<String>,
        #[serde(default)]
        case_insensitive: bool,
        #[serde(default = "yes")]
        allow_null: bool,
    },
    Regex {
        column: String,
        pattern: String,
        /// Require the whole value to match (otherwise any substring).
        #[serde(default = "yes")]
        full_match: bool,
        #[serde(default = "yes")]
        allow_null: bool,
    },
    /// Every non-NULL `column` value appears in `parent`'s `parent_column`.
    Referential {
        column: String,
        parent: String,
        parent_column: String,
    },
    DistributionDrift {
        column: String,
        #[serde(default)]
        metric: DriftMetric,
        #[serde(default)]
        warn: Option<f64>,
        #[serde(default)]
        fail: Option<f64>,
    },
    /// The newest timestamp in `column` is at most `max_age_seconds` old.
    /// Timestamps without a zone are read in `time_zone` (an IANA name such
    /// as `Europe/Berlin`, `UTC`, or an offset such as `+05:30`; UTC when
    /// unset). A newest timestamp in the future warns.
    Freshness {
        column: String,
        max_age_seconds: u64,
        #[serde(default)]
        warn_age_seconds: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        time_zone: Option<String>,
    },
    Duration {
        max_ms: u64,
        #[serde(default)]
        warn_ms: Option<u64>,
    },
    KpiThreshold {
        value: ValueSource,
        op: CompareOp,
        threshold: f64,
    },
}

impl Assertion {
    /// Whether the verdict depends on seeing every row.
    pub fn depends_on_every_row(&self) -> bool {
        match self {
            Assertion::Duration { .. } => false,
            Assertion::KpiThreshold { value, .. } => value.depends_on_every_row(),
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// A failure is `Fail`.
    #[default]
    Error,
    /// A failure is downgraded to `Warn`.
    Warn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionSpec {
    #[serde(flatten)]
    pub assertion: Assertion,
    #[serde(default)]
    pub severity: Severity,
    /// Judge truncated, sampled or partial inputs anyway.
    #[serde(default)]
    pub allow_incomplete: bool,
    #[serde(default)]
    pub name: Option<String>,
}

impl From<Assertion> for AssertionSpec {
    fn from(assertion: Assertion) -> Self {
        Self {
            assertion,
            severity: Severity::Error,
            allow_incomplete: false,
            name: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssertionStatus {
    Pass,
    Warn,
    Fail,
    /// The assertion could not run (bad spec, missing column…).
    Error,
    /// The data cannot settle it (incomplete input or parent).
    Inconclusive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AssertionIssue {
    IncompleteInput {
        completeness: Completeness,
    },
    IncompleteParent {
        completeness: Completeness,
    },
    IncompleteBaseline {
        completeness: Completeness,
    },
    MissingColumn {
        column: String,
    },
    MissingOutput {
        name: String,
    },
    MissingBaseline,
    MissingBaselineColumn {
        column: String,
    },
    /// The metric does not apply to this column's baseline.
    UnsupportedMetric,
    /// Freshness needs the caller's clock.
    NoClock,
    /// Duration needs the run's elapsed time.
    NoDuration,
    InvalidPattern {
        detail: String,
    },
    PatternTooLong {
        max_chars: usize,
    },
    NoTimestamps,
    /// The freshness check's time zone is not an IANA name or offset.
    InvalidTimeZone {
        zone: String,
    },
    /// The newest timestamp is later than now: a clock or time zone is off.
    FutureTimestamp,
    NoNumericValues,
    Kpi {
        error: KpiError,
    },
}

impl AssertionIssue {
    /// The issue as a catalog message (`notebook.analysis.assertion.issue.*`)
    /// with English fallback, for exports and other non-UI readers.
    pub fn message(&self) -> sqlly_core::DisplayMessage {
        use sqlly_core::DisplayMessage as M;
        match self {
            AssertionIssue::IncompleteInput { .. } => M::new(
                "notebook.analysis.assertion.issue.incomplete_input",
                "The input was cut short or sampled, so this check cannot pass or fail.",
            ),
            AssertionIssue::IncompleteParent { .. } => M::new(
                "notebook.analysis.assertion.issue.incomplete_parent",
                "The parent result was cut short, so missing matches cannot be judged.",
            ),
            AssertionIssue::IncompleteBaseline { .. } => M::new(
                "notebook.analysis.assertion.issue.incomplete_baseline",
                "The baseline was cut short, so drift cannot be judged.",
            ),
            AssertionIssue::MissingColumn { column } => M::new(
                "notebook.analysis.assertion.issue.missing_column",
                format!("Column {column} is not in the input."),
            )
            .with_arg("column", column.clone()),
            AssertionIssue::MissingOutput { name } => {
                M::new("notebook.analysis.assertion.issue.missing_output", format!("No result named {name}."))
                    .with_arg("name", name.clone())
            }
            AssertionIssue::MissingBaseline => M::new(
                "notebook.analysis.assertion.issue.missing_baseline",
                "No baseline to compare with yet. Run the notebook once or pin a snapshot.",
            ),
            AssertionIssue::MissingBaselineColumn { column } => M::new(
                "notebook.analysis.assertion.issue.missing_baseline_column",
                format!("The baseline has no column {column}."),
            )
            .with_arg("column", column.clone()),
            AssertionIssue::UnsupportedMetric => M::new(
                "notebook.analysis.assertion.issue.unsupported_metric",
                "This drift measure does not apply to this column.",
            ),
            AssertionIssue::NoClock => M::new("notebook.analysis.assertion.issue.no_clock", "Freshness needs the current time."),
            AssertionIssue::NoDuration => {
                M::new("notebook.analysis.assertion.issue.no_duration", "The input's run time is not known.")
            }
            AssertionIssue::InvalidPattern { detail } => M::new(
                "notebook.analysis.assertion.issue.invalid_pattern",
                format!("The pattern is not valid: {detail}"),
            )
            .with_arg("detail", detail.clone()),
            AssertionIssue::PatternTooLong { max_chars } => M::new(
                "notebook.analysis.assertion.issue.pattern_too_long",
                format!("The pattern is longer than {max_chars} characters."),
            )
            .with_arg("max_chars", max_chars.to_string()),
            AssertionIssue::NoTimestamps => {
                M::new("notebook.analysis.assertion.issue.no_timestamps", "The column has no dates or times.")
            }
            AssertionIssue::InvalidTimeZone { zone } => M::new(
                "notebook.analysis.assertion.issue.invalid_time_zone",
                format!(
                    "{zone} is not a time zone name (such as Europe/Berlin) or an offset (such as +05:30)."
                ),
            )
            .with_arg("zone", zone.clone()),
            AssertionIssue::FutureTimestamp => M::new(
                "notebook.analysis.assertion.issue.future_timestamp",
                "The newest time is in the future; check the clock or the time zone the column is read in.",
            ),
            AssertionIssue::NoNumericValues => {
                M::new("notebook.analysis.assertion.issue.no_numeric_values", "The column has no numbers.")
            }
            AssertionIssue::Kpi { .. } => M::new("notebook.analysis.assertion.issue.kpi", "The KPI value could not be computed."),
        }
    }
}

/// One observed figure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Metric {
    Bool(bool),
    Integer(i64),
    Number(f64),
    Text(String),
}

impl From<u64> for Metric {
    fn from(value: u64) -> Self {
        Metric::Integer(i64::try_from(value).unwrap_or(i64::MAX))
    }
}

impl From<f64> for Metric {
    fn from(value: f64) -> Self {
        Metric::Number(value)
    }
}

impl From<&str> for Metric {
    fn from(value: &str) -> Self {
        Metric::Text(value.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Evidence {
    pub observed: BTreeMap<String, Metric>,
    pub checked_rows: u64,
    pub failing_count: u64,
    /// 0-based indexes of the first failing rows.
    pub failing_rows: Vec<usize>,
    /// The first distinct failing values, as display text.
    pub failing_values: Vec<String>,
    /// Gathered from an incomplete input.
    pub partial: bool,
}

impl Evidence {
    fn observe(&mut self, name: &str, value: impl Into<Metric>) {
        self.observed.insert(name.to_string(), value.into());
    }

    fn fail_row(&mut self, row: usize, value: Option<&CellValue>) {
        self.failing_count += 1;
        if self.failing_rows.len() < FAILING_ROW_CAP {
            self.failing_rows.push(row);
        }
        if let Some(cell) = value
            && self.failing_values.len() < FAILING_VALUE_CAP
        {
            let text = cell_text(cell)
                .map(|t| truncate_chars(&t, 80).into_owned())
                .unwrap_or_else(|| "NULL".to_string());
            if !self.failing_values.contains(&text) {
                self.failing_values.push(text);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssertionOutcome {
    pub name: Option<String>,
    pub status: AssertionStatus,
    pub issue: Option<AssertionIssue>,
    pub evidence: Evidence,
}

/// Other outputs an assertion can name (the parent of a referential check).
pub trait OutputResolver {
    fn output(&self, name: &str) -> Option<OutputView<'_>>;
}

/// No other outputs.
pub struct NoOutputs;

impl OutputResolver for NoOutputs {
    fn output(&self, _name: &str) -> Option<OutputView<'_>> {
        None
    }
}

impl OutputResolver for BTreeMap<String, OutputView<'_>> {
    fn output(&self, name: &str) -> Option<OutputView<'_>> {
        self.get(name).copied()
    }
}

/// Everything an assertion may read besides its own output.
pub struct AssertionContext<'a> {
    pub output: OutputView<'a>,
    pub elapsed_ms: Option<u64>,
    /// "Now" in UTC, injected so freshness checks are deterministic.
    pub now: Option<NaiveDateTime>,
    pub outputs: &'a dyn OutputResolver,
    /// The baseline profile drift assertions compare against.
    pub baseline: Option<&'a ResultProfile>,
}

impl<'a> AssertionContext<'a> {
    pub fn new(output: OutputView<'a>) -> Self {
        Self {
            output,
            elapsed_ms: None,
            now: None,
            outputs: &NoOutputs,
            baseline: None,
        }
    }
}

/// Evaluate several assertions over one context.
pub fn evaluate_all(
    specs: &[AssertionSpec],
    context: &AssertionContext<'_>,
) -> Vec<AssertionOutcome> {
    specs.iter().map(|spec| evaluate(spec, context)).collect()
}

/// Evaluate one assertion.
pub fn evaluate(spec: &AssertionSpec, context: &AssertionContext<'_>) -> AssertionOutcome {
    let mut evidence = Evidence::default();
    let verdict = run(&spec.assertion, context, &mut evidence);
    let (status, issue) = match verdict {
        Verdict::Pass => (AssertionStatus::Pass, None),
        Verdict::Warn => (AssertionStatus::Warn, None),
        Verdict::WarnBecause(issue) => (AssertionStatus::Warn, Some(issue)),
        Verdict::Fail => (
            match spec.severity {
                Severity::Error => AssertionStatus::Fail,
                Severity::Warn => AssertionStatus::Warn,
            },
            None,
        ),
        Verdict::Error(issue) => (AssertionStatus::Error, Some(issue)),
        Verdict::Inconclusive(issue) => (AssertionStatus::Inconclusive, Some(issue)),
    };
    let incomplete = !context.output.completeness.is_complete();
    if incomplete && spec.assertion.depends_on_every_row() {
        evidence.partial = true;
    }
    let refuse = evidence.partial
        && !spec.allow_incomplete
        && matches!(
            status,
            AssertionStatus::Pass | AssertionStatus::Warn | AssertionStatus::Fail
        );
    let (status, issue) = if refuse {
        (
            AssertionStatus::Inconclusive,
            Some(AssertionIssue::IncompleteInput {
                completeness: context.output.completeness,
            }),
        )
    } else {
        (status, issue)
    };
    AssertionOutcome {
        name: spec.name.clone(),
        status,
        issue,
        evidence,
    }
}

enum Verdict {
    Pass,
    Warn,
    /// A warning with a reason the user should see.
    WarnBecause(AssertionIssue),
    Fail,
    Error(AssertionIssue),
    Inconclusive(AssertionIssue),
}

fn pass_if(ok: bool) -> Verdict {
    if ok { Verdict::Pass } else { Verdict::Fail }
}

/// Grade a metric against (warn, fail) levels where larger is worse.
fn grade(value: f64, warn: Option<f64>, fail: f64) -> Verdict {
    if value > fail {
        Verdict::Fail
    } else if warn.is_some_and(|warn| value > warn) {
        Verdict::Warn
    } else {
        Verdict::Pass
    }
}

fn column(view: &OutputView<'_>, name: &str) -> Result<usize, Verdict> {
    view.column_index(name).ok_or_else(|| {
        Verdict::Error(AssertionIssue::MissingColumn {
            column: name.to_string(),
        })
    })
}

macro_rules! column_or_return {
    ($view:expr, $name:expr) => {
        match column($view, $name) {
            Ok(index) => index,
            Err(verdict) => return verdict,
        }
    };
}

fn run(assertion: &Assertion, context: &AssertionContext<'_>, evidence: &mut Evidence) -> Verdict {
    let view = &context.output;
    match assertion {
        Assertion::RowCount { min, max } => {
            let rows = view.row_count();
            evidence.checked_rows = rows;
            evidence.observe("row_count", rows);
            pass_if(min.is_none_or(|min| rows >= min) && max.is_none_or(|max| rows <= max))
        }
        Assertion::Unique { columns } => {
            let mut indexes = Vec::with_capacity(columns.len());
            for name in columns {
                indexes.push(column_or_return!(view, name));
            }
            let mut seen: HashMap<(u64, u64), usize> = HashMap::new();
            let mut null_keys = 0u64;
            let mut duplicate_keys: HashSet<(u64, u64)> = HashSet::new();
            view.for_each_row(|index, row| {
                evidence.checked_rows += 1;
                let key: Vec<&CellValue> = indexes
                    .iter()
                    .map(|i| row.get(*i).unwrap_or(&CellValue::Null))
                    .collect();
                if key.iter().any(|cell| matches!(cell, CellValue::Null)) {
                    null_keys += 1;
                    return ControlFlow::Continue(());
                }
                let hash = (
                    cells_hash(key.iter().copied(), 11),
                    cells_hash(key.iter().copied(), 13),
                );
                if seen.insert(hash, index).is_some() {
                    duplicate_keys.insert(hash);
                    let display = if key.len() == 1 {
                        key[0].clone()
                    } else {
                        CellValue::String(
                            key.iter()
                                .map(|cell| cell_text(cell).unwrap_or_default().into_owned())
                                .collect::<Vec<_>>()
                                .join(", "),
                        )
                    };
                    evidence.fail_row(index, Some(&display));
                }
                ControlFlow::Continue(())
            });
            evidence.observe("duplicate_rows", evidence.failing_count);
            evidence.observe("duplicate_keys", duplicate_keys.len() as u64);
            evidence.observe("null_key_rows", null_keys);
            pass_if(evidence.failing_count == 0)
        }
        Assertion::NotNull {
            column: name,
            max_null_rate,
        } => {
            let index = column_or_return!(view, name);
            view.for_each_row(|row_index, row| {
                evidence.checked_rows += 1;
                if matches!(row.get(index), None | Some(CellValue::Null)) {
                    evidence.fail_row(row_index, None);
                }
                ControlFlow::Continue(())
            });
            let rate = if evidence.checked_rows == 0 {
                0.0
            } else {
                evidence.failing_count as f64 / evidence.checked_rows as f64
            };
            evidence.observe("null_count", evidence.failing_count);
            evidence.observe("null_rate", rate);
            pass_if(rate <= *max_null_rate + 1e-12)
        }
        Assertion::ValueRange {
            column: name,
            min,
            max,
            exclusive,
        } => {
            let index = column_or_return!(view, name);
            let (mut observed_min, mut observed_max) = (f64::INFINITY, f64::NEG_INFINITY);
            let mut non_numeric = 0u64;
            view.for_each_row(|row_index, row| {
                evidence.checked_rows += 1;
                let cell = row.get(index).unwrap_or(&CellValue::Null);
                if matches!(cell, CellValue::Null) {
                    return ControlFlow::Continue(());
                }
                let value = super::value::cell_number(cell);
                let Some(value) = value.filter(|v| !v.is_nan()) else {
                    non_numeric += 1;
                    evidence.fail_row(row_index, Some(cell));
                    return ControlFlow::Continue(());
                };
                observed_min = observed_min.min(value);
                observed_max = observed_max.max(value);
                let below = min.is_some_and(|min| {
                    if *exclusive {
                        value <= min
                    } else {
                        value < min
                    }
                });
                let above = max.is_some_and(|max| {
                    if *exclusive {
                        value >= max
                    } else {
                        value > max
                    }
                });
                if below || above {
                    evidence.fail_row(row_index, Some(cell));
                }
                ControlFlow::Continue(())
            });
            if observed_min <= observed_max {
                evidence.observe("observed_min", observed_min);
                evidence.observe("observed_max", observed_max);
            }
            evidence.observe("violations", evidence.failing_count);
            evidence.observe("non_numeric", non_numeric);
            pass_if(evidence.failing_count == 0)
        }
        Assertion::AcceptedValues {
            column: name,
            values,
            case_insensitive,
            allow_null,
        } => {
            let index = column_or_return!(view, name);
            let fold = |text: &str| {
                if *case_insensitive {
                    text.to_lowercase()
                } else {
                    text.to_string()
                }
            };
            let accepted: HashSet<String> = values.iter().map(|v| fold(v)).collect();
            view.for_each_row(|row_index, row| {
                evidence.checked_rows += 1;
                let cell = row.get(index).unwrap_or(&CellValue::Null);
                let ok = match cell_text(cell) {
                    None => *allow_null,
                    Some(text) => accepted.contains(&fold(&text)),
                };
                if !ok {
                    evidence.fail_row(row_index, Some(cell));
                }
                ControlFlow::Continue(())
            });
            evidence.observe("violations", evidence.failing_count);
            pass_if(evidence.failing_count == 0)
        }
        Assertion::Regex {
            column: name,
            pattern,
            full_match,
            allow_null,
        } => {
            let index = column_or_return!(view, name);
            let regex = match compile_pattern(pattern, *full_match) {
                Ok(regex) => regex,
                Err(issue) => return Verdict::Error(issue),
            };
            view.for_each_row(|row_index, row| {
                evidence.checked_rows += 1;
                let cell = row.get(index).unwrap_or(&CellValue::Null);
                let ok = match cell_text(cell) {
                    None => *allow_null,
                    Some(text) => regex.is_match(&text),
                };
                if !ok {
                    evidence.fail_row(row_index, Some(cell));
                }
                ControlFlow::Continue(())
            });
            evidence.observe("violations", evidence.failing_count);
            pass_if(evidence.failing_count == 0)
        }
        Assertion::Referential {
            column: name,
            parent,
            parent_column,
        } => {
            let index = column_or_return!(view, name);
            let Some(parent_view) = context.outputs.output(parent) else {
                return Verdict::Error(AssertionIssue::MissingOutput {
                    name: parent.clone(),
                });
            };
            let parent_index = column_or_return!(&parent_view, parent_column);
            let mut keys: HashSet<JoinKey> = HashSet::new();
            parent_view.for_each_row(|_, row| {
                if let Some(key) = row.get(parent_index).and_then(join_key) {
                    keys.insert(key);
                }
                ControlFlow::Continue(())
            });
            let mut orphan_values: HashSet<JoinKey> = HashSet::new();
            view.for_each_row(|row_index, row| {
                evidence.checked_rows += 1;
                let cell = row.get(index).unwrap_or(&CellValue::Null);
                if let Some(key) = join_key(cell)
                    && !keys.contains(&key)
                {
                    orphan_values.insert(key);
                    evidence.fail_row(row_index, Some(cell));
                }
                ControlFlow::Continue(())
            });
            evidence.observe("parent_keys", keys.len() as u64);
            evidence.observe("orphan_rows", evidence.failing_count);
            evidence.observe("orphan_values", orphan_values.len() as u64);
            if !parent_view.completeness.is_complete() {
                // Missing parent rows would make children look orphaned.
                return Verdict::Inconclusive(AssertionIssue::IncompleteParent {
                    completeness: parent_view.completeness,
                });
            }
            pass_if(evidence.failing_count == 0)
        }
        Assertion::DistributionDrift {
            column: name,
            metric,
            warn,
            fail,
        } => {
            let index = column_or_return!(view, name);
            let Some(baseline) = context.baseline else {
                return Verdict::Error(AssertionIssue::MissingBaseline);
            };
            let Some(base) = baseline.column(name) else {
                return Verdict::Error(AssertionIssue::MissingBaselineColumn {
                    column: name.clone(),
                });
            };
            let (default_warn, default_fail) = metric.default_levels();
            let warn = warn.or(Some(default_warn));
            let fail = fail.unwrap_or(default_fail);
            let value = match drift(view, index, base, *metric, evidence) {
                Ok(value) => value,
                Err(issue) => return Verdict::Error(issue),
            };
            evidence.observe("drift", value);
            evidence.observe("metric", metric_name(*metric));
            if !baseline.completeness.is_complete() {
                return Verdict::Inconclusive(AssertionIssue::IncompleteBaseline {
                    completeness: baseline.completeness,
                });
            }
            grade(value, warn, fail)
        }
        Assertion::Freshness {
            column: name,
            max_age_seconds,
            warn_age_seconds,
            time_zone,
        } => {
            let index = column_or_return!(view, name);
            let Some(now) = context.now else {
                return Verdict::Error(AssertionIssue::NoClock);
            };
            let zone = match time_zone
                .as_deref()
                .map(str::trim)
                .filter(|z| !z.is_empty())
            {
                None => Zone::Utc,
                Some(text) => match Zone::parse(text) {
                    Some(zone) => zone,
                    None => {
                        return Verdict::Error(AssertionIssue::InvalidTimeZone {
                            zone: text.to_string(),
                        });
                    }
                },
            };
            let mut newest: Option<NaiveDateTime> = None;
            view.for_each_row(|_, row| {
                evidence.checked_rows += 1;
                if let Some(stamp) = row.get(index).and_then(|cell| zone.instant(cell)) {
                    newest = Some(newest.map_or(stamp, |n| n.max(stamp)));
                }
                ControlFlow::Continue(())
            });
            let Some(newest) = newest else {
                return Verdict::Error(AssertionIssue::NoTimestamps);
            };
            let age = epoch_seconds(now) - epoch_seconds(newest);
            evidence.observe(
                "newest",
                newest.format("%Y-%m-%dT%H:%M:%S").to_string().as_str(),
            );
            evidence.observe("age_seconds", Metric::Integer(age));
            if age < 0 {
                return Verdict::WarnBecause(AssertionIssue::FutureTimestamp);
            }
            let age = age as f64;
            grade(
                age,
                warn_age_seconds.map(|w| w as f64),
                *max_age_seconds as f64,
            )
        }
        Assertion::Duration { max_ms, warn_ms } => {
            let Some(elapsed) = context.elapsed_ms else {
                return Verdict::Error(AssertionIssue::NoDuration);
            };
            evidence.observe("elapsed_ms", elapsed);
            grade(elapsed as f64, warn_ms.map(|w| w as f64), *max_ms as f64)
        }
        Assertion::KpiThreshold {
            value,
            op,
            threshold,
        } => match evaluate_value(view, value) {
            Ok(observed) => {
                evidence.observe("value", observed);
                evidence.observe("threshold", *threshold);
                pass_if(op.holds(observed, *threshold))
            }
            Err(error) => Verdict::Error(AssertionIssue::Kpi { error }),
        },
    }
}

fn metric_name(metric: DriftMetric) -> &'static str {
    match metric {
        DriftMetric::Psi => "psi",
        DriftMetric::Ks => "ks",
        DriftMetric::MeanShift => "mean_shift",
    }
}

/// Compile a user pattern with length and size limits.
pub fn compile_pattern(pattern: &str, full_match: bool) -> Result<regex::Regex, AssertionIssue> {
    if pattern.chars().count() > MAX_PATTERN_CHARS {
        return Err(AssertionIssue::PatternTooLong {
            max_chars: MAX_PATTERN_CHARS,
        });
    }
    let source = if full_match {
        format!("^(?:{pattern})$")
    } else {
        pattern.to_string()
    };
    regex::RegexBuilder::new(&source)
        .size_limit(PATTERN_SIZE_LIMIT)
        .dfa_size_limit(PATTERN_SIZE_LIMIT)
        .nest_limit(64)
        .build()
        .map_err(|error| AssertionIssue::InvalidPattern {
            detail: truncate_chars(&error.to_string(), 200).into_owned(),
        })
}

/// The zone a freshness check reads zone-less timestamps in.
enum Zone {
    Utc,
    Fixed(i64),
    Named(chrono_tz::Tz),
}

impl Zone {
    fn parse(text: &str) -> Option<Zone> {
        if text.eq_ignore_ascii_case("utc") || text.eq_ignore_ascii_case("z") {
            return Some(Zone::Utc);
        }
        if let Some(rest) = text.strip_prefix(['+', '-']) {
            let sign = if text.starts_with('-') { -1 } else { 1 };
            let digits: String = rest.chars().filter(|c| *c != ':').collect();
            if !(digits.len() == 2 || digits.len() == 4)
                || !digits.bytes().all(|b| b.is_ascii_digit())
            {
                return None;
            }
            let hours: i64 = digits[..2].parse().ok()?;
            let minutes: i64 = digits.get(2..).map_or(Some(0), |m| m.parse().ok())?;
            if hours > 18 || minutes > 59 {
                return None;
            }
            return Some(Zone::Fixed(sign * (hours * 3_600 + minutes * 60)));
        }
        text.parse::<chrono_tz::Tz>().ok().map(Zone::Named)
    }

    /// The UTC instant of a timestamp cell: its own offset when it has one,
    /// else read as wall-clock time in this zone.
    fn instant(&self, cell: &CellValue) -> Option<NaiveDateTime> {
        let CellValue::String(text) = cell else {
            return None;
        };
        let zoned = parse_zoned(text)?;
        if zoned.offset_seconds.is_some() {
            return Some(zoned.utc());
        }
        match self {
            Zone::Utc => Some(zoned.local),
            Zone::Fixed(offset) => zoned
                .local
                .checked_sub_signed(chrono::Duration::seconds(*offset)),
            Zone::Named(tz) => {
                use chrono::TimeZone;
                tz.from_local_datetime(&zoned.local)
                    .earliest()
                    .map(|t| t.naive_utc())
            }
        }
    }
}

/// A join key for referential checks: numbers by exact value (so `1`
/// matches `1.0` but a 19-digit id never matches its float neighbour),
/// text exactly, other values by type and content; NULL has none.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum JoinKey {
    Number(Decimal),
    /// NaN and infinities.
    Float(u64),
    Text(String),
    Other(u64),
}

fn join_key(cell: &CellValue) -> Option<JoinKey> {
    Some(match cell {
        CellValue::Null => return None,
        CellValue::I64(n) => JoinKey::Number(Decimal::from_i64(*n)),
        CellValue::F64(f) => match Decimal::from_f64(*f) {
            Some(d) => JoinKey::Number(d),
            None => JoinKey::Float(cell_hash(cell, 17)),
        },
        CellValue::String(t) => JoinKey::Text(t.clone()),
        other => JoinKey::Other(cell_hash(other, 17)),
    })
}

fn drift(
    view: &OutputView<'_>,
    index: usize,
    base: &ColumnProfile,
    metric: DriftMetric,
    evidence: &mut Evidence,
) -> Result<f64, AssertionIssue> {
    const EPSILON: f64 = 1e-4;
    let numeric = base
        .numeric
        .as_ref()
        .filter(|n| !n.histogram.counts.is_empty());
    match (metric, numeric) {
        (DriftMetric::Psi | DriftMetric::Ks, Some(stats)) => {
            let histogram = &stats.histogram;
            let mut current = vec![0u64; histogram.counts.len()];
            let mut total = 0u64;
            view.for_each_row(|_, row| {
                evidence.checked_rows += 1;
                if let Some(value) = row.get(index).and_then(finite_number) {
                    current[histogram.bin_of(value)] += 1;
                    total += 1;
                }
                ControlFlow::Continue(())
            });
            let base_total: u64 = histogram.counts.iter().sum();
            if total == 0 || base_total == 0 {
                return Err(AssertionIssue::NoNumericValues);
            }
            evidence.observe("current_values", total);
            evidence.observe("baseline_values", base_total);
            let expected: Vec<f64> = histogram
                .counts
                .iter()
                .map(|c| *c as f64 / base_total as f64)
                .collect();
            let actual: Vec<f64> = current.iter().map(|c| *c as f64 / total as f64).collect();
            Ok(match metric {
                DriftMetric::Psi => psi(&expected, &actual, EPSILON),
                _ => ks(&expected, &actual),
            })
        }
        (DriftMetric::MeanShift, Some(stats)) => {
            let mut accumulator = super::kpi::AggregateAccumulator::new(super::kpi::Aggregate::Avg);
            view.for_each_row(|_, row| {
                evidence.checked_rows += 1;
                accumulator.push(row.get(index).unwrap_or(&CellValue::Null));
                ControlFlow::Continue(())
            });
            let mean = accumulator
                .finish()
                .ok_or(AssertionIssue::NoNumericValues)?;
            evidence.observe("current_mean", mean);
            evidence.observe("baseline_mean", stats.mean);
            let shift = (mean - stats.mean).abs();
            Ok(if stats.stddev > 0.0 {
                shift / stats.stddev
            } else if stats.mean != 0.0 {
                shift / stats.mean.abs()
            } else {
                shift
            })
        }
        (DriftMetric::Psi, None) if !base.top_values.is_empty() => {
            let categories: Vec<&str> = base.top_values.iter().map(|v| v.value.as_str()).collect();
            let base_non_null = base.non_null_count().max(1);
            let mut expected: Vec<f64> = base
                .top_values
                .iter()
                .map(|v| v.count as f64 / base_non_null as f64)
                .collect();
            expected.push((1.0 - expected.iter().sum::<f64>()).max(0.0));
            let mut current = vec![0u64; categories.len() + 1];
            let mut total = 0u64;
            view.for_each_row(|_, row| {
                evidence.checked_rows += 1;
                if let Some(text) = row.get(index).and_then(cell_text) {
                    let truncated = truncate_chars(&text, 64);
                    let slot = categories
                        .iter()
                        .position(|c| *c == truncated.as_ref())
                        .unwrap_or(categories.len());
                    current[slot] += 1;
                    total += 1;
                }
                ControlFlow::Continue(())
            });
            if total == 0 {
                return Err(AssertionIssue::NoNumericValues);
            }
            let actual: Vec<f64> = current.iter().map(|c| *c as f64 / total as f64).collect();
            Ok(psi(&expected, &actual, EPSILON))
        }
        _ => Err(AssertionIssue::UnsupportedMetric),
    }
}

/// Population stability index; empty bins are floored at `epsilon`.
pub fn psi(expected: &[f64], actual: &[f64], epsilon: f64) -> f64 {
    expected
        .iter()
        .zip(actual)
        .map(|(e, a)| {
            let (e, a) = (e.max(epsilon), a.max(epsilon));
            (a - e) * (a / e).ln()
        })
        .sum()
}

/// Largest gap between two binned CDFs.
pub fn ks(expected: &[f64], actual: &[f64]) -> f64 {
    let (mut ce, mut ca, mut worst) = (0.0f64, 0.0f64, 0.0f64);
    for (e, a) in expected.iter().zip(actual) {
        ce += e;
        ca += a;
        worst = worst.max((ce - ca).abs());
    }
    worst
}

#[cfg(test)]
#[path = "assertions_tests.rs"]
mod tests;
