//! Diff cell spec: two sides, each a cell's output now, in an earlier run,
//! in a pinned snapshot, or re-run on another connection slot.
//!
//! ```json
//! {"inputs": [{"cell": "orders"}, {"cell": "orders", "run": "previous"}],
//!  "keys": ["id"], "column_map": {"total": "total_amount"},
//!  "tolerance": {"absolute": 0.01}, "case_insensitive": true}
//! ```
//!
//! A side is `{"cell": id}` plus at most one of `"run": "<run id>" |
//! "previous"`, `"snapshot": "<name>"` or `"connection_ref": "<slot>"`.
//! `keys` empty compares positionally when `"alignment": "positional"`,
//! otherwise as a multiset.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use super::unknown_fields;
use crate::analytics::diff::{DiffOptions, RowAlignment, Tolerance};

const KNOWN: &[&str] = &[
    "inputs",
    "keys",
    "alignment",
    "column_map",
    "ignore_columns",
    "tolerance",
    "case_insensitive",
    "normalize_whitespace",
    "max_reported_rows",
];

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SideSource {
    /// The cell's current output.
    #[default]
    Current,
    /// The previous recorded run that has this cell's output.
    PreviousRun,
    Run(String),
    Snapshot(String),
    /// The cell's SQL run on another connection slot.
    Connection(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffSide {
    pub cell: String,
    pub source: SideSource,
}

impl DiffSide {
    pub fn current(cell: &str) -> Self {
        Self {
            cell: cell.to_string(),
            source: SideSource::Current,
        }
    }

    fn parse(value: &Value) -> Option<Self> {
        let cell = match value {
            Value::String(s) => return Some(Self::current(s)),
            Value::Object(m) => m.get("cell")?.as_str()?.to_string(),
            _ => return None,
        };
        let text = |k: &str| value.get(k).and_then(Value::as_str).map(str::to_string);
        let source = if let Some(run) = text("run") {
            if run == "previous" {
                SideSource::PreviousRun
            } else {
                SideSource::Run(run)
            }
        } else if let Some(name) = text("snapshot") {
            SideSource::Snapshot(name)
        } else if let Some(slot) = text("connection_ref") {
            SideSource::Connection(slot)
        } else {
            SideSource::Current
        };
        Some(Self { cell, source })
    }

    fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("cell".into(), Value::String(self.cell.clone()));
        match &self.source {
            SideSource::Current => {}
            SideSource::PreviousRun => {
                m.insert("run".into(), "previous".into());
            }
            SideSource::Run(id) => {
                m.insert("run".into(), Value::String(id.clone()));
            }
            SideSource::Snapshot(n) => {
                m.insert("snapshot".into(), Value::String(n.clone()));
            }
            SideSource::Connection(s) => {
                m.insert("connection_ref".into(), Value::String(s.clone()));
            }
        }
        Value::Object(m)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Alignment {
    Keyed,
    Positional,
    #[default]
    Multiset,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DiffCellSpec {
    pub left: DiffSide,
    pub right: DiffSide,
    pub keys: Vec<String>,
    pub alignment: Alignment,
    pub column_map: BTreeMap<String, String>,
    pub ignore_columns: Vec<String>,
    pub tolerance: Option<Tolerance>,
    pub case_insensitive: bool,
    pub normalize_whitespace: bool,
    pub max_reported_rows: Option<usize>,
    pub extra: Map<String, Value>,
}

/// Most detail rows a diff cell keeps per list.
pub const MAX_REPORTED_ROWS: usize = 2_000;

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl DiffCellSpec {
    /// The connection slots a side re-runs its cell's SQL on.
    pub fn connection_slots(&self) -> Vec<&str> {
        [&self.left, &self.right]
            .into_iter()
            .filter_map(|side| match &side.source {
                SideSource::Connection(slot) => Some(slot.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn from_spec(spec: &Value) -> Self {
        let sides: Vec<DiffSide> = spec
            .get("inputs")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(DiffSide::parse).collect())
            .unwrap_or_default();
        let keys = strings(spec.get("keys"));
        let alignment = match spec.get("alignment").and_then(Value::as_str) {
            Some("positional") => Alignment::Positional,
            Some("multiset") => Alignment::Multiset,
            Some("keyed") => Alignment::Keyed,
            _ if !keys.is_empty() => Alignment::Keyed,
            _ => Alignment::Multiset,
        };
        let flag = |k: &str| spec.get(k).and_then(Value::as_bool).unwrap_or(false);
        Self {
            left: sides.first().cloned().unwrap_or_default(),
            right: sides.get(1).cloned().unwrap_or_default(),
            keys,
            alignment,
            column_map: spec
                .get("column_map")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            ignore_columns: strings(spec.get("ignore_columns")),
            tolerance: spec
                .get("tolerance")
                .and_then(|v| serde_json::from_value(v.clone()).ok()),
            case_insensitive: flag("case_insensitive"),
            normalize_whitespace: flag("normalize_whitespace"),
            max_reported_rows: spec
                .get("max_reported_rows")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            extra: unknown_fields(spec, KNOWN),
        }
    }

    pub fn to_spec(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert(
            "inputs".into(),
            Value::Array(vec![self.left.to_value(), self.right.to_value()]),
        );
        if !self.keys.is_empty() {
            map.insert("keys".into(), serde_json::json!(self.keys));
        }
        let implied = if self.keys.is_empty() {
            Alignment::Multiset
        } else {
            Alignment::Keyed
        };
        if self.alignment != implied {
            let name = match self.alignment {
                Alignment::Keyed => "keyed",
                Alignment::Positional => "positional",
                Alignment::Multiset => "multiset",
            };
            map.insert("alignment".into(), name.into());
        }
        if !self.column_map.is_empty() {
            map.insert("column_map".into(), serde_json::json!(self.column_map));
        }
        if !self.ignore_columns.is_empty() {
            map.insert(
                "ignore_columns".into(),
                serde_json::json!(self.ignore_columns),
            );
        }
        if let Some(t) = self.tolerance {
            map.insert(
                "tolerance".into(),
                serde_json::to_value(t).unwrap_or(Value::Null),
            );
        }
        if self.case_insensitive {
            map.insert("case_insensitive".into(), true.into());
        }
        if self.normalize_whitespace {
            map.insert("normalize_whitespace".into(), true.into());
        }
        if let Some(n) = self.max_reported_rows {
            map.insert("max_reported_rows".into(), n.into());
        }
        Value::Object(map)
    }

    pub fn options(&self) -> DiffOptions {
        DiffOptions {
            alignment: match self.alignment {
                Alignment::Keyed if !self.keys.is_empty() => RowAlignment::Keyed {
                    columns: self.keys.clone(),
                },
                Alignment::Positional => RowAlignment::Positional,
                _ => RowAlignment::Multiset,
            },
            column_map: self.column_map.clone(),
            ignore_columns: self.ignore_columns.clone(),
            numeric_tolerance: self.tolerance,
            case_insensitive: self.case_insensitive,
            normalize_whitespace: self.normalize_whitespace,
            max_reported_rows: self
                .max_reported_rows
                .unwrap_or(500)
                .clamp(1, MAX_REPORTED_ROWS),
            ..DiffOptions::default()
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.left.cell.is_empty() && !self.right.cell.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::OutputView;
    use crate::analytics::diff::diff_outputs;
    use crate::analytics::test_support::{col, f, i};
    use serde_json::json;

    #[test]
    fn sides_and_options_round_trip() {
        let raw = json!({"inputs": [{"cell": "a"}, {"cell": "a", "run": "previous"}],
                         "keys": ["id"], "tolerance": {"absolute": 0.01, "relative": 0.0},
                         "column_map": {"t": "total"}, "case_insensitive": true, "zz": 1});
        let spec = DiffCellSpec::from_spec(&raw);
        assert_eq!(spec.right.source, SideSource::PreviousRun);
        assert_eq!(spec.alignment, Alignment::Keyed);
        assert!(spec.is_configured());
        assert_eq!(spec.to_spec(), raw);
        for side in [
            json!({"cell": "x", "run": "r_1"}),
            json!({"cell": "x", "snapshot": "golden"}),
            json!({"cell": "x", "connection_ref": "staging"}),
            json!("x"),
        ] {
            let parsed = DiffSide::parse(&side).unwrap();
            assert_eq!(DiffSide::parse(&parsed.to_value()).unwrap(), parsed);
        }
        let positional = DiffCellSpec::from_spec(&json!({"alignment": "positional"}));
        assert_eq!(positional.options().alignment, RowAlignment::Positional);
        assert!(!positional.is_configured());
        assert_eq!(
            DiffCellSpec::from_spec(&json!({"max_reported_rows": 1_000_000}))
                .options()
                .max_reported_rows,
            MAX_REPORTED_ROWS
        );
    }

    #[test]
    fn tolerance_and_keys_drive_the_diff() {
        let cols = [col("id", "int"), col("v", "float")];
        let left = vec![vec![i(1), f(1.0)], vec![i(2), f(2.0)]];
        let right = vec![vec![i(2), f(2.004)], vec![i(3), f(3.0)]];
        let spec = DiffCellSpec::from_spec(&json!({"inputs": ["l", "r"], "keys": ["id"],
                                                    "tolerance": {"absolute": 0.01}}));
        let d = diff_outputs(
            &OutputView::complete(&cols, &left),
            &OutputView::complete(&cols, &right),
            &spec.options(),
        )
        .unwrap();
        assert_eq!(
            (d.summary.added, d.summary.removed, d.summary.changed),
            (1, 1, 0)
        );
    }
}
