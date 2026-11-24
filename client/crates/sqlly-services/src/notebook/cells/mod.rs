//! Typed `spec` shapes of the notebook's non-SQL cell kinds, shared by the
//! app's cell renderers and the CLI runner. Each module reads its kind's
//! `Cell::spec` JSON without losing fields it does not know.

pub mod ai;
pub mod assertion;
pub mod attachment;
pub mod chart;
pub mod diff;
pub mod entity_detail;
pub mod kpi;
pub mod layout;
pub mod local;
pub mod parameter;
pub mod pivot;
pub mod plan;
pub mod profile;
pub mod scalar;
pub mod spec;
pub mod table;
pub mod transform;

use serde_json::{Map, Value};

use sqlly_core::CellValue;

use super::output::{CellOutput, Completeness as OutputCompleteness};
use crate::analytics::{Completeness, OutputView};

/// The one conversion from a cell output's completeness to the analytics
/// module's. They differ in one place: an output is `Spilled` when rows
/// beyond its in-memory snapshot went to a spill file, while analytics
/// `Spilled` means the analysis read every one of those rows. So a spilled
/// output analysed from fewer than `total` rows is `Truncated` at
/// `rows_read`, and `DerivedFromPartial` (every row came back, but a bound
/// value was computed from incomplete data) is `Partial`.
pub fn analytics_completeness(completeness: OutputCompleteness, rows_read: usize) -> Completeness {
    match completeness {
        OutputCompleteness::Complete => Completeness::Complete,
        OutputCompleteness::Truncated { cap } => Completeness::Truncated { cap },
        OutputCompleteness::Spilled { total } if rows_read as u64 >= total => {
            Completeness::Spilled { total }
        }
        OutputCompleteness::Spilled { .. } => Completeness::Truncated {
            cap: rows_read as u64,
        },
        OutputCompleteness::Sampled { n } => Completeness::Sampled { n },
        OutputCompleteness::Partial | OutputCompleteness::DerivedFromPartial => {
            Completeness::Partial
        }
    }
}

/// A cell output's in-memory snapshot as an analytics view.
pub fn output_view(output: &CellOutput) -> OutputView<'_> {
    output_view_of(output, &output.rows)
}

/// `rows` taken from `output`'s snapshot (a cross-filtered subset, say) as
/// an analytics view; completeness follows the snapshot, not the subset.
pub fn output_view_of<'a>(output: &'a CellOutput, rows: &'a [Vec<CellValue>]) -> OutputView<'a> {
    OutputView::new(
        &output.columns,
        rows,
        analytics_completeness(output.completeness, output.rows.len()),
    )
}

/// `spec` with the keys in `known` removed: what a typed spec keeps aside
/// and writes back untouched.
pub(crate) fn unknown_fields(spec: &Value, known: &[&str]) -> Map<String, Value> {
    match spec {
        Value::Object(map) => map
            .iter()
            .filter(|(k, _)| !known.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => Map::new(),
    }
}

/// The single input id of a spec (`input`, or the first of `inputs`).
pub(crate) fn input_of(spec: &Value) -> String {
    let pick = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => m.get("cell").and_then(Value::as_str).map(str::to_string),
        _ => None,
    };
    spec.get("input")
        .and_then(pick)
        .or_else(|| {
            spec.get("inputs")
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(pick)
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn spilled_outputs_read_from_memory_are_truncated() {
        assert_eq!(
            analytics_completeness(OutputCompleteness::Spilled { total: 100 }, 100),
            Completeness::Spilled { total: 100 }
        );
        assert_eq!(
            analytics_completeness(OutputCompleteness::Spilled { total: 100 }, 10),
            Completeness::Truncated { cap: 10 }
        );
        assert_eq!(
            analytics_completeness(OutputCompleteness::Partial, 3),
            Completeness::Partial
        );
    }

    #[test]
    fn a_kpi_over_a_spilled_snapshot_is_inconclusive() {
        use crate::analytics::kpi::{Aggregate, KpiSpec, ValueSource, evaluate_kpi};
        let columns = vec![sqlly_core::ResultColumn {
            name: "amount".into(),
            data_type: "int".into(),
            nullable: false,
        }];
        let rows = (0..10).map(|i| vec![CellValue::I64(i)]).collect();
        let mut output = CellOutput::capped(
            columns,
            rows,
            None,
            usize::MAX,
            1,
            "r_1",
            super::super::fingerprint::Fingerprint("f".into()),
            Default::default(),
        );
        output.completeness = OutputCompleteness::Spilled { total: 60_000 };
        output.row_count = 60_000;
        let view = output_view(&output);
        assert!(!view.completeness.is_complete());
        let spec = KpiSpec {
            value: ValueSource::Aggregate {
                column: "amount".into(),
                aggregate: Aggregate::Sum,
            },
            comparison: None,
            direction: Default::default(),
            thresholds: None,
            sparkline: None,
            format: None,
            allow_incomplete: false,
        };
        assert!(evaluate_kpi(&view, &spec, None).unwrap().inconclusive);
        let filtered = vec![output.rows[0].clone()];
        assert!(
            !output_view_of(&output, &filtered)
                .completeness
                .is_complete()
        );
    }

    #[test]
    fn inputs_read_from_either_field() {
        assert_eq!(input_of(&json!({"input": "a"})), "a");
        assert_eq!(input_of(&json!({"input": {"cell": "b"}})), "b");
        assert_eq!(input_of(&json!({"inputs": ["c", "d"]})), "c");
        assert_eq!(input_of(&json!({})), "");
    }
}
