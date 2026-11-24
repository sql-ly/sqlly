//! Result analytics for notebook cells: column profiles, assertions, KPI
//! math, result diffs, chart suggestions, time-series helpers and the scalar
//! formula evaluator.
//!
//! Everything here is pure and deterministic: no I/O, no clock reads (the
//! caller injects "now" where it matters), no gpui, and it compiles for
//! wasm32. Inputs are the row-oriented results every engine produces
//! (`ResultColumn` + `CellValue` rows), read through [`OutputView`] so a
//! spill-backed output can stream batches instead of materializing.
//!
//! Every analysis carries the input's [`Completeness`] through. A truncated,
//! sampled or partial input is never presented as the whole: profiles flag
//! it, and assertions report `Inconclusive` unless they opt in.

#![forbid(unsafe_code)]

pub mod assertions;
pub mod decimal;
pub mod diff;
pub mod kpi;
pub mod profile;
pub mod resultset;
pub mod scalar;
pub mod sketch;
pub mod suggest;
pub mod time;
pub mod timeseries;
pub mod value;

use std::borrow::Cow;
use std::ops::ControlFlow;

use serde::{Deserialize, Serialize};
use sqlly_core::{CellValue, ResultColumn};

/// How much of the underlying result an analysis saw. Mirrors the notebook
/// `CellOutput` completeness contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Completeness {
    /// Every row of the result was read.
    #[default]
    Complete,
    /// Only the first `cap` rows were read; the result had more.
    Truncated { cap: u64 },
    /// The result spilled to disk and every one of its `total` rows was read
    /// back through the spill. Pass [`Completeness::Truncated`] instead when
    /// only the in-memory snapshot of a spilled result is analysed.
    Spilled { total: u64 },
    /// A sample of `n` rows was read.
    Sampled { n: u64 },
    /// The query stopped early (cancel, error, timeout) after some rows.
    Partial,
}

impl Completeness {
    /// Whether the analysis saw every row of the result.
    pub fn is_complete(self) -> bool {
        matches!(self, Completeness::Complete | Completeness::Spilled { .. })
    }
}

/// A re-iterable source of row batches, so a spill-backed result can be
/// analysed without loading it whole. Each call starts again from the first
/// row; the visitor returns `ControlFlow::Break` to stop early.
pub trait RowSource {
    fn for_each_batch(&self, visit: &mut dyn FnMut(&[Vec<CellValue>]) -> ControlFlow<()>);

    /// The row count when known up front.
    fn row_count_hint(&self) -> Option<u64> {
        None
    }
}

/// Rows already split into batches (as a spill reader hands them back).
pub struct BatchedRows<'a>(pub &'a [Vec<Vec<CellValue>>]);

impl RowSource for BatchedRows<'_> {
    fn for_each_batch(&self, visit: &mut dyn FnMut(&[Vec<CellValue>]) -> ControlFlow<()>) {
        for batch in self.0 {
            if visit(batch).is_break() {
                return;
            }
        }
    }

    fn row_count_hint(&self) -> Option<u64> {
        Some(self.0.iter().map(|batch| batch.len() as u64).sum())
    }
}

#[derive(Clone, Copy)]
enum Rows<'a> {
    Slice(&'a [Vec<CellValue>]),
    Source(&'a dyn RowSource),
}

/// One result as the analytics read it: its columns, its rows, and how much
/// of the underlying result those rows are.
#[derive(Clone, Copy)]
pub struct OutputView<'a> {
    pub columns: &'a [ResultColumn],
    rows: Rows<'a>,
    pub completeness: Completeness,
}

impl<'a> OutputView<'a> {
    pub fn new(
        columns: &'a [ResultColumn],
        rows: &'a [Vec<CellValue>],
        completeness: Completeness,
    ) -> Self {
        Self {
            columns,
            rows: Rows::Slice(rows),
            completeness,
        }
    }

    /// A complete in-memory result.
    pub fn complete(columns: &'a [ResultColumn], rows: &'a [Vec<CellValue>]) -> Self {
        Self::new(columns, rows, Completeness::Complete)
    }

    pub fn from_source(
        columns: &'a [ResultColumn],
        rows: &'a dyn RowSource,
        completeness: Completeness,
    ) -> Self {
        Self {
            columns,
            rows: Rows::Source(rows),
            completeness,
        }
    }

