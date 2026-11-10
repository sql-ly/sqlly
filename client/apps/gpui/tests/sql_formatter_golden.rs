//! Golden tests for `format_sql` (shared sqlly-sqlformat engine).

use std::fs;
use std::path::PathBuf;

use sqlly_gpui::sql_formatter::{SqlFormattingPreferences, format_sql};
use sqlly_testkit::GoldenTest;

fn formatter_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/formatter")
}

fn assert_formatter_golden(case: &str) {
    let input_path = formatter_dir().join(format!("{case}.input.sql"));
    let golden_path = formatter_dir().join(format!("{case}.golden.sql"));
    let input = fs::read_to_string(&input_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", input_path.display()));
    let prefs = SqlFormattingPreferences::default();
    let formatted = format_sql(&input, &prefs, None);
    let ok = GoldenTest::compare(&formatted, &golden_path)
        .unwrap_or_else(|e| panic!("golden {}: {e}", golden_path.display()));
    assert!(
        ok,
        "formatter golden mismatch for {case} — re-run with SQLLY_UPDATE_GOLDEN=1"
    );
}

#[test]
fn formatter_golden_suite() {
    for case in [
        "simple_select",
        "multi_join",
        "insert",
        "update",
        "delete",
        "cte",
        "batch_go",
        "malformed",
    ] {
        assert_formatter_golden(case);
    }
}

#[test]
fn formatter_output_is_deterministic() {
    let input_path = formatter_dir().join("simple_select.input.sql");
    let input = fs::read_to_string(&input_path).unwrap();
    let prefs = SqlFormattingPreferences::default();
    let first = format_sql(&input, &prefs, None);
    let second = format_sql(&first, &prefs, None);
    assert_eq!(first, second);
}
