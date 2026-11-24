//! Result set difference — compare two result sets to find added/removed rows.
//!
//! Port of `ResultSetDifference.swift`. The comparison itself lives in
//! `sqlly_services::analytics::resultset` (shared with notebook diff cells);
//! this module keeps the app's types with their localized summaries and
//! turns the core's typed errors into display text.

use sqlly_core::{CellValue, ResultColumn};
use sqlly_services::analytics::resultset::{self as core, DiffError, DiffInput};

pub use sqlly_services::analytics::resultset::{
    ChangedRow, ColumnChange, DIFF_MAX_ROWS, DIFF_MAX_RUNS,
};

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

impl From<core::DetailedDiff> for DetailedDiff {
    fn from(diff: core::DetailedDiff) -> Self {
        Self {
            added: diff.added,
            removed: diff.removed,
            changed: diff.changed,
            unchanged_count: diff.unchanged_count,
            key_columns: diff.key_columns,
        }
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
    core::compare_detailed(
        columns,
        baseline_rows,
        comparison_columns,
        comparison_rows,
        key_columns,
    )
    .map(DetailedDiff::from)
    .map_err(|error| error_text(&error))
}

fn error_text(error: &DiffError) -> String {
    match error {
        DiffError::IncompatibleColumns => {
            crate::localization::text("results.difference.incompatible_columns")
        }
        DiffError::RowLimit { maximum_rows } => crate::localization::format(
            "results.difference.row_limit",
            &[("maximum_rows", &maximum_rows.to_string())],
        ),
        DiffError::KeyColumnMissing { column_position } => crate::localization::format(
            "results.difference.key_column_missing",
            &[("column_position", &column_position.to_string())],
        ),
        DiffError::NullKey { input, row_number } => {
            let key = match input {
                DiffInput::Baseline => "results.difference.baseline_null_key",
                DiffInput::Comparison => "results.difference.comparison_null_key",
            };
            crate::localization::format(key, &[("row_number", &row_number.to_string())])
        }
        DiffError::DuplicateKey { input, row_number } => {
            let key = match input {
                DiffInput::Baseline => "results.difference.baseline_duplicate_key",
                DiffInput::Comparison => "results.difference.comparison_duplicate_key",
            };
            crate::localization::format(key, &[("row_number", &row_number.to_string())])
        }
    }
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
    let diff = core::diff(old_columns, old_rows, new_columns, new_rows);
    let mismatch_detail = diff.schema_mismatch.then(|| {
        let names = |columns: &[ResultColumn]| {
            columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        crate::localization::format(
            "results.difference.schema_mismatch_detail",
            &[
                ("old_count", &old_columns.len().to_string()),
                ("old_columns", &names(old_columns)),
                ("new_count", &new_columns.len().to_string()),
                ("new_columns", &names(new_columns)),
            ],
        )
    });
    ResultSetDiff {
        schema_mismatch: diff.schema_mismatch,
        mismatch_detail,
        added: diff.added,
        removed: diff.removed,
        unchanged: diff.unchanged,
    }
}

/// Hash-grouped diff for large row sets; the same answer as [`diff`].
pub fn diff_optimized(
    old_columns: &[ResultColumn],
    old_rows: &[Vec<CellValue>],
    new_columns: &[ResultColumn],
    new_rows: &[Vec<CellValue>],
) -> ResultSetDiff {
    diff(old_columns, old_rows, new_columns, new_rows)
}

#[cfg(test)]
mod tests {
    use sqlly_services::analytics::resultset::schemas_match;

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
