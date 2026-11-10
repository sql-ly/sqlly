//! Bounded, null-aware result profiling. The accumulator accepts row batches
//! incrementally, so callers can reuse it for streamed or on-demand data.

use std::collections::HashMap;

use sqlly_core::{CellValue, ResultColumn};

pub const PROFILE_MAX_ROWS: usize = 100_000;
const DISTINCT_VALUE_CAP: usize = 10_000;

#[derive(Clone, Debug, PartialEq)]
pub struct ColumnProfile {
    pub name: String,
    pub data_type: String,
    pub rows: usize,
    pub nulls: usize,
    pub distinct: usize,
    pub distinct_truncated: bool,
    pub numeric_min: Option<f64>,
    pub numeric_max: Option<f64>,
    pub numeric_mean: Option<f64>,
    pub min_length: Option<usize>,
    pub max_length: Option<usize>,
    pub frequent_values: Vec<(String, usize)>,
}

#[derive(Default)]
struct ColumnAccumulator {
    rows: usize,
    nulls: usize,
    frequencies: HashMap<String, (String, usize)>,
    distinct_truncated: bool,
    numeric_count: usize,
    numeric_sum: f64,
    numeric_min: Option<f64>,
    numeric_max: Option<f64>,
    min_length: Option<usize>,
    max_length: Option<usize>,
}

pub struct ResultProfiler {
    columns: Vec<ResultColumn>,
    accumulators: Vec<ColumnAccumulator>,
    rows_seen: usize,
    truncated: bool,
}

impl ResultProfiler {
    pub fn new(columns: &[ResultColumn]) -> Self {
        Self {
            columns: columns.to_vec(),
            accumulators: (0..columns.len())
                .map(|_| ColumnAccumulator::default())
                .collect(),
            rows_seen: 0,
            truncated: false,
        }
    }

    pub fn push_rows(&mut self, rows: &[Vec<CellValue>]) {
        for row in rows {
            if self.rows_seen == PROFILE_MAX_ROWS {
                self.truncated = true;
                break;
            }
            self.rows_seen += 1;
            for (index, accumulator) in self.accumulators.iter_mut().enumerate() {
                let value = row.get(index).unwrap_or(&CellValue::Null);
                accumulator.rows += 1;
                if matches!(value, CellValue::Null) {
                    accumulator.nulls += 1;
                    continue;
                }
                let distinct_key = distinct_key(value);
                if let Some((_, count)) = accumulator.frequencies.get_mut(&distinct_key) {
                    *count += 1;
                } else if accumulator.frequencies.len() < DISTINCT_VALUE_CAP {
                    accumulator
                        .frequencies
                        .insert(distinct_key, (display_value(value), 1));
                } else {
                    accumulator.distinct_truncated = true;
                }
                if let Some(number) = numeric_value(value) {
                    accumulator.numeric_count += 1;
                    accumulator.numeric_sum += number;
                    accumulator.numeric_min = Some(
                        accumulator
                            .numeric_min
                            .map_or(number, |minimum| minimum.min(number)),
                    );
                    accumulator.numeric_max = Some(
                        accumulator
                            .numeric_max
                            .map_or(number, |maximum| maximum.max(number)),
                    );
                }
                if let Some(length) = value_length(value) {
                    accumulator.min_length = Some(
                        accumulator
                            .min_length
                            .map_or(length, |minimum| minimum.min(length)),
                    );
                    accumulator.max_length = Some(
                        accumulator
                            .max_length
                            .map_or(length, |maximum| maximum.max(length)),
                    );
                }
            }
        }
    }

