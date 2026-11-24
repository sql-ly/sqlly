//! Profile cell spec:
//!
//! ```json
//! {"input": "orders", "columns": ["amount", "region"], "top_k": 5,
//!  "histogram_bins": 20, "sort": "null_rate", "descending": true}
//! ```
//!
//! `columns` empty profiles every column. `sort` orders the summary grid:
//! `position` (default), `name`, `null_rate`, `distinct`, `type`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{input_of, unknown_fields};
use crate::analytics::Completeness;
use crate::analytics::profile::{ColumnProfile, KeyCandidate, ProfileOptions, ResultProfile};
use sqlly_core::{CellValue, ResultColumn};

const KNOWN: &[&str] = &[
    "input",
    "columns",
    "top_k",
    "histogram_bins",
    "sort",
    "descending",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSort {
    #[default]
    Position,
    Name,
    NullRate,
    Distinct,
    Type,
}

impl ProfileSort {
    pub const ALL: [ProfileSort; 5] = [
        ProfileSort::Position,
        ProfileSort::Name,
        ProfileSort::NullRate,
        ProfileSort::Distinct,
        ProfileSort::Type,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ProfileSort::Position => "position",
            ProfileSort::Name => "name",
            ProfileSort::NullRate => "null_rate",
            ProfileSort::Distinct => "distinct",
            ProfileSort::Type => "type",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProfileCellSpec {
    pub input: String,
    pub columns: Vec<String>,
    pub top_k: Option<usize>,
    pub histogram_bins: Option<usize>,
    pub sort: ProfileSort,
    pub descending: bool,
    pub extra: Map<String, Value>,
}

/// Bounds the spec may ask for, so a hand-edited file cannot make the
/// profiler hold an unbounded table.
pub const MAX_TOP_K: usize = 50;
pub const MAX_HISTOGRAM_BINS: usize = 100;

impl ProfileCellSpec {
    pub fn from_spec(spec: &Value) -> Self {
        let get = |k: &str| spec.get(k);
        Self {
            input: input_of(spec),
            columns: get("columns")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            top_k: get("top_k").and_then(Value::as_u64).map(|n| n as usize),
            histogram_bins: get("histogram_bins")
                .and_then(Value::as_u64)
                .map(|n| n as usize),
            sort: get("sort")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
            descending: get("descending").and_then(Value::as_bool).unwrap_or(false),
            extra: unknown_fields(spec, KNOWN),
        }
    }

    pub fn to_spec(&self) -> Value {
        let mut map = self.extra.clone();
        map.insert("input".into(), Value::String(self.input.clone()));
        if !self.columns.is_empty() {
            map.insert("columns".into(), serde_json::json!(self.columns));
        }
        if let Some(k) = self.top_k {
            map.insert("top_k".into(), k.into());
        }
        if let Some(b) = self.histogram_bins {
            map.insert("histogram_bins".into(), b.into());
        }
        if self.sort != ProfileSort::Position {
            map.insert("sort".into(), self.sort.as_str().into());
        }
        if self.descending {
            map.insert("descending".into(), true.into());
        }
        Value::Object(map)
    }

    pub fn options(&self) -> ProfileOptions {
        let mut options = ProfileOptions::default();
        if let Some(k) = self.top_k {
            options.top_k = k.clamp(1, MAX_TOP_K);
        }
        if let Some(b) = self.histogram_bins {
            options.histogram_bins = b.clamp(2, MAX_HISTOGRAM_BINS);
        }
        options
    }

    /// Whether `column` is profiled.
    pub fn includes(&self, column: &str) -> bool {
        self.columns.is_empty() || self.columns.iter().any(|c| c.eq_ignore_ascii_case(column))
    }
}

/// How much of the input a profile describes, for its honest label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Coverage {
    /// Every row was read and every figure is exact.
    Complete { rows: u64 },
    /// Every row was read; some figures are sketches.
    CompleteEstimated { rows: u64 },
    /// Only `read` of `total` rows (when known) were read.
    Partial { read: u64, total: Option<u64> },
    /// Column statistics cover all `total` rows of a local table; the
    /// histograms, top values and meanings only the first `read`.
    Mixed { read: u64, total: u64 },
}

impl Coverage {
    pub fn of(profile: &ResultProfile, total_rows: Option<u64>) -> Self {
        let rows = profile.row_count;
        if !profile.completeness.is_complete() {
            let total = match profile.completeness {
                Completeness::Spilled { total } => Some(total),
                _ => total_rows.filter(|t| *t > rows),
            };
            return Coverage::Partial { read: rows, total };
        }
        if profile.columns.iter().any(column_has_estimates) {
            Coverage::CompleteEstimated { rows }
        } else {
            Coverage::Complete { rows }
        }
    }

    pub fn is_complete(self) -> bool {
        !matches!(self, Coverage::Partial { .. } | Coverage::Mixed { .. })
    }
}

pub fn column_has_estimates(column: &ColumnProfile) -> bool {
    column.distinct.approximate
        || column.top_values_approximate
        || column
            .numeric
            .as_ref()
            .is_some_and(|n| n.percentiles.approximate || n.histogram.approximate)
        || column
            .temporal
            .as_ref()
            .is_some_and(|t| t.buckets_approximate)
}

/// Column indexes of `profile` in the order `sort` asks for.
pub fn sorted_columns(profile: &ResultProfile, sort: ProfileSort, descending: bool) -> Vec<usize> {
    let mut order: Vec<usize> = (0..profile.columns.len()).collect();
    let cols = &profile.columns;
    match sort {
        ProfileSort::Position => {}
        ProfileSort::Name => order.sort_by(|a, b| {
            cols[*a]
                .name
                .to_lowercase()
                .cmp(&cols[*b].name.to_lowercase())
        }),
        ProfileSort::NullRate => order.sort_by(|a, b| {
            cols[*a]
                .null_rate
                .partial_cmp(&cols[*b].null_rate)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        ProfileSort::Distinct => {
            order.sort_by_key(|i| cols[*i].distinct.value);
        }
        ProfileSort::Type => order.sort_by(|a, b| {
            format!("{:?}", cols[*a].semantic_type).cmp(&format!("{:?}", cols[*b].semantic_type))
        }),
    }
    if descending {
        order.reverse();
    }
    order
}

/// Statistics a local database computed over every row of a relation
/// (DuckDB `SUMMARIZE`), shown beside a profile read from the first rows
/// when the input is a local table larger than its snapshot.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WholeTable {
    pub relation: String,
    pub rows: u64,
    pub columns: Vec<WholeColumn>,
}

/// One column of a [`WholeTable`]. `distinct` is DuckDB's estimate.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct WholeColumn {
    pub name: String,
    pub data_type: String,
    pub count: u64,
    pub null_count: u64,
    pub distinct: u64,
    pub min: Option<String>,
    pub max: Option<String>,
    pub mean: Option<f64>,
    pub stddev: Option<f64>,
    pub q25: Option<f64>,
    pub q50: Option<f64>,
    pub q75: Option<f64>,
}

impl WholeColumn {
    pub fn null_rate(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.null_count as f64 / self.count as f64
        }
    }
}

fn number_of(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::I64(n) => Some(*n as f64),
        CellValue::F64(f) => Some(*f),
        CellValue::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f: &f64| f.is_finite())
}

