//! Assertion (test) cell spec, the same shape the CLI runner reads:
//!
//! ```json
//! {"input": "orders", "baseline": "previous_run",
//!  "assertions": [{"kind": "row_count", "min": 1}, {"kind": "unique", "columns": ["id"]}]}
//! ```
//!
//! `baseline` is `"previous_run"` (the input's output in the last recorded
//! run that has one) or `{"snapshot": "<name>"}` (a pinned snapshot). A
//! single assertion may also be written inline next to `input`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlly_core::{CellValue, ResultColumn};

use super::{input_of, unknown_fields};
use crate::analytics::OutputView;
use crate::analytics::assertions::{
    Assertion, AssertionOutcome, AssertionSpec, AssertionStatus, CompareOp, DriftMetric,
};
use crate::analytics::kpi::ValueSource;

const KNOWN: &[&str] = &["input", "inputs", "baseline", "assertions"];

/// Where a drift assertion's baseline comes from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BaselineRef {
    #[default]
    None,
    PreviousRun,
    Snapshot(String),
}

impl BaselineRef {
    fn parse(value: Option<&Value>) -> Self {
        match value {
            Some(Value::String(s)) if s == "previous_run" => BaselineRef::PreviousRun,
            Some(Value::Object(m)) => m
                .get("snapshot")
                .and_then(Value::as_str)
                .map(|n| BaselineRef::Snapshot(n.to_string()))
                .unwrap_or_default(),
            _ => BaselineRef::None,
        }
    }