    pub fn finish(self) -> ResultProfile {
        let columns = self
            .columns
            .into_iter()
            .zip(self.accumulators)
            .map(|(column, accumulator)| ColumnProfile {
                name: column.name,
                data_type: column.data_type,
                rows: accumulator.rows,
                nulls: accumulator.nulls,
                distinct: accumulator.frequencies.len(),
                distinct_truncated: accumulator.distinct_truncated,
                numeric_min: accumulator.numeric_min,
                numeric_max: accumulator.numeric_max,
                numeric_mean: (accumulator.numeric_count > 0)
                    .then(|| accumulator.numeric_sum / accumulator.numeric_count as f64),
                min_length: accumulator.min_length,
                max_length: accumulator.max_length,
                frequent_values: top_values(accumulator.frequencies),
            })
            .collect();
        ResultProfile {
            rows_profiled: self.rows_seen,
            rows_truncated: self.truncated,
            columns,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResultProfile {
    pub rows_profiled: usize,
    pub rows_truncated: bool,
    pub columns: Vec<ColumnProfile>,
}

/// A column is "mostly null" when at least this share of its profiled rows
/// is null.
pub const MOSTLY_NULL_PERCENT: f64 = 50.0;

/// Upper distinct-value bound for the "Low cardinality" filter — the
/// category-like columns worth eyeballing value by value.
pub const LOW_CARDINALITY_MAX: usize = 20;

/// Semantic quick filters over a profile's columns: the data-shape questions
/// a reviewer actually asks of a profile (what's nullable, what's constant,
/// what could be a key), not free-form predicates — the grid's own column
/// filters cover those. Pure so the classification is testable without a
/// grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProfileFilter {
    #[default]
    All,
    /// At least one null in the profiled rows.
    WithNulls,
    /// Null in at least [`MOSTLY_NULL_PERCENT`] of the profiled rows.
    MostlyNull,
    /// At most one distinct non-null value (includes all-null columns).
    Constant,
    /// Null-free with every profiled value distinct — candidate keys.
    Unique,
    /// Repeating values from a small set — category-like columns.
    LowCardinality,
    /// Values that read as numbers.
    Numeric,
}

impl ProfileFilter {
    pub fn all() -> [ProfileFilter; 7] {
        [
            ProfileFilter::All,
            ProfileFilter::WithNulls,
            ProfileFilter::MostlyNull,
            ProfileFilter::Constant,
            ProfileFilter::Unique,
            ProfileFilter::LowCardinality,
            ProfileFilter::Numeric,
        ]
    }

    pub fn label(self) -> String {
        crate::localization::text(match self {
            ProfileFilter::All => "results.profiling.filter_all_columns",
            ProfileFilter::WithNulls => "results.profiling.filter_with_nulls",
            ProfileFilter::MostlyNull => "results.profiling.filter_mostly_null",
            ProfileFilter::Constant => "results.profiling.filter_constant",
            ProfileFilter::Unique => "results.profiling.filter_unique",
            ProfileFilter::LowCardinality => "results.profiling.filter_low_cardinality",
            ProfileFilter::Numeric => "results.profiling.filter_numeric",
        })
    }

    pub fn matches(self, column: &ColumnProfile) -> bool {
        // `distinct` counts non-null values only, capped by the accumulator;
        // when the cap was hit (`distinct_truncated`) the true count is
        // unknown, so cardinality-based filters exclude the column rather
        // than guess.
        let non_null = column.rows.saturating_sub(column.nulls);
        match self {
            ProfileFilter::All => true,
            ProfileFilter::WithNulls => column.nulls > 0,
            ProfileFilter::MostlyNull => {
                column.rows > 0
                    && column.nulls as f64 * 100.0 / column.rows as f64 >= MOSTLY_NULL_PERCENT
            }
            ProfileFilter::Constant => !column.distinct_truncated && column.distinct <= 1,
            ProfileFilter::Unique => {
                column.rows > 0
                    && column.nulls == 0
                    && !column.distinct_truncated
                    && column.distinct == column.rows
            }
            ProfileFilter::LowCardinality => {
                !column.distinct_truncated
                    && (1..=LOW_CARDINALITY_MAX).contains(&column.distinct)
                    && column.distinct < non_null
            }
            ProfileFilter::Numeric => column.numeric_min.is_some(),
        }
    }
}

impl ResultProfile {
    /// The profile as a result set — one row per profiled column — so it can
    /// render in a real grid (sortable, filterable, copyable) instead of only
    /// as clipboard Markdown.
    pub fn to_result_set(&self) -> (Vec<ResultColumn>, Vec<Vec<CellValue>>) {
        let text = |key: &str| ResultColumn {
            name: crate::localization::text(key),
            data_type: "text".to_string(),
            nullable: true,
        };
        let number = |key: &str| ResultColumn {
            name: crate::localization::text(key),
            data_type: "double".to_string(),
            nullable: true,
        };
        let integer = |key: &str| ResultColumn {
            name: crate::localization::text(key),
            data_type: "bigint".to_string(),
            nullable: true,
        };
        let columns = vec![
            text("results.profiling.grid_column_name"),
            text("results.profiling.grid_type"),
            integer("results.profiling.grid_rows"),
            integer("results.profiling.grid_nulls"),
            number("results.profiling.grid_null_percent"),
            text("results.profiling.grid_distinct"),
            number("results.profiling.grid_minimum"),
            number("results.profiling.grid_maximum"),
            number("results.profiling.grid_mean"),
            text("results.profiling.grid_length"),
            text("results.profiling.grid_top_values"),
        ];
        let rows = self
            .columns
            .iter()
            .map(|column| {
                let null_percent = if column.rows == 0 {
                    0.0
                } else {
                    column.nulls as f64 * 100.0 / column.rows as f64
                };
                let distinct = if column.distinct_truncated {
                    format!(">={}", column.distinct)
                } else {
                    column.distinct.to_string()
                };
                let length = match (column.min_length, column.max_length) {
                    (Some(minimum), Some(maximum)) if minimum == maximum => minimum.to_string(),
                    (Some(minimum), Some(maximum)) => format!("{minimum}–{maximum}"),
                    _ => String::new(),
                };
                // The grid keeps a short single-line preview; the detail pane
                // below the grid shows the full retained list.
                let mut frequent = column
                    .frequent_values
                    .iter()
                    .take(TOP_VALUE_PREVIEW)
                    .map(|(value, count)| format!("{} × {count}", single_line(value)))
                    .collect::<Vec<_>>()
                    .join(", ");
                if column.frequent_values.len() > TOP_VALUE_PREVIEW {
                    frequent.push_str(", …");
                }
                let numeric =
                    |value: Option<f64>| value.map(CellValue::F64).unwrap_or(CellValue::Null);
                vec![
                    CellValue::String(column.name.clone()),
                    CellValue::String(column.data_type.clone()),
                    CellValue::I64(column.rows as i64),
                    CellValue::I64(column.nulls as i64),
                    CellValue::F64((null_percent * 100.0).round() / 100.0),
                    CellValue::String(distinct),
                    numeric(column.numeric_min),
                    numeric(column.numeric_max),
                    numeric(column.numeric_mean.map(round4)),
                    CellValue::String(length),
                    CellValue::String(frequent),
                ]
            })
            .collect();
        (columns, rows)
    }

    pub fn to_markdown(&self) -> String {
        let mut output = crate::localization::format(
            "results.profiling.markdown_heading",
            &[("row_count", &self.rows_profiled.to_string())],
        );
        if self.rows_truncated {
            output.push_str(&crate::localization::format(
                "results.profiling.markdown_truncation",
                &[("maximum_rows", &PROFILE_MAX_ROWS.to_string())],
            ));
        }
        output.push_str(&crate::localization::text(
            "results.profiling.markdown_column_header",
        ));
        for column in &self.columns {
            let distinct = if column.distinct_truncated {
                format!(">={}", column.distinct)
            } else {
                column.distinct.to_string()
            };
            let length = match (column.min_length, column.max_length) {
                (Some(minimum), Some(maximum)) => format!("{minimum}–{maximum}"),
                _ => "—".to_string(),
            };
            output.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
                column.name.replace('|', "\\|"),
                column.data_type.replace('|', "\\|"),
                column.nulls,
                distinct,
                format_number(column.numeric_min),
                format_number(column.numeric_max),
                format_number(column.numeric_mean),
                length,
                column
                    .frequent_values
                    .iter()
                    .map(|(value, count)| format!("{} × {count}", value.replace('|', "\\|")))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        output
    }
}

fn numeric_value(value: &CellValue) -> Option<f64> {
    match value {
        CellValue::I64(value) => Some(*value as f64),
        CellValue::F64(value) if value.is_finite() => Some(*value),
        _ => None,
    }
}

fn value_length(value: &CellValue) -> Option<usize> {
    match value {
        CellValue::String(value) => Some(value.chars().count()),
        CellValue::Bytes(value) => Some(value.len()),
        _ => None,
    }
}

fn distinct_key(value: &CellValue) -> String {
    match value {
        CellValue::Null => "null".to_string(),
        CellValue::Bool(value) => format!("bool:{value}"),
        CellValue::I64(value) => format!("i64:{value}"),
        CellValue::F64(value) => format!("f64:{:016X}", value.to_bits()),
        CellValue::String(value) => format!("str:{value}"),
        CellValue::Bytes(value) => format!(
            "bytes:{}",
            value
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        ),
    }
}

fn display_value(value: &CellValue) -> String {
    let raw = match value {
        CellValue::Null => "NULL".to_string(),
        CellValue::Bool(value) => value.to_string(),
        CellValue::I64(value) => value.to_string(),
        CellValue::F64(value) => value.to_string(),
        CellValue::String(value) => value.clone(),
        CellValue::Bytes(value) => crate::localization::format(
            "results.profiling.binary_value",
            &[("byte_count", &value.len().to_string())],
        ),
    };
    let mut chars = raw.chars();
    let abbreviated: String = chars.by_ref().take(48).collect();
    if chars.next().is_some() {
        format!("{abbreviated}…")
    } else {
        abbreviated
    }
}

/// How many retained frequent values appear inline in the grid's "Top
/// values" cell; the rest are only in the detail pane's full list.
const TOP_VALUE_PREVIEW: usize = 3;

/// How many repeated values each column retains for display.
const TOP_VALUE_CAP: usize = 8;

fn top_values(frequencies: HashMap<String, (String, usize)>) -> Vec<(String, usize)> {
    let mut values: Vec<_> = frequencies
        .into_values()
        .filter(|(_, count)| *count > 1)
        .collect();
    values.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    values.truncate(TOP_VALUE_CAP);
    values
}

/// Collapse a display value to one line so grid cells and detail rows never
/// carry embedded newlines (GPUI's shaper rejects them).
pub fn single_line(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A statistic rounded for display: up to four decimals, trailing zeros
/// trimmed, `—` when absent.
pub fn format_number(value: Option<f64>) -> String {
    value.map_or_else(
        || "—".to_string(),
        |value| {
            let text = format!("{value:.4}");
            let text = text.trim_end_matches('0').trim_end_matches('.');
            text.to_string()
        },
    )
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> ResultColumn {
        ResultColumn {
            name: name.into(),
            data_type: String::new(),
            nullable: true,
        }
    }

    #[test]
    fn incremental_profile_is_null_aware_and_typed() {
        let mut profiler = ResultProfiler::new(&[column("id"), column("name")]);
        profiler.push_rows(&[
            vec![CellValue::I64(1), CellValue::String("Ada".into())],
            vec![CellValue::I64(2), CellValue::Null],
        ]);
        profiler.push_rows(&[vec![CellValue::I64(2), CellValue::String("Grace".into())]]);
        let profile = profiler.finish();
        assert_eq!(profile.rows_profiled, 3);
        assert_eq!(profile.columns[0].distinct, 2);
        assert_eq!(profile.columns[0].numeric_mean, Some(5.0 / 3.0));
        assert_eq!(profile.columns[1].nulls, 1);
        assert_eq!(profile.columns[1].min_length, Some(3));
        assert_eq!(profile.columns[1].max_length, Some(5));
        assert_eq!(profile.columns[0].frequent_values, vec![("2".into(), 2)]);
        assert!(profile.to_markdown().contains("| name |  | 1 | 2 |"));
    }

    #[test]
    fn profile_converts_to_a_grid_result_set() {
        let mut profiler = ResultProfiler::new(&[column("id"), column("name")]);
        profiler.push_rows(&[
            vec![CellValue::I64(1), CellValue::String("Ada".into())],
            vec![CellValue::I64(3), CellValue::Null],
        ]);
        let profile = profiler.finish();
        let (columns, rows) = profile.to_result_set();
        assert_eq!(columns.len(), 11);
        assert_eq!(columns[0].name, "Column");
        assert_eq!(columns[2].name, "Rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], CellValue::String("id".into()));
        assert_eq!(rows[0][2], CellValue::I64(2));
        assert_eq!(rows[0][3], CellValue::I64(0));
        assert_eq!(rows[0][6], CellValue::F64(1.0));
        assert_eq!(rows[0][7], CellValue::F64(3.0));
        // name column: 1 null of 2 rows = 50%.
        assert_eq!(rows[1][3], CellValue::I64(1));
        assert_eq!(rows[1][4], CellValue::F64(50.0));
        assert_eq!(rows[1][6], CellValue::Null);
    }

    #[test]
    fn grid_preview_caps_top_values_and_rounds_the_mean() {
        // Six distinct repeated values: all are retained (≤ cap) but the grid
        // cell previews only the first three, with an ellipsis marker.
        let mut profiler = ResultProfiler::new(&[column("v")]);
        for value in 0..6 {
            profiler.push_rows(&[vec![CellValue::I64(value)], vec![CellValue::I64(value)]]);
        }
        profiler.push_rows(&[vec![CellValue::I64(1_000)]]);
        let profile = profiler.finish();
        assert_eq!(profile.columns[0].frequent_values.len(), 6);
        let (_, rows) = profile.to_result_set();
        let CellValue::String(preview) = &rows[0][10] else {
            panic!("preview cell is not text");
        };
        assert_eq!(preview.matches('×').count(), TOP_VALUE_PREVIEW);
        assert!(preview.ends_with('…'));
        // 13 values summing to 1030: the raw mean repeats forever; the grid
        // cell carries it rounded to four decimals.
        let CellValue::F64(mean) = rows[0][8] else {
            panic!("mean cell is not numeric");
        };
        assert_eq!(mean, (1_030.0 / 13.0 * 10_000.0_f64).round() / 10_000.0);
        assert_eq!(format!("{mean}"), "79.2308");
    }

    #[test]
    fn profile_filters_classify_column_shapes() {
        use ProfileFilter::*;
        let base = ColumnProfile {
            name: "c".into(),
            data_type: String::new(),
            rows: 100,
            nulls: 0,
            distinct: 100,
            distinct_truncated: false,
            numeric_min: None,
            numeric_max: None,
            numeric_mean: None,
            min_length: None,
            max_length: None,
            frequent_values: Vec::new(),
        };

        let unique = base.clone();
        assert!(All.matches(&unique) && Unique.matches(&unique));
        assert!(!WithNulls.matches(&unique) && !LowCardinality.matches(&unique));

        let category = ColumnProfile {
            distinct: 3,
            ..base.clone()
        };
        assert!(LowCardinality.matches(&category) && !Unique.matches(&category));

        let sparse = ColumnProfile {
            nulls: 10,
            distinct: 90,
            ..base.clone()
        };
        assert!(WithNulls.matches(&sparse));
        assert!(!MostlyNull.matches(&sparse) && !Unique.matches(&sparse));

        let mostly_null = ColumnProfile {
            nulls: 60,
            distinct: 40,
            ..base.clone()
        };
        assert!(MostlyNull.matches(&mostly_null) && WithNulls.matches(&mostly_null));

        let all_null = ColumnProfile {
            nulls: 100,
            distinct: 0,
            ..base.clone()
        };
        assert!(Constant.matches(&all_null) && MostlyNull.matches(&all_null));

        let numeric = ColumnProfile {
            numeric_min: Some(1.0),
            numeric_max: Some(9.0),
            ..base.clone()
        };
        assert!(Numeric.matches(&numeric) && !Numeric.matches(&base));

        // A truncated distinct count is unknown, not evidence: no
        // cardinality-based filter may claim the column.
        let truncated = ColumnProfile {
            distinct: DISTINCT_VALUE_CAP,
            distinct_truncated: true,
            rows: DISTINCT_VALUE_CAP,
            ..base.clone()
        };
        assert!(!Unique.matches(&truncated));
        assert!(!Constant.matches(&truncated));
        assert!(!LowCardinality.matches(&truncated));
    }

    #[test]
    fn display_helpers_are_single_line_and_trimmed() {
        assert_eq!(single_line("SELECT *\n  FROM t"), "SELECT * FROM t");
        assert_eq!(format_number(Some(1.0)), "1");
        assert_eq!(format_number(Some(1.25)), "1.25");
        assert_eq!(format_number(None), "—");
    }

    #[test]
    fn missing_cells_count_as_nulls() {
        let mut profiler = ResultProfiler::new(&[column("a"), column("b")]);
        profiler.push_rows(&[vec![CellValue::Bool(true)]]);
        let profile = profiler.finish();
        assert_eq!(profile.columns[1].nulls, 1);
    }

    #[test]
    fn profile_caps_rows_and_labels_the_sample() {
        let rows = (0..=PROFILE_MAX_ROWS)
            .map(|value| vec![CellValue::I64(value as i64)])
            .collect::<Vec<_>>();
        let mut profiler = ResultProfiler::new(&[column("id")]);
        profiler.push_rows(&rows);
        let profile = profiler.finish();
        assert_eq!(profile.rows_profiled, PROFILE_MAX_ROWS);
        assert!(profile.rows_truncated);
        assert!(profile.to_markdown().contains("result was larger"));
    }
}
