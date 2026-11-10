//! Result set difference — compare two result sets to find added/removed rows.
//!
//! Port of `ResultSetDifference.swift`. Pure logic: takes two result sets
//! (columns + rows) and produces a diff showing which rows were added,
//! removed, or unchanged. Schema mismatches are detected and reported.

use std::hash::{Hash, Hasher};

use sqlly_core::{CellValue, ResultColumn};

/// Per-result-set row cap for difference tracking. Diffing clones the full
/// previous set and builds multiset grouping tables over both sides, so an
/// unbounded set would roughly triple its resident memory. Sets larger than
/// this are not retained for diffing and the diff is skipped (the same way
/// spill-backed and incomplete sets are skipped).
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

/// A typed comparison suitable for both the UI and exports. With no key the
/// added/removed vectors use multiset semantics; with keys, updates are kept
/// as one row with per-column details instead of being disguised as a delete
/// plus insert.
#[derive(Debug, Clone, PartialEq)]
pub struct DetailedDiff {
    pub added: Vec<Vec<CellValue>>,
    pub removed: Vec<Vec<CellValue>>,
    pub changed: Vec<ChangedRow>,
    pub unchanged_count: usize,
    pub key_columns: Vec<usize>,
}

type KeyedRows = std::collections::HashMap<u64, Vec<(Vec<CellValue>, Vec<CellValue>)>>;

impl DetailedDiff {
    pub fn is_identical(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    pub fn summary(&self) -> String {
        crate::localization::format(
            "results.difference.detailed_summary",
            &[
                ("added_count", &self.added.len().to_string()),
                ("removed_count", &self.removed.len().to_string()),
                ("changed_count", &self.changed.len().to_string()),
                ("unchanged_count", &self.unchanged_count.to_string()),
            ],
        )
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
) -> Result<DetailedDiff, String> {
    if !schemas_match(columns, comparison_columns) {
        return Err(crate::localization::text(
            "results.difference.incompatible_columns",
        ));
    }
    if baseline_rows.len() > DIFF_MAX_ROWS || comparison_rows.len() > DIFF_MAX_ROWS {
        return Err(crate::localization::format(
            "results.difference.row_limit",
            &[("maximum_rows", &DIFF_MAX_ROWS.to_string())],
        ));
    }
    if key_columns.is_empty() {
        let legacy = diff_optimized(columns, baseline_rows, comparison_columns, comparison_rows);
        return Ok(DetailedDiff {
            added: legacy.added,
            removed: legacy.removed,
            changed: Vec::new(),
            unchanged_count: legacy.unchanged.len(),
            key_columns: Vec::new(),
        });
    }
    let mut keys = key_columns.to_vec();
    keys.sort_unstable();
    keys.dedup();
    if let Some(index) = keys.iter().copied().find(|index| *index >= columns.len()) {
        return Err(crate::localization::format(
            "results.difference.key_column_missing",
            &[("column_position", &(index + 1).to_string())],
        ));
    }

    let baseline = keyed_rows("baseline", baseline_rows, &keys)?;
    let comparison = keyed_rows("comparison", comparison_rows, &keys)?;
    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut unchanged_count = 0;

    for (key, before) in baseline.values().flatten() {
        let Some((_, after)) = comparison.get(&row_hash(key)).and_then(|bucket| {
            bucket
                .iter()
                .find(|(candidate, _)| rows_equal(candidate, key))
        }) else {
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
                key: key.clone(),
                before: before.clone(),
                after: after.clone(),
                changes,
            });
        }
    }
    for (key, row) in comparison.values().flatten() {
        if !baseline.get(&row_hash(key)).is_some_and(|bucket| {
            bucket
                .iter()
                .any(|(candidate, _)| rows_equal(candidate, key))
        }) {
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

fn keyed_rows(
    side: &str,
    rows: &[Vec<CellValue>],
    key_columns: &[usize],
) -> Result<KeyedRows, String> {
    let mut keyed = KeyedRows::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let key = key_columns
            .iter()
            .map(|index| row.get(*index).cloned().unwrap_or(CellValue::Null))
            .collect::<Vec<_>>();
        if key.iter().any(|cell| matches!(cell, CellValue::Null)) {
            let key = if side == "baseline" {
                "results.difference.baseline_null_key"
            } else {
                "results.difference.comparison_null_key"
            };
            return Err(crate::localization::format(
                key,
                &[("row_number", &(row_index + 1).to_string())],
            ));
        }
        let bucket = keyed.entry(row_hash(&key)).or_default();
        if bucket
            .iter()
            .any(|(candidate, _)| rows_equal(candidate, &key))
        {
            let key = if side == "baseline" {
                "results.difference.baseline_duplicate_key"
            } else {
                "results.difference.comparison_duplicate_key"
            };
            return Err(crate::localization::format(
                key,
                &[("row_number", &(row_index + 1).to_string())],
            ));
        }
        bucket.push((key, row.clone()));
    }
    Ok(keyed)
}

/// The result of comparing two result sets.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSetDiff {
    /// True when the column schemas differ (different count or names/types).
    pub schema_mismatch: bool,
    /// Human-readable description of the schema mismatch, if any.
    pub mismatch_detail: Option<String>,
    /// Rows present in the new set but not in the old set (by value tuple).
    pub added: Vec<Vec<CellValue>>,
    /// Rows present in the old set but not in the new set (by value tuple).
    pub removed: Vec<Vec<CellValue>>,
    /// Rows present in both sets (unchanged).
    pub unchanged: Vec<Vec<CellValue>>,
}

impl ResultSetDiff {
    #[allow(dead_code)]
    /// Number of rows that changed (added + removed).
    pub fn changed_count(&self) -> usize {
        self.added.len() + self.removed.len()
    }

