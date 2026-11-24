//! Assertion and KPI cell specs, read strictly through the shared
//! [`sqlly_services::notebook::cells::assertion`] and
//! [`sqlly_services::notebook::cells::kpi`] types so the command line and
//! the app agree on every shape, with problems worded for the terminal.
//!
//! `baseline: "previous_run"` compares against the input cell's output in
//! the previous recorded run (for `comparison: {"kind": "baseline"}` and
//! drift assertions). A KPI `baseline` naming another cell reads that
//! cell's output from this run.

use sqlly_services::notebook::Cell;
use sqlly_services::notebook::cells::assertion::{
    AssertionCellSpec, AssertionSpecProblem, BaselineRef,
};
use sqlly_services::notebook::cells::kpi::{KpiCellSpec, KpiSpecProblem};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecError(pub String);

const NO_INPUT: &str = "needs an `input` cell";

pub fn assertion_spec(cell: &Cell) -> Result<AssertionCellSpec, SpecError> {
    let spec = AssertionCellSpec::parse(&cell.spec).map_err(|problem| {
        SpecError(match problem {
            AssertionSpecProblem::NoInput => NO_INPUT.into(),
            AssertionSpecProblem::BadBaseline => {
                "`baseline` must be \"previous_run\" or {\"snapshot\": \"<name>\"}".into()
            }
            AssertionSpecProblem::NotAList => "`assertions` must be a list".into(),
            AssertionSpecProblem::Invalid { index, reason } => {
                format!("assertion {}: {reason}", index + 1)
            }
            AssertionSpecProblem::Empty => "has no assertions".into(),
        })
    })?;
    if let BaselineRef::Snapshot(name) = &spec.baseline {
        return Err(SpecError(format!(
            "baseline snapshot \"{name}\" is kept by the app; use \"previous_run\" on the command line"
        )));
    }
    Ok(spec)
}

pub fn kpi_spec(cell: &Cell) -> Result<KpiCellSpec, SpecError> {
    KpiCellSpec::parse(&cell.spec).map_err(|problem| {
        SpecError(match problem {
            KpiSpecProblem::NoInput => NO_INPUT.into(),
            KpiSpecProblem::Invalid { reason } => reason,
            KpiSpecProblem::NoTiles => "has no KPI tiles".into(),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use sqlly_services::analytics::assertions::Assertion;
    use sqlly_services::notebook::{CellKind, ExecutionPolicy};

    fn cell(kind: CellKind, spec: Value) -> Cell {
        let mut cell = Cell::new("c_1", kind, ExecutionPolicy::default());
        cell.spec = spec;
        cell
    }

    #[test]
    fn assertion_lists_and_inline_assertions_parse() {
        let list = assertion_spec(&cell(
            CellKind::Assertion,
            json!({"input": "orders", "baseline": "previous_run", "assertions": [
                {"kind": "row_count", "min": 1},
                {"kind": "unique", "columns": ["id"], "severity": "warn"}
            ]}),
        ))
        .unwrap();
        assert_eq!(list.input, "orders");
        assert_eq!(list.baseline, BaselineRef::PreviousRun);
        assert_eq!(list.assertions.len(), 2);
        let inline = assertion_spec(&cell(
            CellKind::Assertion,
            json!({"input": {"cell": "orders"}, "kind": "not_null", "column": "id"}),
        ))
        .unwrap();
        assert!(matches!(
            inline.assertions[0].assertion,
            Assertion::NotNull { .. }
        ));
    }

    #[test]
    fn bad_assertion_specs_say_why() {
        for spec in [
            json!({"kind": "row_count"}),
            json!({"input": "o", "assertions": []}),
            json!({"input": "o", "assertions": {}}),
            json!({"input": "o", "kind": "no_such_check"}),
            json!({"input": "o", "kind": "row_count", "baseline": "yesterday"}),
            json!({"input": "o", "kind": "row_count", "baseline": {"snapshot": "monday"}}),
        ] {
            assert!(
                assertion_spec(&cell(CellKind::Assertion, spec.clone())).is_err(),
                "{spec}"
            );
        }
        let error = assertion_spec(&cell(
            CellKind::Assertion,
            json!({"input": "o", "assertions": [{"kind": "row_count"}, {"kind": "nope"}]}),
        ))
        .unwrap_err();
        assert!(error.0.starts_with("assertion 2: "), "{error:?}");
    }

    #[test]
    fn kpi_specs_parse_with_their_input() {
        let spec = kpi_spec(&cell(
            CellKind::Kpi,
            json!({"input": "revenue",
                   "value": {"kind": "aggregate", "column": "amount", "aggregate": "sum"},
                   "comparison": {"kind": "target", "value": 1000.0},
                   "thresholds": {"warn": 900.0, "critical": 500.0}}),
        ))
        .unwrap();
        assert_eq!(spec.input.as_deref(), Some("revenue"));
        assert!(spec.tiles[0].kpi.thresholds.is_some());
        assert!(kpi_spec(&cell(CellKind::Kpi, json!({"input": "r"}))).is_err());
    }

    #[test]
    fn kpi_cells_written_by_the_app_parse_too() {
        let spec = kpi_spec(&cell(
            CellKind::Kpi,
            json!({"input": "daily", "tiles": [
                {"label": "Orders", "kpi": {"value": {"kind": "row_count"}}},
                {"label": "Revenue", "kpi": {"value": {"kind": "aggregate", "column": "amount", "aggregate": "sum"}}}
            ]}),
        ))
        .unwrap();
        assert_eq!(spec.tiles.len(), 2);
    }
}