    /// Index of the column named `name`: exact match first, then
    /// case-insensitive.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        column_index(self.columns, name)
    }

    /// The in-memory rows, when the view is backed by a slice.
    pub fn as_slice(&self) -> Option<&'a [Vec<CellValue>]> {
        match self.rows {
            Rows::Slice(rows) => Some(rows),
            Rows::Source(_) => None,
        }
    }

    /// Visit each batch of rows in order.
    pub fn for_each_batch(&self, visit: &mut dyn FnMut(&[Vec<CellValue>]) -> ControlFlow<()>) {
        match self.rows {
            Rows::Slice(rows) => {
                let _ = visit(rows);
            }
            Rows::Source(source) => source.for_each_batch(visit),
        }
    }

    /// Visit each row in order with its 0-based index.
    pub fn for_each_row(&self, mut visit: impl FnMut(usize, &[CellValue]) -> ControlFlow<()>) {
        let mut index = 0usize;
        self.for_each_batch(&mut |batch| {
            for row in batch {
                visit(index, row)?;
                index += 1;
            }
            ControlFlow::Continue(())
        });
    }

    /// Number of rows the view holds (a full pass for a streaming source
    /// without a hint).
    pub fn row_count(&self) -> u64 {
        match self.rows {
            Rows::Slice(rows) => rows.len() as u64,
            Rows::Source(source) => source.row_count_hint().unwrap_or_else(|| {
                let mut count = 0u64;
                source.for_each_batch(&mut |batch| {
                    count += batch.len() as u64;
                    ControlFlow::Continue(())
                });
                count
            }),
        }
    }

    /// The rows as one slice, borrowing when possible and collecting a
    /// streaming source otherwise. `None` when the view holds more than
    /// `cap` rows.
    pub fn rows_up_to(&self, cap: usize) -> Option<Cow<'a, [Vec<CellValue>]>> {
        match self.rows {
            Rows::Slice(rows) => (rows.len() <= cap).then_some(Cow::Borrowed(rows)),
            Rows::Source(source) => {
                let mut collected = Vec::new();
                let mut over = false;
                source.for_each_batch(&mut |batch| {
                    if collected.len() + batch.len() > cap {
                        over = true;
                        return ControlFlow::Break(());
                    }
                    collected.extend_from_slice(batch);
                    ControlFlow::Continue(())
                });
                (!over).then_some(Cow::Owned(collected))
            }
        }
    }
}

/// Index of the column named `name`: exact match first, then
/// case-insensitive.
pub fn column_index(columns: &[ResultColumn], name: &str) -> Option<usize> {
    columns
        .iter()
        .position(|column| column.name == name)
        .or_else(|| {
            columns
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case(name))
        })
}

#[cfg(test)]
pub(crate) mod test_support {
    use sqlly_core::{CellValue, ResultColumn};

    pub fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    pub fn s(text: &str) -> CellValue {
        CellValue::String(text.to_string())
    }

    pub fn i(value: i64) -> CellValue {
        CellValue::I64(value)
    }

    pub fn f(value: f64) -> CellValue {
        CellValue::F64(value)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn views_over_slices_and_batches_visit_the_same_rows() {
        let columns = vec![col("n", "int")];
        let rows: Vec<Vec<CellValue>> = (0..7).map(|n| vec![i(n)]).collect();
        let batches = vec![rows[..3].to_vec(), rows[3..].to_vec()];
        let source = BatchedRows(&batches);
        let slice_view = OutputView::complete(&columns, &rows);
        let batch_view = OutputView::from_source(&columns, &source, Completeness::Complete);
        let mut seen = Vec::new();
        batch_view.for_each_row(|index, row| {
            seen.push((index, row[0].clone()));
            ControlFlow::Continue(())
        });
        assert_eq!(seen.len(), 7);
        assert_eq!(seen[6], (6, i(6)));
        assert_eq!(slice_view.row_count(), batch_view.row_count());
        assert_eq!(batch_view.rows_up_to(10).unwrap().len(), 7);
        assert!(batch_view.rows_up_to(5).is_none());
        assert!(slice_view.rows_up_to(6).is_none());
    }

    #[test]
    fn completeness_flags_only_full_reads_as_complete() {
        assert!(Completeness::Complete.is_complete());
        assert!(Completeness::Spilled { total: 10 }.is_complete());
        assert!(!Completeness::Truncated { cap: 10 }.is_complete());
        assert!(!Completeness::Sampled { n: 10 }.is_complete());
        assert!(!Completeness::Partial.is_complete());
        let json = serde_json::to_string(&Completeness::Sampled { n: 5 }).unwrap();
        assert_eq!(json, r#"{"kind":"sampled","n":5}"#);
    }

    #[test]
    fn column_lookup_prefers_exact_then_case_insensitive() {
        let columns = vec![col("Total", "int"), col("total", "int")];
        assert_eq!(column_index(&columns, "total"), Some(1));
        assert_eq!(column_index(&columns, "TOTAL"), Some(0));
        assert_eq!(column_index(&columns, "missing"), None);
    }
}
