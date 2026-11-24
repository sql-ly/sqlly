//! Result set difference: compare two result sets to find added, removed
//! and (with a key) changed rows. The pure core behind the results pane's
//! run-to-run difference view; the app adds the localized summaries.
//!
//! Cells compare by typed equality (see [`cell_equal`]). Without a key the
//! comparison is a multiset one; with a unique, non-NULL key an update is
//! one changed row with per-column details. Output order follows the input
//! order, so the same inputs always give the same diff.

use std::collections::HashMap;

use sqlly_core::{CellValue, ResultColumn};

use super::value::cells_hash;

/// Per-result-set row cap for difference tracking. Diffing builds multiset
/// grouping tables over both sides, so sets larger than this are refused
/// (the same way spill-backed and incomplete sets are skipped).
pub const DIFF_MAX_ROWS: usize = 100_000;

/// Maximum completed runs retained by one query tab while tracking is on.
pub const DIFF_MAX_RUNS: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnChange {
    pub column_index: usize,
    pub column_name: String,
    pub before: CellValue,
    pub after: CellValue,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChangedRow {
    pub key: Vec<CellValue>,
    pub before: Vec<CellValue>,
    pub after: Vec<CellValue>,
    pub changes: Vec<ColumnChange>,
}

/// A typed comparison. With no key the added/removed vectors use multiset
/// semantics; with keys, updates are kept as one row with per-column details
/// instead of being disguised as a delete plus insert.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailedDiff {
    pub added: Vec<Vec<CellValue>>,
    pub removed: Vec<Vec<CellValue>>,
    pub changed: Vec<ChangedRow>,
    pub unchanged_count: usize,
    pub key_columns: Vec<usize>,
}

impl DetailedDiff {
    pub fn is_identical(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Which input a keyed-row problem was found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffInput {
    Baseline,
    Comparison,
}

/// Why two result sets could not be compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffError {
    IncompatibleColumns,
    RowLimit {
        maximum_rows: usize,
    },
    /// 1-based position of a key column the result does not have.
    KeyColumnMissing {
        column_position: usize,
    },
    /// 1-based row number.
    NullKey {
        input: DiffInput,
        row_number: usize,
    },
    DuplicateKey {
        input: DiffInput,
        row_number: usize,
    },
}

/// The result of comparing two result sets row by row.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSetDiff {
    /// True when the column schemas differ (different count or names/types).
    pub schema_mismatch: bool,
    /// Rows present in the new set but not in the old set (by value tuple).
    pub added: Vec<Vec<CellValue>>,
    /// Rows present in the old set but not in the new set (by value tuple).
    pub removed: Vec<Vec<CellValue>>,
    /// Rows present in both sets (unchanged).
    pub unchanged: Vec<Vec<CellValue>>,
}

impl ResultSetDiff {
    /// Number of rows that changed (added + removed).
    pub fn changed_count(&self) -> usize {
        self.added.len() + self.removed.len()
    }

    /// True if there are no differences (no added, no removed, no mismatch).
    pub fn is_identical(&self) -> bool {
        !self.schema_mismatch && self.added.is_empty() && self.removed.is_empty()
    }
}