    fn to_value(&self) -> Option<Value> {
        match self {
            BaselineRef::None => None,
            BaselineRef::PreviousRun => Some(Value::String("previous_run".into())),
            BaselineRef::Snapshot(name) => Some(serde_json::json!({ "snapshot": name })),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct AssertionCellSpec {
    pub input: String,
    pub assertions: Vec<AssertionSpec>,
    pub baseline: BaselineRef,
    /// Assertions that did not parse, kept verbatim with their position so
    /// a save never drops them.
    pub invalid: Vec<(usize, Value)>,
    pub extra: Map<String, Value>,
}

impl AssertionCellSpec {
    pub fn from_spec(spec: &Value) -> Self {
        let mut out = Self {
            input: input_of(spec),
            baseline: BaselineRef::parse(spec.get("baseline")),
            ..Self::default()
        };
        match spec.get("assertions") {
            Some(Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    match serde_json::from_value::<AssertionSpec>(item.clone()) {
                        Ok(a) => out.assertions.push(a),
                        Err(_) => out.invalid.push((i, item.clone())),
                    }
                }
                out.extra = unknown_fields(spec, KNOWN);
            }
            _ => {
                let inline = Value::Object(unknown_fields(spec, KNOWN));
                if inline.get("kind").is_some() {
                    match serde_json::from_value::<AssertionSpec>(inline.clone()) {
                        Ok(a) => out.assertions.push(a),
                        Err(_) => out.invalid.push((0, inline)),
                    }
                } else {
                    out.extra = unknown_fields(spec, KNOWN);
                }
            }
        }
        out
    }

    /// Always written as an `assertions` list.
    pub fn to_spec(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("input".into(), Value::String(self.input.clone()));
        if let Some(b) = self.baseline.to_value() {
            map.insert("baseline".into(), b);
        }
        let mut items: Vec<Value> = self
            .assertions
            .iter()
            .map(|a| serde_json::to_value(a).unwrap_or(Value::Null))
            .collect();
        for (i, raw) in &self.invalid {
            items.insert((*i).min(items.len()), raw.clone());
        }
        map.insert("assertions".into(), Value::Array(items));
        Value::Object(map)
    }

    /// [`Self::from_spec`] for a headless runner: anything a lenient read
    /// would keep aside or ignore is a problem.
    pub fn parse(spec: &Value) -> Result<Self, AssertionSpecProblem> {
        let parsed = Self::from_spec(spec);
        if parsed.input.is_empty() {
            return Err(AssertionSpecProblem::NoInput);
        }
        match spec.get("baseline") {
            None | Some(Value::Null) => {}
            Some(_) if parsed.baseline != BaselineRef::None => {}
            Some(_) => return Err(AssertionSpecProblem::BadBaseline),
        }
        if spec.get("assertions").is_some_and(|a| !a.is_array()) {
            return Err(AssertionSpecProblem::NotAList);
        }
        if let Some((index, raw)) = parsed.invalid.first() {
            let reason = serde_json::from_value::<AssertionSpec>(raw.clone())
                .err()
                .map(|e| e.to_string())
                .unwrap_or_default();
            return Err(AssertionSpecProblem::Invalid {
                index: *index,
                reason,
            });
        }
        if parsed.assertions.is_empty() {
            return Err(AssertionSpecProblem::Empty);
        }
        Ok(parsed)
    }

    pub fn needs_baseline(&self) -> bool {
        self.assertions
            .iter()
            .any(|a| matches!(a.assertion, Assertion::DistributionDrift { .. }))
    }

    /// Other outputs the assertions read (referential parents), by name.
    pub fn parents(&self) -> Vec<String> {
        self.assertions
            .iter()
            .filter_map(|a| match &a.assertion {
                Assertion::Referential { parent, .. } => Some(parent.clone()),
                _ => None,
            })
            .collect()
    }
}

/// Why [`AssertionCellSpec::parse`] refused a spec. `reason` is the JSON
/// reader's own wording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssertionSpecProblem {
    NoInput,
    BadBaseline,
    NotAList,
    Invalid { index: usize, reason: String },
    Empty,
}

/// The kinds an editor offers, by their serde tag.
pub const ASSERTION_KINDS: [&str; 11] = [
    "row_count",
    "unique",
    "not_null",
    "value_range",
    "accepted_values",
    "regex",
    "referential",
    "distribution_drift",
    "freshness",
    "duration",
    "kpi_threshold",
];

/// The serde tag of an assertion's kind.
pub fn kind_of(assertion: &Assertion) -> &'static str {
    match assertion {
        Assertion::RowCount { .. } => "row_count",
        Assertion::Unique { .. } => "unique",
        Assertion::NotNull { .. } => "not_null",
        Assertion::ValueRange { .. } => "value_range",
        Assertion::AcceptedValues { .. } => "accepted_values",
        Assertion::Regex { .. } => "regex",
        Assertion::Referential { .. } => "referential",
        Assertion::DistributionDrift { .. } => "distribution_drift",
        Assertion::Freshness { .. } => "freshness",
        Assertion::Duration { .. } => "duration",
        Assertion::KpiThreshold { .. } => "kpi_threshold",
    }
}

/// A starting assertion of `kind` over `column` for the editor.
pub fn new_assertion(kind: &str, column: &str) -> Option<Assertion> {
    let column = column.to_string();
    Some(match kind {
        "row_count" => Assertion::RowCount {
            min: Some(1),
            max: None,
        },
        "unique" => Assertion::Unique {
            columns: vec![column],
        },
        "not_null" => Assertion::NotNull {
            column,
            max_null_rate: 0.0,
        },
        "value_range" => Assertion::ValueRange {
            column,
            min: Some(0.0),
            max: None,
            exclusive: false,
        },
        "accepted_values" => Assertion::AcceptedValues {
            column,
            values: Vec::new(),
            case_insensitive: false,
            allow_null: true,
        },
        "regex" => Assertion::Regex {
            column,
            pattern: ".+".into(),
            full_match: true,
            allow_null: true,
        },
        "referential" => Assertion::Referential {
            column: column.clone(),
            parent: String::new(),
            parent_column: column,
        },
        "distribution_drift" => Assertion::DistributionDrift {
            column,
            metric: DriftMetric::Psi,
            warn: None,
            fail: None,
        },
        "freshness" => Assertion::Freshness {
            column,
            max_age_seconds: 86_400,
            warn_age_seconds: None,
            time_zone: None,
        },
        "duration" => Assertion::Duration {
            max_ms: 5_000,
            warn_ms: None,
        },
        "kpi_threshold" => Assertion::KpiThreshold {
            value: ValueSource::RowCount,
            op: CompareOp::Ge,
            threshold: 1.0,
        },
        _ => return None,
    })
}