    /// True if there are no differences (no added, no removed, no mismatch).
    pub fn is_identical(&self) -> bool {
        !self.schema_mismatch && self.added.is_empty() && self.removed.is_empty()
    }

    /// Summary text for display, e.g. "+3 added, -1 removed, 10 unchanged".
    pub fn summary(&self) -> String {
        if self.schema_mismatch {
            return crate::localization::text("results.difference.schema_mismatch_summary");
        }
        crate::localization::format(
            "results.difference.summary",
            &[
                ("added_count", &self.added.len().to_string()),
                ("removed_count", &self.removed.len().to_string()),
                ("unchanged_count", &self.unchanged.len().to_string()),
            ],
        )
    }
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
    // Schema check: same column count and same names.
    let schema_mismatch = !schemas_match(old_columns, new_columns);
    if schema_mismatch {
        return ResultSetDiff {
            schema_mismatch: true,
            mismatch_detail: Some(crate::localization::format(
                "results.difference.schema_mismatch_detail",
                &[
                    ("old_count", &old_columns.len().to_string()),
                    (
                        "old_columns",
                        &old_columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    ("new_count", &new_columns.len().to_string()),
                    (
                        "new_columns",
                        &new_columns
                            .iter()
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            )),
            added: Vec::new(),
            removed: Vec::new(),
            unchanged: Vec::new(),
        };
    }

    // Multiset diff: count occurrences of each row tuple in both sets.
    let mut old_counts: Vec<(Vec<CellValue>, usize)> = group_rows(old_rows);
    let mut new_counts: Vec<(Vec<CellValue>, usize)> = group_rows(new_rows);

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = Vec::new();

    // For each unique row in the old set, find how many are in the new set.
    for (row, old_count) in &mut old_counts {
        let new_count = new_counts
            .iter()
            .find(|(r, _)| rows_equal(r, row))
            .map(|(_, c)| *c)
            .unwrap_or(0);
        let common = (*old_count).min(new_count);
        let rem = *old_count - common;
        for _ in 0..common {
            unchanged.push(row.clone());
        }
        for _ in 0..rem {
            removed.push(row.clone());
        }
        // Store the common count back so the added loop can subtract it.
        *old_count = common;
    }

    // For each unique row in the new set, find how many are NOT in the old set.
    // `old_counts` now holds the common (intersection) count per row.
    for (row, new_count) in &mut new_counts {
        let common = old_counts
            .iter()
            .find(|(r, _)| rows_equal(r, row))
            .map(|(_, c)| *c)
            .unwrap_or(0);
        let extra = new_count.saturating_sub(common);
        for _ in 0..extra {
            added.push(row.clone());
        }
    }

    ResultSetDiff {
        schema_mismatch: false,
        mismatch_detail: None,
        added,
        removed,
        unchanged,
    }
}

/// Check if two column schemas match (same count, same names, same types).
pub fn schemas_match(a: &[ResultColumn], b: &[ResultColumn]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(ca, cb)| ca.name == cb.name && ca.data_type == cb.data_type)
}

/// Group rows by value tuple, returning (row, count) pairs.
fn group_rows(rows: &[Vec<CellValue>]) -> Vec<(Vec<CellValue>, usize)> {
    let mut groups: Vec<(Vec<CellValue>, usize)> = Vec::new();
    for row in rows {
        if let Some(entry) = groups.iter_mut().find(|(r, _)| rows_equal(r, row)) {
            entry.1 += 1;
        } else {
            groups.push((row.clone(), 1));
        }
    }
    groups
}

/// Compare two rows by typed cell equality: every cell must match in both
/// variant and value (see [`cell_equal`]).
fn rows_equal(a: &[CellValue], b: &[CellValue]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(ca, cb)| cell_equal(ca, cb))
}

/// Typed cell equality: two cells are equal only when their `CellValue`
/// variants match AND their values match. `I64(1)`, `F64(1.0)`, `String("1")`,
/// and `Bool(true)` are four different values — a type change between runs is
/// a real difference and must show up as one. Within `F64`, NaN compares equal
/// to NaN (so a NaN cell doesn't make every run look changed) and all other
/// values compare by IEEE equality. `Null` equals only `Null`; `Bytes` compare
/// by content.
fn cell_equal(a: &CellValue, b: &CellValue) -> bool {
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

/// Compute a quick hash of a row for use in HashSet-based lookups.
/// This is a fast path; actual equality is checked with `rows_equal`.
fn row_hash(row: &[CellValue]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for cell in row {
        cell_hash(cell, &mut hasher);
    }
    hasher.finish()
}

/// Hash a cell consistently with [`cell_equal`]: a per-variant discriminant
/// tag keeps keys from colliding across variants, and `F64` normalizes the
/// hashed bits so the values `cell_equal` treats as equal (all NaNs; +0.0 and
/// -0.0) hash identically.
fn cell_hash<H: Hasher>(cell: &CellValue, hasher: &mut H) {
    match cell {
        CellValue::Null => 0u8.hash(hasher),
        CellValue::Bool(b) => {
            1u8.hash(hasher);
            b.hash(hasher);
        }
        CellValue::I64(v) => {
            2u8.hash(hasher);
            v.hash(hasher);
        }
        CellValue::F64(v) => {
            3u8.hash(hasher);
            let bits = if v.is_nan() {
                f64::NAN.to_bits()
            } else if *v == 0.0 {
                0f64.to_bits()
            } else {
                v.to_bits()
            };
            bits.hash(hasher);
        }
        CellValue::String(s) => {
            4u8.hash(hasher);
            s.hash(hasher);
        }
        CellValue::Bytes(b) => {
            5u8.hash(hasher);
            b.hash(hasher);
        }
    }
}

/// Optimized diff using hash-based grouping for large row sets.
/// Falls back to the linear-scan `diff` for small sets.
pub fn diff_optimized(
    old_columns: &[ResultColumn],
    old_rows: &[Vec<CellValue>],
    new_columns: &[ResultColumn],
    new_rows: &[Vec<CellValue>],
) -> ResultSetDiff {
    // For small datasets, the linear scan is fine and avoids hashing overhead.
    const LINEAR_THRESHOLD: usize = 500;
    if old_rows.len() < LINEAR_THRESHOLD && new_rows.len() < LINEAR_THRESHOLD {
        return diff(old_columns, old_rows, new_columns, new_rows);
    }

    // Schema check first.
    if !schemas_match(old_columns, new_columns) {
        return diff(old_columns, old_rows, new_columns, new_rows);
    }

    // Hash-based grouping for large sets.
    let mut old_groups: std::collections::HashMap<u64, Vec<(Vec<CellValue>, usize)>> =
        std::collections::HashMap::new();
    for row in old_rows {
        let h = row_hash(row);
        let bucket = old_groups.entry(h).or_default();
        if let Some(entry) = bucket.iter_mut().find(|(r, _)| rows_equal(r, row)) {
            entry.1 += 1;
        } else {
            bucket.push((row.clone(), 1));
        }
    }

    let mut new_groups: std::collections::HashMap<u64, Vec<(Vec<CellValue>, usize)>> =
        std::collections::HashMap::new();
    for row in new_rows {
        let h = row_hash(row);
        let bucket = new_groups.entry(h).or_default();
        if let Some(entry) = bucket.iter_mut().find(|(r, _)| rows_equal(r, row)) {
            entry.1 += 1;
        } else {
            bucket.push((row.clone(), 1));
        }
    }

    let mut added = Vec::new();
    let mut removed = Vec::new();
    let mut unchanged = Vec::new();

    // Process old groups.
    for bucket in old_groups.values() {
        for (row, old_count) in bucket {
            let new_count = new_groups
                .get(&row_hash(row))
                .and_then(|b| b.iter().find(|(r, _)| rows_equal(r, row)))
                .map(|(_, c)| *c)
                .unwrap_or(0);
            let common = (*old_count).min(new_count);
            let rem = *old_count - common;
            for _ in 0..common {
                unchanged.push(row.clone());
            }
            for _ in 0..rem {
                removed.push(row.clone());
            }
        }
    }

    // Process new groups for added rows.
    for bucket in new_groups.values() {
        for (row, new_count) in bucket {
            let old_count = old_groups
                .get(&row_hash(row))
                .and_then(|b| b.iter().find(|(r, _)| rows_equal(r, row)))
                .map(|(_, c)| *c)
                .unwrap_or(0);
            let extra = *new_count - old_count;
            for _ in 0..extra {
                added.push(row.clone());
            }
        }
    }

    ResultSetDiff {
        schema_mismatch: false,
        mismatch_detail: None,
        added,
        removed,
        unchanged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, data_type: &str) -> ResultColumn {
        ResultColumn {
            name: name.to_string(),
            data_type: data_type.to_string(),
            nullable: true,
        }
    }

    fn row(vals: &[CellValue]) -> Vec<CellValue> {
        vals.to_vec()
    }

    #[test]
    fn identical_sets_no_diff() {
        let cols = vec![col("id", "int")];
        let rows = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(2)])];
        let d = diff(&cols, &rows, &cols, &rows);
        assert!(!d.schema_mismatch);
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
        assert_eq!(d.unchanged.len(), 2);
        assert!(d.is_identical());
    }