/// Compare compatible result sets, optionally aligning rows with a unique,
/// non-null composite key.
pub fn compare_detailed(
    columns: &[ResultColumn],
    baseline_rows: &[Vec<CellValue>],
    comparison_columns: &[ResultColumn],
    comparison_rows: &[Vec<CellValue>],
    key_columns: &[usize],
) -> Result<DetailedDiff, DiffError> {
    if !schemas_match(columns, comparison_columns) {
        return Err(DiffError::IncompatibleColumns);
    }
    if baseline_rows.len() > DIFF_MAX_ROWS || comparison_rows.len() > DIFF_MAX_ROWS {
        return Err(DiffError::RowLimit {
            maximum_rows: DIFF_MAX_ROWS,
        });
    }
    if key_columns.is_empty() {
        let multiset = diff(columns, baseline_rows, comparison_columns, comparison_rows);
        return Ok(DetailedDiff {
            added: multiset.added,
            removed: multiset.removed,
            changed: Vec::new(),
            unchanged_count: multiset.unchanged.len(),
            key_columns: Vec::new(),
        });
    }
    let mut keys = key_columns.to_vec();
    keys.sort_unstable();
    keys.dedup();
    if let Some(index) = keys.iter().copied().find(|index| *index >= columns.len()) {
        return Err(DiffError::KeyColumnMissing {
            column_position: index + 1,
        });
    }

    let baseline = KeyedRows::build(DiffInput::Baseline, baseline_rows, &keys)?;
    let comparison = KeyedRows::build(DiffInput::Comparison, comparison_rows, &keys)?;
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged_count = 0;

    for (key, before) in baseline.entries() {
        let Some(after) = comparison.get(&key) else {
            removed.push(before.clone());
            continue;
        };
        let changes = before
            .iter()
            .zip(after)
            .enumerate()
            .filter(|(index, (left, right))| !keys.contains(index) && !cell_equal(left, right))
            .map(|(index, (before, after))| ColumnChange {
                column_index: index,
                column_name: columns[index].name.clone(),
                before: before.clone(),
                after: after.clone(),
            })
            .collect::<Vec<_>>();
        if changes.is_empty() {
            unchanged_count += 1;
        } else {
            changed.push(ChangedRow {
                key,
                before: before.clone(),
                after: after.clone(),
                changes,
            });
        }
    }
    for (key, row) in comparison.entries() {
        if baseline.get(&key).is_none() {
            added.push(row.clone());
        }
    }

    Ok(DetailedDiff {
        added,
        removed,
        changed,
        unchanged_count,
        key_columns: keys,
    })
}

struct KeyedRows<'a> {
    rows: &'a [Vec<CellValue>],
    keys: &'a [usize],
    by_hash: HashMap<u64, Vec<usize>>,
}

impl<'a> KeyedRows<'a> {
    fn build(
        input: DiffInput,
        rows: &'a [Vec<CellValue>],
        keys: &'a [usize],
    ) -> Result<Self, DiffError> {
        let mut keyed = Self {
            rows,
            keys,
            by_hash: HashMap::with_capacity(rows.len()),
        };
        for (row_index, row) in rows.iter().enumerate() {
            let key = key_of(row, keys);
            if key.iter().any(|cell| matches!(cell, CellValue::Null)) {
                return Err(DiffError::NullKey {
                    input,
                    row_number: row_index + 1,
                });
            }
            if keyed.get(&key).is_some() {
                return Err(DiffError::DuplicateKey {
                    input,
                    row_number: row_index + 1,
                });
            }
            keyed
                .by_hash
                .entry(cells_hash(&key, 0))
                .or_default()
                .push(row_index);
        }
        Ok(keyed)
    }

    fn get(&self, key: &[CellValue]) -> Option<&'a Vec<CellValue>> {
        let rows = self.rows;
        self.by_hash
            .get(&cells_hash(key, 0))?
            .iter()
            .map(|index| &rows[*index])
            .find(|row| rows_equal(&key_of(row, self.keys), key))
    }

    fn entries(&self) -> impl Iterator<Item = (Vec<CellValue>, &'a Vec<CellValue>)> + '_ {
        self.rows.iter().map(|row| (key_of(row, self.keys), row))
    }
}

fn key_of(row: &[CellValue], keys: &[usize]) -> Vec<CellValue> {
    keys.iter()
        .map(|index| row.get(*index).cloned().unwrap_or(CellValue::Null))
        .collect()
}

/// Compare two result sets by row value tuples.
///
/// Rows are compared by exact cell-value equality. A row is "the same" if
/// every cell matches. The order of rows does not matter — this is a set
/// comparison, not a sequence comparison. Duplicate rows are handled by
/// multiset semantics: if the old set has two identical rows and the new set
/// has one, one is "removed" and one is "unchanged".
pub fn diff(
    old_columns: &[ResultColumn],
    old_rows: &[Vec<CellValue>],
    new_columns: &[ResultColumn],
    new_rows: &[Vec<CellValue>],
) -> ResultSetDiff {
    if !schemas_match(old_columns, new_columns) {
        return ResultSetDiff {
            schema_mismatch: true,
            added: Vec::new(),
            removed: Vec::new(),
            unchanged: Vec::new(),
        };
    }

    let old_groups = RowGroups::build(old_rows);
    let new_groups = RowGroups::build(new_rows);
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = Vec::new();

    for (row, old_count) in old_groups.groups() {
        let new_count = new_groups.count_of(row);
        let common = old_count.min(new_count);
        unchanged.extend(std::iter::repeat_n(row, common).cloned());
        removed.extend(std::iter::repeat_n(row, old_count - common).cloned());
    }
    for (row, new_count) in new_groups.groups() {
        let old_count = old_groups.count_of(row);
        added.extend(std::iter::repeat_n(row, new_count.saturating_sub(old_count)).cloned());
    }

    ResultSetDiff {
        schema_mismatch: false,
        added,
        removed,
        unchanged,
    }
}