/// Counts of outcomes by status, for the cell header and the notebook's
/// test summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TestSummary {
    pub passed: u32,
    pub warned: u32,
    pub failed: u32,
    pub errors: u32,
    pub inconclusive: u32,
}

impl TestSummary {
    pub fn of<'a>(outcomes: impl IntoIterator<Item = &'a AssertionOutcome>) -> Self {
        let mut s = Self::default();
        for o in outcomes {
            s.add(o.status);
        }
        s
    }

    pub fn add(&mut self, status: AssertionStatus) {
        match status {
            AssertionStatus::Pass => self.passed += 1,
            AssertionStatus::Warn => self.warned += 1,
            AssertionStatus::Fail => self.failed += 1,
            AssertionStatus::Error => self.errors += 1,
            AssertionStatus::Inconclusive => self.inconclusive += 1,
        }
    }

    pub fn merge(&mut self, other: TestSummary) {
        self.passed += other.passed;
        self.warned += other.warned;
        self.failed += other.failed;
        self.errors += other.errors;
        self.inconclusive += other.inconclusive;
    }

    pub fn total(&self) -> u32 {
        self.passed + self.warned + self.failed + self.errors + self.inconclusive
    }

    /// Failed or could not run.
    pub fn failing(&self) -> u32 {
        self.failed + self.errors
    }
}

/// Up to this many failing rows are kept per assertion as evidence.
pub const SAMPLE_ROWS: usize = 20;

/// The rows behind an outcome's `failing_rows`, kept with the run so the
/// evidence outlives the output.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct FailingSample {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
}

impl FailingSample {
    pub fn of(outcome: &AssertionOutcome, input: &OutputView<'_>) -> Option<Self> {
        if outcome.evidence.failing_rows.is_empty() {
            return None;
        }
        let wanted: Vec<usize> = outcome
            .evidence
            .failing_rows
            .iter()
            .copied()
            .take(SAMPLE_ROWS)
            .collect();
        let last = *wanted.iter().max()?;
        let mut found: BTreeMap<usize, Vec<CellValue>> = BTreeMap::new();
        input.for_each_row(|i, row| {
            if wanted.contains(&i) {
                found.insert(i, row.to_vec());
            }
            if i >= last {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        });
        Some(Self {
            columns: input.columns.to_vec(),
            rows: wanted.iter().filter_map(|i| found.remove(i)).collect(),
        })
    }
}

/// One assertion cell's results as kept in a run record.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AssertionRun {
    pub outcomes: Vec<AssertionOutcome>,
    /// Failing-row samples by assertion position.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub samples: BTreeMap<usize, FailingSample>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::assertions::{AssertionContext, evaluate};
    use crate::analytics::test_support::{col, i};
    use crate::analytics::{Completeness, OutputView};
    use serde_json::json;