fn text_of(value: &CellValue) -> Option<String> {
    match value {
        CellValue::Null | CellValue::Bytes(_) => None,
        CellValue::String(s) => Some(s.clone()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::F64(f) => Some(f.to_string()),
        CellValue::Bool(b) => Some(b.to_string()),
    }
}

impl WholeTable {
    /// Read a `SUMMARIZE` result: one row per column with `column_name`,
    /// `column_type`, `min`, `max`, `approx_unique`, `avg`, `std`, `q25`,
    /// `q50`, `q75`, `count` and `null_percentage`. `None` when the shape
    /// is not that.
    pub fn from_summarize(
        relation: &str,
        columns: &[ResultColumn],
        rows: &[Vec<CellValue>],
    ) -> Option<Self> {
        let at = |name: &str| {
            columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
        };
        let name_at = at("column_name")?;
        let count_at = at("count")?;
        let field = |row: &Vec<CellValue>, name: &str| at(name).and_then(|i| row.get(i)).cloned();
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let name = text_of(row.get(name_at)?)?;
            let count = number_of(row.get(count_at)?)?.max(0.0) as u64;
            let null_rate = field(row, "null_percentage")
                .as_ref()
                .and_then(number_of)
                .unwrap_or(0.0)
                / 100.0;
            let num = |name: &str| field(row, name).as_ref().and_then(number_of);
            out.push(WholeColumn {
                name,
                data_type: field(row, "column_type")
                    .as_ref()
                    .and_then(text_of)
                    .unwrap_or_default(),
                count,
                null_count: (count as f64 * null_rate).round() as u64,
                distinct: num("approx_unique").unwrap_or(0.0).max(0.0) as u64,
                min: field(row, "min").as_ref().and_then(text_of),
                max: field(row, "max").as_ref().and_then(text_of),
                mean: num("avg"),
                stddev: num("std"),
                q25: num("q25"),
                q50: num("q50"),
                q75: num("q75"),
            });
        }
        Some(WholeTable {
            relation: relation.to_string(),
            rows: out.first().map_or(0, |c| c.count),
            columns: out,
        })
    }

    pub fn column(&self, name: &str) -> Option<&WholeColumn> {
        self.columns.iter().find(|c| c.name == name).or_else(|| {
            self.columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(name))
        })
    }
}