/// The same comparison as [`diff`]; kept as a separate name for the app's
/// existing callers. Grouping is hash-based for every size.
pub fn diff_optimized(
    old_columns: &[ResultColumn],
    old_rows: &[Vec<CellValue>],
    new_columns: &[ResultColumn],
    new_rows: &[Vec<CellValue>],
) -> ResultSetDiff {
    diff(old_columns, old_rows, new_columns, new_rows)
}

/// Distinct rows in first-appearance order, with their counts.
struct RowGroups<'a> {
    order: Vec<(&'a Vec<CellValue>, usize)>,
    by_hash: HashMap<u64, Vec<usize>>,
}

impl<'a> RowGroups<'a> {
    fn build(rows: &'a [Vec<CellValue>]) -> Self {
        let mut groups = Self {
            order: Vec::new(),
            by_hash: HashMap::new(),
        };
        for row in rows {
            let bucket = groups.by_hash.entry(cells_hash(row, 0)).or_default();
            match bucket
                .iter()
                .find(|index| rows_equal(groups.order[**index].0, row))
            {
                Some(index) => groups.order[*index].1 += 1,
                None => {
                    bucket.push(groups.order.len());
                    groups.order.push((row, 1));
                }
            }
        }
        groups
    }

    fn count_of(&self, row: &[CellValue]) -> usize {
        self.by_hash
            .get(&cells_hash(row, 0))
            .and_then(|bucket| {
                bucket
                    .iter()
                    .find(|index| rows_equal(self.order[**index].0, row))
            })
            .map(|index| self.order[*index].1)
            .unwrap_or(0)
    }

    fn groups(&self) -> impl Iterator<Item = (&'a Vec<CellValue>, usize)> + '_ {
        self.order.iter().copied()
    }
}

/// Check if two column schemas match (same count, same names, same types).
pub fn schemas_match(a: &[ResultColumn], b: &[ResultColumn]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(ca, cb)| ca.name == cb.name && ca.data_type == cb.data_type)
}

/// Compare two rows by typed cell equality: every cell must match in both
/// variant and value (see [`cell_equal`]).
pub fn rows_equal(a: &[CellValue], b: &[CellValue]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(ca, cb)| cell_equal(ca, cb))
}