    #[test]
    fn added_rows() {
        let cols = vec![col("id", "int")];
        let old = vec![row(&[CellValue::I64(1)])];
        let new = vec![
            row(&[CellValue::I64(1)]),
            row(&[CellValue::I64(2)]),
            row(&[CellValue::I64(3)]),
        ];
        let d = diff(&cols, &old, &cols, &new);
        assert!(!d.schema_mismatch);
        assert_eq!(d.added.len(), 2);
        assert!(d.removed.is_empty());
        assert_eq!(d.unchanged.len(), 1);
    }

    #[test]
    fn removed_rows() {
        let cols = vec![col("id", "int")];
        let old = vec![
            row(&[CellValue::I64(1)]),
            row(&[CellValue::I64(2)]),
            row(&[CellValue::I64(3)]),
        ];
        let new = vec![row(&[CellValue::I64(1)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 2);
        assert!(d.added.is_empty());
        assert_eq!(d.unchanged.len(), 1);
    }

    #[test]
    fn schema_mismatch_different_count() {
        let old_cols = vec![col("id", "int"), col("name", "varchar")];
        let new_cols = vec![col("id", "int")];
        let d = diff(&old_cols, &[], &new_cols, &[]);
        assert!(d.schema_mismatch);
        assert!(d.mismatch_detail.is_some());
        assert!(d.added.is_empty());
        assert!(d.removed.is_empty());
    }

    #[test]
    fn schema_mismatch_different_names() {
        let old_cols = vec![col("id", "int")];
        let new_cols = vec![col("count", "int")];
        let d = diff(&old_cols, &[], &new_cols, &[]);
        assert!(d.schema_mismatch);
    }

    #[test]
    fn schema_mismatch_different_types() {
        let old_cols = vec![col("id", "int")];
        let new_cols = vec![col("id", "varchar")];
        let d = diff(&old_cols, &[], &new_cols, &[]);
        assert!(d.schema_mismatch);
    }

    #[test]
    fn duplicate_rows_multiset_semantics() {
        let cols = vec![col("id", "int")];
        let old = vec![
            row(&[CellValue::I64(1)]),
            row(&[CellValue::I64(1)]),
            row(&[CellValue::I64(2)]),
        ];
        let new = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(3)])];
        let d = diff(&cols, &old, &cols, &new);
        // One "1" was removed, one "1" is unchanged, "2" was removed, "3" was added.
        assert_eq!(d.removed.len(), 2); // one "1" and one "2"
        assert_eq!(d.added.len(), 1); // "3"
        assert_eq!(d.unchanged.len(), 1); // one "1"
    }

    #[test]
    fn empty_sets() {
        let cols = vec![col("id", "int")];
        let d = diff(&cols, &[], &cols, &[]);
        assert!(d.is_identical());
        assert_eq!(d.changed_count(), 0);
    }

    #[test]
    fn summary_text_identical() {
        let cols = vec![col("id", "int")];
        let d = diff(&cols, &[], &cols, &[]);
        assert_eq!(d.summary(), "+0 added, -0 removed, 0 unchanged");
    }

    #[test]
    fn summary_text_schema_mismatch() {
        let d = diff(&[col("a", "int")], &[], &[col("b", "int")], &[]);
        assert_eq!(d.summary(), "Schema mismatch — cannot compare");
    }

    #[test]
    fn summary_text_with_changes() {
        let cols = vec![col("id", "int")];
        let old = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(2)])];
        let new = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(3)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.summary(), "+1 added, -1 removed, 1 unchanged");
    }

    #[test]
    fn changed_count_sums_added_and_removed() {
        let cols = vec![col("id", "int")];
        let old = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(2)])];
        let new = vec![row(&[CellValue::I64(3)]), row(&[CellValue::I64(4)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.changed_count(), 4);
    }

    #[test]
    fn optimized_diff_matches_linear_for_small_sets() {
        let cols = vec![col("id", "int")];
        let old = vec![row(&[CellValue::I64(1)]), row(&[CellValue::I64(2)])];
        let new = vec![row(&[CellValue::I64(2)]), row(&[CellValue::I64(3)])];
        let linear = diff(&cols, &old, &cols, &new);
        let optimized = diff_optimized(&cols, &old, &cols, &new);
        assert_eq!(linear, optimized);
    }

    #[test]
    fn optimized_diff_large_sets() {
        let cols = vec![col("id", "int")];
        let old: Vec<Vec<CellValue>> = (0..600).map(|i| row(&[CellValue::I64(i)])).collect();
        let new: Vec<Vec<CellValue>> = (100..700).map(|i| row(&[CellValue::I64(i)])).collect();
        let d = diff_optimized(&cols, &old, &cols, &new);
        // Rows 0-99 removed (100), rows 600-699 added (100), rows 100-599 unchanged (500).
        assert_eq!(d.removed.len(), 100);
        assert_eq!(d.added.len(), 100);
        assert_eq!(d.unchanged.len(), 500);
    }

    #[test]
    fn rows_with_null_values() {
        let cols = vec![col("id", "int"), col("name", "varchar")];
        let old = vec![row(&[CellValue::I64(1), CellValue::Null])];
        let new = vec![row(&[CellValue::I64(1), CellValue::Null])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical());
    }

    #[test]
    fn null_vs_value_is_different() {
        let cols = vec![col("id", "int"), col("name", "varchar")];
        let old = vec![row(&[CellValue::I64(1), CellValue::Null])];
        let new = vec![row(&[CellValue::I64(1), CellValue::String("x".into())])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn float_equality_exact() {
        let cols = vec![col("x", "float")];
        let old = vec![row(&[CellValue::F64(1.5)])];
        let new = vec![row(&[CellValue::F64(1.5)])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical());
    }

    #[test]
    fn float_nearby_values_are_different() {
        let cols = vec![col("x", "float")];
        let old = vec![row(&[CellValue::F64(1.5)])];
        let new = vec![row(&[CellValue::F64(1.5 + f64::EPSILON)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn float_negative_zero_equals_positive_zero() {
        let cols = vec![col("x", "float")];
        let old = vec![row(&[CellValue::F64(0.0)])];
        let new = vec![row(&[CellValue::F64(-0.0)])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical(), "IEEE equality treats +0.0 and -0.0 equal");
    }

    #[test]
    fn float_nan_equal_to_nan() {
        let cols = vec![col("x", "float")];
        let old = vec![row(&[CellValue::F64(f64::NAN)])];
        let new = vec![row(&[CellValue::F64(f64::NAN)])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical());
    }

    #[test]
    fn bytes_comparison() {
        let cols = vec![col("data", "varbinary")];
        let old = vec![row(&[CellValue::Bytes(vec![1, 2, 3])])];
        let new = vec![row(&[CellValue::Bytes(vec![1, 2, 3])])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical());
    }

    #[test]
    fn schemas_match_same() {
        let a = vec![col("id", "int"), col("name", "varchar")];
        let b = vec![col("id", "int"), col("name", "varchar")];
        assert!(schemas_match(&a, &b));
    }

    #[test]
    fn schemas_match_different_count() {
        let a = vec![col("id", "int")];
        let b = vec![col("id", "int"), col("name", "varchar")];
        assert!(!schemas_match(&a, &b));
    }

    #[test]
    fn schemas_match_different_type() {
        let a = vec![col("id", "int")];
        let b = vec![col("id", "bigint")];
        assert!(!schemas_match(&a, &b));
    }

    #[test]
    fn typed_i64_vs_string_one_is_difference() {
        let cols = vec![col("id", "int")];
        let old = vec![row(&[CellValue::I64(1)])];
        let new = vec![row(&[CellValue::String("1".into())])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1, "I64(1) is not String(\"1\")");
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn typed_i64_vs_f64_one_is_difference() {
        let cols = vec![col("x", "float")];
        let old = vec![row(&[CellValue::I64(1)])];
        let new = vec![row(&[CellValue::F64(1.0)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1, "I64(1) is not F64(1.0)");
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn typed_bool_true_vs_i64_one_is_difference() {
        let cols = vec![col("flag", "bit")];
        let old = vec![row(&[CellValue::Bool(true)])];
        let new = vec![row(&[CellValue::I64(1)])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1, "Bool(true) is not I64(1)");
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
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
        let result = compare_detailed(&cols, &old, &cols, &new, &[0, 1]).expect("valid keys");
        assert!(result.added.is_empty());
        assert!(result.removed.is_empty());
        assert_eq!(result.changed.len(), 1);
        assert_eq!(
            result.changed[0].key,
            vec![CellValue::I64(7), CellValue::I64(1)]
        );
        assert_eq!(result.changed[0].changes.len(), 2);
        assert_eq!(result.changed[0].changes[0].column_name, "name");
        assert_eq!(result.changed[0].changes[1].column_name, "active");
    }

    #[test]
    fn keyed_comparison_rejects_duplicate_and_null_keys() {
        let cols = vec![col("id", "int"), col("value", "text")];
        let duplicate = vec![
            row(&[CellValue::I64(1), CellValue::String("a".into())]),
            row(&[CellValue::I64(1), CellValue::String("b".into())]),
        ];
        let error = compare_detailed(&cols, &duplicate, &cols, &[], &[0]).unwrap_err();
        assert!(error.contains("duplicate key"));

        let null = vec![row(&[CellValue::Null, CellValue::String("a".into())])];
        let error = compare_detailed(&cols, &null, &cols, &[], &[0]).unwrap_err();
        assert!(error.contains("NULL key"));
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

    #[test]
    fn detailed_comparison_rejects_incompatible_schema_and_oversized_runs() {
        let cols = vec![col("id", "int")];
        let other = vec![col("id", "text")];
        assert!(
            compare_detailed(&cols, &[], &other, &[], &[])
                .unwrap_err()
                .contains("incompatible")
        );
        let rows = vec![row(&[CellValue::I64(1)]); DIFF_MAX_ROWS + 1];
        assert!(
            compare_detailed(&cols, &rows, &cols, &[], &[])
                .unwrap_err()
                .contains("limited")
        );
    }

    #[test]
    fn null_is_distinct_from_empty_string() {
        let cols = vec![col("name", "varchar")];
        let old = vec![row(&[CellValue::Null])];
        let new = vec![row(&[CellValue::String("".into())])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(
            !d.is_identical(),
            "Null and empty string should be different"
        );
    }

    #[test]
    fn null_equal_to_null() {
        let cols = vec![col("name", "varchar")];
        let old = vec![row(&[CellValue::Null])];
        let new = vec![row(&[CellValue::Null])];
        let d = diff(&cols, &old, &cols, &new);
        assert!(d.is_identical());
    }

    #[test]
    fn bytes_differing_content_is_difference() {
        let cols = vec![col("data", "varbinary")];
        let old = vec![row(&[CellValue::Bytes(vec![0xAB, 0xCD])])];
        let new = vec![row(&[CellValue::Bytes(vec![0xAB, 0xCE])])];
        let d = diff(&cols, &old, &cols, &new);
        assert_eq!(d.removed.len(), 1);
        assert_eq!(d.added.len(), 1);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn optimized_diff_typed_semantics_large() {
        let cols = vec![col("id", "int")];
        let old: Vec<Vec<CellValue>> = (0..600).map(|i| row(&[CellValue::I64(i)])).collect();
        let new: Vec<Vec<CellValue>> = (0..600)
            .map(|i| row(&[CellValue::String(i.to_string())]))
            .collect();
        let d = diff_optimized(&cols, &old, &cols, &new);
        // Every row changed variant, so nothing is unchanged.
        assert_eq!(d.removed.len(), 600);
        assert_eq!(d.added.len(), 600);
        assert!(d.unchanged.is_empty());
    }

    #[test]
    fn optimized_diff_nan_equal_across_hash_path() {
        // NaN rows must hash into the same bucket and compare equal on the
        // hash-grouped (large-set) path, not just the linear path.
        let cols = vec![col("id", "int"), col("x", "float")];
        let old: Vec<Vec<CellValue>> = (0..600)
            .map(|i| row(&[CellValue::I64(i), CellValue::F64(f64::NAN)]))
            .collect();
        let new = old.clone();
        let d = diff_optimized(&cols, &old, &cols, &new);
        assert!(d.is_identical(), "NaN must equal NaN on the hashed path");
    }
}
