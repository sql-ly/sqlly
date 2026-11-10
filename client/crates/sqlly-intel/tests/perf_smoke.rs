//! Performance guardrail (smoke) tests for completion at realistic schema
//! sizes. AGENTS.md requires the tokenizer and IntelliSense paths to stay
//! performant; the criterion benches measure it, but nothing failed CI when a
//! change made completion super-linear. These tests run the hot completion
//! paths against production-shaped catalogs (hundreds to thousands of tables)
//! and assert two things:
//!
//! 1. **Correctness at scale** — completion still returns the right objects
//!    when the catalog is large. This is the primary, non-flaky guardrail: an
//!    accidental O(n^2)/O(n^3) rewrite that hangs will trip the harness
//!    timeout, and a filter that silently drops matches at scale fails the
//!    assertions.
//! 2. **A deliberately loose wall-clock ceiling** — a catastrophic-regression
//!    backstop. The ceilings are set far above the real runtime (which is a
//!    few milliseconds), so ordinary CI jitter can never trip them; only a
//!    genuine algorithmic blow-up will. The measured time is printed so a
//!    slow-creep regression is visible in CI logs before it ever fails.

use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject, complete, default_snippets};
use std::time::Instant;

/// A catalog shaped like a real production database: several schemas, wide
/// tables. Mirrors the shape used by `benches/completion.rs` so the smoke
/// test and the bench exercise the same code at the same scale.
fn generated_catalog(table_count: usize) -> CompletionCatalog {
    let schemas = ["dbo", "sales", "hr", "audit"];
    let objects = (0..table_count)
        .map(|i| {
            let columns = (0..12).map(|c| format!("Column{c:02}Name")).collect();
            CompletionObject::table(
                schemas[i % schemas.len()],
                format!("BenchTable{i:04}"),
                columns,
            )
        })
        .collect();
    CompletionCatalog {
        databases: vec!["BenchDb".into()],
        schemas: schemas.iter().map(|s| s.to_string()).collect(),
        objects,
        filesystem_schemas: Vec::new(),
        snippets: default_snippets(),
    }
}

#[test]
fn completion_is_correct_at_scale() {
    let catalog = generated_catalog(2000);

    // Table-name completion in FROM: matches against every object.
    let sql = "SELECT * FROM BenchTable0001";
    let items = complete(sql, sql.len(), &catalog);
    assert!(
        items.iter().any(|i| i.label.contains("BenchTable0001")),
        "table completion must surface the matching table even in a 2000-table catalog"
    );

    // Column completion behind a resolved alias — the hot per-keystroke path.
    let sql = "SELECT b.Colu FROM sales.BenchTable0001 b WHERE b.Colu";
    let items = complete(sql, sql.len(), &catalog);
    assert!(
        items.iter().any(|i| i.label.contains("Column")),
        "alias column completion must resolve columns at scale"
    );
}

#[test]
fn completion_stays_fast_at_scale() {
    // 2000 tables is well beyond a typical working catalog; the hot alias
    // column path should still complete in microseconds-to-low-milliseconds.
    let catalog = generated_catalog(2000);
    let sql = "SELECT b.Colu FROM sales.BenchTable0001 b WHERE b.";
    let cursor = sql.len();

    // Warm any lazy state, then time a batch so the measurement isn't a single
    // noisy sample.
    let _ = complete(sql, cursor, &catalog);
    let iterations = 200;
    let start = Instant::now();
    let mut total = 0usize;
    for _ in 0..iterations {
        total += complete(sql, cursor, &catalog).len();
    }
    let elapsed = start.elapsed();
    assert!(total > 0, "completion returned nothing at scale");

    // Deliberately loose: 200 completions over a 2000-table catalog take a
    // small fraction of a second in practice. A multi-second result means the
    // completion path went super-linear.
    let ceiling = std::time::Duration::from_secs(10);
    assert!(
        elapsed < ceiling,
        "completion perf regression: {iterations} completions over a 2000-table \
         catalog took {elapsed:?} (ceiling {ceiling:?})"
    );
    eprintln!(
        "perf-smoke: {iterations} alias-column completions over 2000 tables in {elapsed:?} \
         ({:.1} us/call)",
        elapsed.as_secs_f64() * 1e6 / iterations as f64
    );
}

#[test]
fn value_templates_and_fk_details_are_cheap_at_scale() {
    use sqlly_intel::intellisense::{ValueKind, comparison_value_items};

    let start = Instant::now();
    for _ in 0..10_000 {
        let items = comparison_value_items(
            ValueKind::Unknown,
            false,
            false,
            Some((0, 0)),
            Some((0, 0)),
            false,
        );
        assert_eq!(items.len(), 5);
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2_000,
        "10000x value template generation took {elapsed:?}, over the 2000ms budget"
    );
    eprintln!(
        "perf-smoke: 10000 value-template generations in {elapsed:?} ({:.1} us/call)",
        elapsed.as_secs_f64() * 1e6 / 10_000.0
    );
}