/// Typed cell equality: two cells are equal only when their `CellValue`
/// variants match AND their values match. `I64(1)`, `F64(1.0)`, `String("1")`,
/// and `Bool(true)` are four different values — a type change between runs is
/// a real difference and must show up as one. Within `F64`, NaN compares equal
/// to NaN (so a NaN cell doesn't make every run look changed) and all other
/// values compare by IEEE equality. `Null` equals only `Null`; `Bytes` compare
/// by content.
pub fn cell_equal(a: &CellValue, b: &CellValue) -> bool {
    match (a, b) {
        (CellValue::Null, CellValue::Null) => true,
        (CellValue::Bool(x), CellValue::Bool(y)) => x == y,
        (CellValue::I64(x), CellValue::I64(y)) => x == y,
        (CellValue::F64(x), CellValue::F64(y)) => (x.is_nan() && y.is_nan()) || x == y,
        (CellValue::String(x), CellValue::String(y)) => x == y,
        (CellValue::Bytes(x), CellValue::Bytes(y)) => x == y,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::test_support::col;

    fn row(vals: &[CellValue]) -> Vec<CellValue> {
        vals.to_vec()
    }

    fn ints(range: std::ops::Range<i64>) -> Vec<Vec<CellValue>> {
        range.map(|i| row(&[CellValue::I64(i)])).collect()
    }

    #[test]
    fn multiset_counts_table() {
        let cols = vec![col("id", "int")];
        type Case = (
            Vec<Vec<CellValue>>,
            Vec<Vec<CellValue>>,
            (usize, usize, usize),
        );
        let cases: Vec<Case> = vec![
            (ints(1..3), ints(1..3), (0, 0, 2)),
            (ints(1..2), ints(1..4), (2, 0, 1)),
            (ints(1..4), ints(1..2), (0, 2, 1)),
            (Vec::new(), Vec::new(), (0, 0, 0)),
            (
                vec![
                    row(&[CellValue::I64(1)]),
                    row(&[CellValue::I64(1)]),
                    row(&[CellValue::I64(2)]),
                ],
                vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(3)])],
                (1, 2, 1),
            ),
            (ints(0..600), ints(100..700), (100, 100, 500)),
        ];
        for (old, new, (added, removed, unchanged)) in cases {
            let d = diff(&cols, &old, &cols, &new);
            assert_eq!(
                (d.added.len(), d.removed.len(), d.unchanged.len()),
                (added, removed, unchanged)
            );
            assert_eq!(d, diff_optimized(&cols, &old, &cols, &new));
        }
    }

    #[test]
    fn schema_mismatches_are_reported() {
        let cases = [
            (
                vec![col("id", "int"), col("name", "varchar")],
                vec![col("id", "int")],
            ),
            (vec![col("id", "int")], vec![col("count", "int")]),
            (vec![col("id", "int")], vec![col("id", "varchar")]),
        ];
        for (old, new) in cases {
            assert!(!schemas_match(&old, &new));
            let d = diff(&old, &[], &new, &[]);
            assert!(d.schema_mismatch);
            assert!(!d.is_identical());
        }
        assert!(schemas_match(&[col("id", "int")], &[col("id", "int")]));
    }

    #[test]
    fn typed_equality_table() {
        let cases: Vec<(CellValue, CellValue, bool)> = vec![
            (CellValue::I64(1), CellValue::String("1".into()), false),
            (CellValue::I64(1), CellValue::F64(1.0), false),
            (CellValue::Bool(true), CellValue::I64(1), false),
            (CellValue::F64(f64::NAN), CellValue::F64(f64::NAN), true),
            (CellValue::F64(0.0), CellValue::F64(-0.0), true),
            (
                CellValue::F64(1.5),
                CellValue::F64(1.5 + f64::EPSILON),
                false,
            ),
            (CellValue::Null, CellValue::Null, true),
            (CellValue::Null, CellValue::String(String::new()), false),
            (
                CellValue::Bytes(vec![1, 2]),
                CellValue::Bytes(vec![1, 2]),
                true,
            ),
            (
                CellValue::Bytes(vec![0xAB]),
                CellValue::Bytes(vec![0xAC]),
                false,
            ),
        ];
        let cols = vec![col("x", "variant")];
        for (a, b, equal) in cases {
            assert_eq!(cell_equal(&a, &b), equal, "{a:?} vs {b:?}");
            let d = diff(&cols, &[vec![a.clone()]], &cols, &[vec![b.clone()]]);
            assert_eq!(d.is_identical(), equal, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn hashed_path_keeps_nan_equal_and_types_apart() {
        let cols = vec![col("id", "int"), col("x", "float")];
        let old: Vec<Vec<CellValue>> = (0..600)
            .map(|i| row(&[CellValue::I64(i), CellValue::F64(f64::NAN)]))
            .collect();
        assert!(diff_optimized(&cols, &old, &cols, &old).is_identical());
        let as_text: Vec<Vec<CellValue>> = (0..600)
            .map(|i| row(&[CellValue::String(i.to_string()), CellValue::F64(f64::NAN)]))
            .collect();
        let d = diff_optimized(&cols, &old, &cols, &as_text);
        assert_eq!((d.added.len(), d.removed.len()), (600, 600));
    }

    #[test]
    fn composite_key_reports_typed_per_column_updates() {
        let cols = vec![
            col("tenant", "int"),
            col("id", "int"),
            col("name", "text"),
            col("active", "bit"),
        ];
        let old = vec![row(&[
            CellValue::I64(7),
            CellValue::I64(1),
            CellValue::String("Ada".into()),
            CellValue::Bool(false),
        ])];
        let new = vec![row(&[
            CellValue::I64(7),
            CellValue::I64(1),
            CellValue::String("Grace".into()),
            CellValue::Bool(true),
        ])];
        let result = compare_detailed(&cols, &old, &cols, &new, &[1, 0, 1]).expect("valid keys");
        assert_eq!(result.key_columns, vec![0, 1]);
        assert!(result.added.is_empty() && result.removed.is_empty());
        assert_eq!(result.changed.len(), 1);
        assert_eq!(
            result.changed[0].key,
            vec![CellValue::I64(7), CellValue::I64(1)]
        );
        let names: Vec<&str> = result.changed[0]
            .changes
            .iter()
            .map(|c| c.column_name.as_str())
            .collect();
        assert_eq!(names, vec!["name", "active"]);
    }

    #[test]
    fn keyed_comparison_rejects_bad_keys_and_inputs() {
        let cols = vec![col("id", "int"), col("value", "text")];
        let duplicate = vec![
            row(&[CellValue::I64(1), CellValue::String("a".into())]),
            row(&[CellValue::I64(1), CellValue::String("b".into())]),
        ];
        assert_eq!(
            compare_detailed(&cols, &duplicate, &cols, &[], &[0]).unwrap_err(),
            DiffError::DuplicateKey {
                input: DiffInput::Baseline,
                row_number: 2
            }
        );
        let null = vec![row(&[CellValue::Null, CellValue::String("a".into())])];
        assert_eq!(
            compare_detailed(&cols, &[], &cols, &null, &[0]).unwrap_err(),
            DiffError::NullKey {
                input: DiffInput::Comparison,
                row_number: 1
            }
        );
        assert_eq!(
            compare_detailed(&cols, &[], &cols, &[], &[5]).unwrap_err(),
            DiffError::KeyColumnMissing { column_position: 6 }
        );
        assert_eq!(
            compare_detailed(&cols, &[], &[col("id", "int")], &[], &[]).unwrap_err(),
            DiffError::IncompatibleColumns
        );
        let rows = vec![row(&[CellValue::I64(1), CellValue::Null]); DIFF_MAX_ROWS + 1];
        assert_eq!(
            compare_detailed(&cols, &rows, &cols, &[], &[]).unwrap_err(),
            DiffError::RowLimit {
                maximum_rows: DIFF_MAX_ROWS
            }
        );
    }

    #[test]
    fn keyed_output_follows_input_order() {
        let cols = vec![col("id", "int"), col("v", "int")];
        let old: Vec<Vec<CellValue>> = (0..50)
            .map(|i| row(&[CellValue::I64(i), CellValue::I64(i)]))
            .collect();
        let new: Vec<Vec<CellValue>> = (25..75)
            .rev()
            .map(|i| row(&[CellValue::I64(i), CellValue::I64(i * 2)]))
            .collect();
        let result = compare_detailed(&cols, &old, &cols, &new, &[0]).unwrap();
        assert_eq!(result.removed.first().unwrap()[0], CellValue::I64(0));
        assert_eq!(result.added.first().unwrap()[0], CellValue::I64(74));
        assert_eq!(
            result.changed.first().unwrap().key,
            vec![CellValue::I64(25)]
        );
        assert_eq!(
            result,
            compare_detailed(&cols, &old, &cols, &new, &[0]).unwrap()
        );
    }

    #[test]
    fn no_key_fallback_preserves_typed_multiset_counts() {
        let cols = vec![col("value", "variant")];
        let old = vec![
            row(&[CellValue::I64(1)]),
            row(&[CellValue::I64(1)]),
            row(&[CellValue::String("1".into())]),
        ];
        let new = vec![row(&[CellValue::I64(1)]), row(&[CellValue::Bool(true)])];
        let result = compare_detailed(&cols, &old, &cols, &new, &[]).expect("compatible");
        assert_eq!(result.unchanged_count, 1);
        assert_eq!(result.removed.len(), 2);
        assert_eq!(result.added, vec![row(&[CellValue::Bool(true)])]);
        assert!(result.changed.is_empty());
    }
}
