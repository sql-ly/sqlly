//! Reading and writing typed cell specs. Specs are stored as JSON in the
//! notebook; every typed spec keeps fields it does not know in a flattened
//! `extra` map so a newer build's settings survive a save here.

use std::collections::BTreeMap;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use sqlly_core::{CellValue, ResultColumn};

use crate::report::chart::DateGrain;
use crate::report::format::{cell_temporal, float_literal, grain_label, truncate_to_grain};

/// Fields a typed spec does not model, written back unchanged.
pub type SpecExtra = BTreeMap<String, Value>;

/// The typed spec in `value`, or the default when it is missing or does not
/// parse (a hand-edited file never stops the notebook from opening).
pub fn read<T: DeserializeOwned + Default>(value: &Value) -> T {
    if value.is_null() {
        return T::default();
    }
    serde_json::from_value(value.clone()).unwrap_or_default()
}

/// `spec` as stored JSON.
pub fn write<T: Serialize>(spec: &T) -> Value {
    serde_json::to_value(spec).unwrap_or(Value::Null)
}

/// One linked cross-filter: rows whose `column` reads as `value`. Set by a
/// click in a chart cell and applied locally by the cells that follow it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct CrossFilter {
    /// The chart cell the selection was made in.
    pub source: String,
    pub column: String,
    /// The category as the chart labels it ([`category_label`]).
    pub value: String,
    /// The date grain the chart grouped `column` by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grain: Option<DateGrain>,
}

/// How a chart labels a category: the value as text, dates bucketed by
/// `grain`. Cross-filters compare these labels, so the chart and the cells
/// following it group values identically.
pub fn category_label(cell: &CellValue, grain: Option<DateGrain>) -> String {
    if let Some(grain) = grain
        && let Some(temporal) = cell_temporal(cell)
    {
        return grain_label(truncate_to_grain(temporal.as_datetime(), grain), grain);
    }
    match cell {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(b) => b.to_string(),
        CellValue::I64(n) => n.to_string(),
        CellValue::F64(f) => float_literal(*f),
        CellValue::String(s) => s.clone(),
        CellValue::Bytes(b) => format!("<{} bytes>", b.len()),
    }
}

impl CrossFilter {
    /// The rows of an output this filter keeps. An output without the
    /// filtered column is left whole (the filter does not apply to it).
    pub fn apply(&self, columns: &[ResultColumn], rows: &[Vec<CellValue>]) -> Vec<Vec<CellValue>> {
        let Some(index) = self.column_index(columns) else {
            return rows.to_vec();
        };
        rows.iter()
            .filter(|row| {
                row.get(index)
                    .is_some_and(|cell| category_label(cell, self.grain) == self.value)
            })
            .cloned()
            .collect()
    }

    pub fn column_index(&self, columns: &[ResultColumn]) -> Option<usize> {
        columns
            .iter()
            .position(|c| c.name == self.column)
            .or_else(|| {
                columns
                    .iter()
                    .position(|c| c.name.eq_ignore_ascii_case(&self.column))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, PartialEq, Serialize, serde::Deserialize)]
    struct Demo {
        #[serde(default)]
        input: Option<String>,
        #[serde(flatten)]
        extra: SpecExtra,
    }

    #[test]
    fn unknown_fields_survive_and_bad_specs_fall_back() {
        let value = serde_json::json!({"input": "c_1", "future": {"x": 1}});
        let demo: Demo = read(&value);
        assert_eq!(demo.input.as_deref(), Some("c_1"));
        assert_eq!(write(&demo), value);
        let broken: Demo = read(&serde_json::json!({"input": 5}));
        assert_eq!(broken, Demo::default());
        assert_eq!(read::<Demo>(&Value::Null), Demo::default());
    }

    #[test]
    fn cross_filters_keep_rows_with_the_clicked_label() {
        let columns = vec![
            ResultColumn {
                name: "Region".into(),
                data_type: "varchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "day".into(),
                data_type: "date".into(),
                nullable: true,
            },
        ];
        let rows = vec![
            vec![
                CellValue::String("West".into()),
                CellValue::String("2026-01-05".into()),
            ],
            vec![
                CellValue::String("East".into()),
                CellValue::String("2026-01-20".into()),
            ],
            vec![CellValue::Null, CellValue::String("2026-02-02".into())],
        ];
        let west = CrossFilter {
            source: "c_chart".into(),
            column: "region".into(),
            value: "West".into(),
            grain: None,
        };
        assert_eq!(west.apply(&columns, &rows).len(), 1);
        let january = CrossFilter {
            column: "day".into(),
            value: category_label(&rows[0][1], Some(DateGrain::Month)),
            grain: Some(DateGrain::Month),
            ..west.clone()
        };
        assert_eq!(january.apply(&columns, &rows).len(), 2);
        let other = CrossFilter {
            column: "missing".into(),
            ..west
        };
        assert_eq!(other.apply(&columns, &rows).len(), 3);
        assert_eq!(category_label(&CellValue::F64(2.5), None), "2.5");
    }
}