    #[test]
    fn cli_shapes_parse_and_save_as_a_list() {
        let raw = json!({"input": "orders", "baseline": "previous_run", "assertions": [
            {"kind": "row_count", "min": 1},
            {"kind": "nope"},
            {"kind": "unique", "columns": ["id"], "severity": "warn"}
        ]});
        let spec = AssertionCellSpec::from_spec(&raw);
        assert_eq!(spec.baseline, BaselineRef::PreviousRun);
        assert_eq!(spec.assertions.len(), 2);
        assert_eq!(spec.invalid.len(), 1);
        let back = spec.to_spec();
        assert_eq!(back["assertions"].as_array().unwrap().len(), 3);
        assert_eq!(back["assertions"][1], json!({"kind": "nope"}));
        assert_eq!(AssertionCellSpec::from_spec(&back), spec);

        let inline = AssertionCellSpec::from_spec(
            &json!({"input": {"cell": "o"}, "kind": "not_null", "column": "id",
                    "baseline": {"snapshot": "golden"}}),
        );
        assert_eq!(inline.input, "o");
        assert_eq!(inline.baseline, BaselineRef::Snapshot("golden".into()));
        assert_eq!(kind_of(&inline.assertions[0].assertion), "not_null");
    }

    #[test]
    fn strict_parsing_names_what_a_lenient_read_would_keep_aside() {
        let ok = AssertionCellSpec::parse(&json!({"input": {"cell": "orders"},
            "baseline": {"snapshot": "monday"}, "kind": "not_null", "column": "id"}))
        .unwrap();
        assert_eq!(ok.input, "orders");
        assert_eq!(ok.baseline, BaselineRef::Snapshot("monday".into()));
        let cases = [
            (json!({"kind": "row_count"}), AssertionSpecProblem::NoInput),
            (
                json!({"input": "o", "assertions": []}),
                AssertionSpecProblem::Empty,
            ),
            (
                json!({"input": "o", "assertions": {}}),
                AssertionSpecProblem::NotAList,
            ),
            (json!({"input": "o"}), AssertionSpecProblem::Empty),
            (
                json!({"input": "o", "kind": "row_count", "baseline": "yesterday"}),
                AssertionSpecProblem::BadBaseline,
            ),
        ];
        for (spec, problem) in cases {
            assert_eq!(AssertionCellSpec::parse(&spec), Err(problem), "{spec}");
        }
        let bad = AssertionCellSpec::parse(&json!({"input": "o", "assertions": [
            {"kind": "row_count", "min": 1}, {"kind": "no_such_check"}]}));
        assert!(
            matches!(bad, Err(AssertionSpecProblem::Invalid { index: 1, ref reason }) if !reason.is_empty()),
            "{bad:?}"
        );
    }

    #[test]
    fn every_editor_kind_has_a_template_that_round_trips() {
        for kind in ASSERTION_KINDS {
            let a = new_assertion(kind, "c").unwrap();
            assert_eq!(kind_of(&a), kind);
            let spec = AssertionSpec::from(a);
            let json = serde_json::to_value(&spec).unwrap();
            assert_eq!(serde_json::from_value::<AssertionSpec>(json).unwrap(), spec);
        }
        assert!(new_assertion("zzz", "c").is_none());
    }

    #[test]
    fn incomplete_input_is_inconclusive_and_samples_failing_rows() {
        let cols = [col("id", "int")];
        let rows: Vec<_> = [1, 2, 2, 3].into_iter().map(|n| vec![i(n)]).collect();
        let unique = AssertionSpec::from(Assertion::Unique {
            columns: vec!["id".into()],
        });
        let cut = AssertionContext::new(OutputView::new(
            &cols,
            &rows,
            Completeness::Truncated { cap: 4 },
        ));
        assert_eq!(
            evaluate(&unique, &cut).status,
            AssertionStatus::Inconclusive
        );
        let view = OutputView::complete(&cols, &rows);
        let outcome = evaluate(&unique, &AssertionContext::new(view));
        assert_eq!(outcome.status, AssertionStatus::Fail);
        let sample = FailingSample::of(&outcome, &view).unwrap();
        assert!(!sample.rows.is_empty() && sample.rows.iter().all(|r| r[0] == i(2)));
        let summary = TestSummary::of([&outcome]);
        assert_eq!(
            (summary.failed, summary.failing(), summary.total()),
            (1, 1, 1)
        );
    }
}
