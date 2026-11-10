//! Golden tests for dialect-aware statement splitting in the editor.

use std::fs;
use std::path::PathBuf;

use sqlly_gpui::editor::statements::statement_spans_for;
use sqlly_lang::sqlmodel::Dialect;
use sqlly_testkit::GoldenTest;

fn statements_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/statements")
}

fn parse_case(filename: &str) -> (&str, Dialect) {
    let stem = filename.strip_suffix(".input.sql").unwrap_or(filename);
    let (case, dialect_tag) = stem.rsplit_once('.').expect("case.dialect.input.sql");
    let dialect = match dialect_tag {
        "sqlserver" => Dialect::SqlServer,
        "postgres" => Dialect::Postgres,
        "mysql" => Dialect::MySql,
        "sqlite" => Dialect::Sqlite,
        other => panic!("unknown dialect tag: {other}"),
    };
    (case, dialect)
}

fn render_spans(sql: &str, dialect: Dialect) -> String {
    let spans = statement_spans_for(sql, dialect);
    let mut out = String::new();
    for (i, span) in spans.iter().enumerate() {
        out.push_str(&format!(
            "--- statement {} [{}..{}]\n",
            i + 1,
            span.start,
            span.end
        ));
        out.push_str(&sql[span.start..span.end]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn assert_statement_golden(filename: &str) {
    let input_path = statements_dir().join(filename);
    let (case, dialect) = parse_case(filename);
    let dialect_name = match dialect {
        Dialect::SqlServer => "sqlserver",
        Dialect::Postgres => "postgres",
        Dialect::MySql => "mysql",
        Dialect::Oracle => "oracle",
        Dialect::Sqlite => "sqlite",
    };
    let golden_path = statements_dir().join(format!("{case}.{dialect_name}.golden.txt"));
    let input = fs::read_to_string(&input_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", input_path.display()));
    let actual = render_spans(&input, dialect);
    let ok = GoldenTest::compare(&actual, &golden_path)
        .unwrap_or_else(|e| panic!("golden {}: {e}", golden_path.display()));
    assert!(
        ok,
        "statement golden mismatch for {filename} — re-run with SQLLY_UPDATE_GOLDEN=1"
    );
}

#[test]
fn statement_split_golden_suite() {
    let dir = statements_dir();
    let mut files: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".input.sql"))
        .collect();
    files.sort();
    assert_eq!(
        files.len(),
        9,
        "expected 9 golden input files, got {files:?}"
    );
    for filename in files {
        assert_statement_golden(&filename);
    }
}