impl Coverage {
    /// The coverage of a profile read from the first rows with column
    /// statistics from `whole`: `Mixed` when the table holds more rows
    /// than were read.
    pub fn with_whole(self, profile: &ResultProfile, whole: Option<&WholeTable>) -> Self {
        match whole {
            Some(whole) if whole.rows > profile.row_count => Coverage::Mixed {
                read: profile.row_count,
                total: whole.rows,
            },
            _ => self,
        }
    }
}

/// Names of the columns that can identify rows on their own.
pub fn candidate_keys(profile: &ResultProfile) -> Vec<(String, KeyCandidate)> {
    profile
        .columns
        .iter()
        .filter(|c| c.candidate_key != KeyCandidate::NotUnique)
        .map(|c| (c.name.clone(), c.candidate_key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::profile::profile_rows;
    use crate::analytics::test_support::{col, i, s};
    use serde_json::json;

    #[test]
    fn spec_round_trips_and_keeps_unknown_fields() {
        let raw = json!({"input": "orders", "columns": ["a"], "top_k": 500, "sort": "null_rate",
                         "descending": true, "future": {"x": 1}});
        let spec = ProfileCellSpec::from_spec(&raw);
        assert_eq!(spec.input, "orders");
        assert_eq!(spec.sort, ProfileSort::NullRate);
        assert_eq!(spec.options().top_k, MAX_TOP_K);
        assert!(spec.includes("A") && !spec.includes("b"));
        assert_eq!(spec.to_spec(), raw);
        let empty = ProfileCellSpec::from_spec(&json!({}));
        assert_eq!(empty.sort, ProfileSort::Position);
        assert!(empty.includes("anything"));
    }

    #[test]
    fn coverage_is_honest_about_partial_input() {
        let cols = [col("id", "int"), col("name", "nvarchar")];
        let rows: Vec<_> = (0..5).map(|n| vec![i(n), s("x")]).collect();
        let full = profile_rows(&cols, &rows, Completeness::Complete, &Default::default());
        assert_eq!(Coverage::of(&full, Some(5)), Coverage::Complete { rows: 5 });
        let cut = profile_rows(
            &cols,
            &rows,
            Completeness::Truncated { cap: 5 },
            &Default::default(),
        );
        assert_eq!(
            Coverage::of(&cut, Some(900)),
            Coverage::Partial {
                read: 5,
                total: Some(900)
            }
        );
        assert!(!Coverage::of(&cut, None).is_complete());
        let keys = candidate_keys(&full);
        assert_eq!(keys, vec![("id".to_string(), KeyCandidate::Unique)]);
        let order = sorted_columns(&full, ProfileSort::Name, true);
        assert_eq!(full.columns[order[0]].name, "name");
    }

    #[test]
    fn summarize_rows_become_whole_table_statistics_and_mixed_coverage() {
        let cols: Vec<ResultColumn> = [
            "column_name",
            "column_type",
            "min",
            "max",
            "approx_unique",
            "avg",
            "std",
            "q25",
            "q50",
            "q75",
            "count",
            "null_percentage",
        ]
        .iter()
        .map(|n| col(n, "varchar"))
        .collect();
        let rows = vec![
            vec![
                s("amount"),
                s("DOUBLE"),
                s("0.5"),
                s("2999.5"),
                i(2950),
                s("1500.0"),
                s("866.3"),
                s("750.2"),
                s("1500.1"),
                s("2250.4"),
                i(3000),
                CellValue::F64(10.0),
            ],
            vec![
                s("region"),
                s("VARCHAR"),
                s("east"),
                s("west"),
                i(4),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                i(3000),
                s("0.00"),
            ],
        ];
        let whole = WholeTable::from_summarize("orders", &cols, &rows).unwrap();
        assert_eq!(whole.rows, 3000);
        let amount = whole.column("AMOUNT").unwrap();
        assert_eq!(amount.null_count, 300);
        assert!((amount.null_rate() - 0.1).abs() < 1e-9);
        assert_eq!(amount.distinct, 2950);
        assert_eq!(amount.max.as_deref(), Some("2999.5"));
        assert_eq!(amount.q50, Some(1500.1));
        let region = whole.column("region").unwrap();
        assert_eq!((region.null_count, region.mean), (0, None));
        assert!(WholeTable::from_summarize("x", &cols[2..], &rows).is_none());

        let read: Vec<_> = (0..1000).map(|n| vec![i(n), s("east")]).collect();
        let first = profile_rows(
            &[col("amount", "double"), col("region", "varchar")],
            &read,
            Completeness::Complete,
            &Default::default(),
        );
        let coverage = Coverage::of(&first, Some(1000)).with_whole(&first, Some(&whole));
        assert_eq!(
            coverage,
            Coverage::Mixed {
                read: 1000,
                total: 3000
            }
        );
        assert!(!coverage.is_complete());
        let small = WholeTable {
            rows: 1000,
            ..whole.clone()
        };
        assert_eq!(
            Coverage::of(&first, Some(1000)).with_whole(&first, Some(&small)),
            Coverage::of(&first, Some(1000)),
            "a table no larger than the snapshot adds nothing"
        );
    }
}
