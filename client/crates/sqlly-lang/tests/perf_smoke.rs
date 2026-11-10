//! Performance guardrail (smoke) tests for tokenizing and model building at
//! realistic schema sizes. AGENTS.md requires the tokenizer path to stay
//! performant; the criterion bench measures it, but nothing failed CI when a
//! change made the scanner or model builder super-linear. These tests run the
//! raw scan and the model-building tokenizer against a production-shaped
//! schema (hundreds of objects) and assert both correctness at scale and a
//! deliberately loose wall-clock ceiling that only a catastrophic regression
//! can trip. See `sqlly-intellisense/tests/perf_smoke.rs` for the rationale.

use sqlly_lang::fsmodel::FileSourceRef;
use sqlly_lang::tokenizer::{SimpleSqlTokenizer, SqlTokenizer, tokenize};
use std::time::{Duration, Instant};

/// A schema with `object_count` tables (each with a matching procedure),
/// mirroring `benches/tokenizer_indexing.rs`.
fn generated_schema(object_count: usize) -> String {
    let mut sql = String::with_capacity(object_count * 240);
    for i in 0..object_count {
        sql.push_str(&format!(
            "CREATE TABLE dbo.BenchTable{i:04} (Id INT NOT NULL, Name NVARCHAR(80), CreatedAt DATETIME2, CONSTRAINT PK_BenchTable{i:04} PRIMARY KEY (Id));\n"
        ));
        sql.push_str(&format!(
            "CREATE PROCEDURE dbo.GetBenchTable{i:04} @Id INT AS SELECT Id, Name FROM dbo.BenchTable{i:04} WHERE Id = @Id;\n"
        ));
    }
    sql
}

#[test]
fn model_build_is_correct_at_scale() {
    let tables = 500;
    let sql = generated_schema(tables);
    let source = FileSourceRef {
        source_file_name: "perf.sql".into(),
        source_file_path: "/perf/perf.sql".into(),
        file_hash: "perf".into(),
    };
    let result = SimpleSqlTokenizer.tokenize_source(&sql, source);
    // Every table lands under the dbo schema; the model must not silently drop
    // objects as the input grows.
    let table_total: usize = result.snapshot.schemas.iter().map(|s| s.tables.len()).sum();
    let proc_total: usize = result
        .snapshot
        .schemas
        .iter()
        .map(|s| s.procedures.len())
        .sum();
    assert!(
        table_total >= tables,
        "model build lost tables at scale: found {table_total} for {tables} tables"
    );
    assert!(
        proc_total >= tables,
        "model build lost procedures at scale: found {proc_total} for {tables} procedures"
    );
}

#[test]
fn tokenize_and_model_stay_fast_at_scale() {
    let sql = generated_schema(500);
    let source = FileSourceRef {
        source_file_name: "perf.sql".into(),
        source_file_path: "/perf/perf.sql".into(),
        file_hash: "perf".into(),
    };

    // Raw scan throughput — cheap, so run a larger batch.
    let _ = tokenize(&sql);
    let scan_iterations = 20;
    let start = Instant::now();
    let mut tokens = 0usize;
    for _ in 0..scan_iterations {
        tokens += tokenize(&sql).len();
    }
    let scan_elapsed = start.elapsed();
    assert!(tokens > 0, "tokenizer produced no tokens at scale");

    // Model-building tokenizer (the snapshot path IntelliSense consumes). Far
    // heavier than the raw scan, so a smaller batch keeps the smoke test snappy
    // while still catching a super-linear regression.
    let model_iterations = 5;
    let start = Instant::now();
    for _ in 0..model_iterations {
        let _ = SimpleSqlTokenizer.tokenize_source(&sql, source.clone());
    }
    let model_elapsed = start.elapsed();

    // Deliberately loose ceilings — a 500-table schema tokenizes and builds in
    // a few milliseconds per pass; multi-second totals mean a super-linear
    // regression.
    let ceiling = Duration::from_secs(15);
    assert!(
        scan_elapsed < ceiling,
        "raw tokenize perf regression: {scan_iterations} scans of a 500-table schema took {scan_elapsed:?}"
    );
    assert!(
        model_elapsed < ceiling,
        "model-build perf regression: {model_iterations} builds of a 500-table schema took {model_elapsed:?}"
    );
    eprintln!(
        "perf-smoke: {scan_iterations}x raw tokenize in {scan_elapsed:?}, {model_iterations}x model build in {model_elapsed:?}"
    );
}
