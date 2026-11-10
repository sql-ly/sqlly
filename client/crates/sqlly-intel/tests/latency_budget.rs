//! Coarse latency guard for the per-keystroke completion path, mirroring the
//! tokenizer's budget test: a generous order-of-magnitude ceiling under plain
//! `cargo test`, deliberately NOT a criterion gate (exact numbers live in
//! `cargo bench`).

use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject, complete, default_snippets};
use std::time::Instant;

fn generated_catalog(table_count: usize) -> CompletionCatalog {
    let schemas = ["dbo", "sales", "hr", "audit"];
    let objects = (0..table_count)
        .map(|i| {
            let columns = (0..12).map(|c| format!("Column{c:02}Name")).collect();
            CompletionObject::table(
                schemas[i % schemas.len()],
                format!("BudgetTable{i:04}"),
                columns,
            )
        })
        .collect();
    CompletionCatalog {
        databases: vec!["BudgetDb".into()],
        schemas: schemas.iter().map(|s| s.to_string()).collect(),
        objects,
        filesystem_schemas: Vec::new(),
        snippets: default_snippets(),
    }
}

#[test]
fn completion_stays_within_latency_budget() {
    let catalog = generated_catalog(2_000);
    // The two hot per-keystroke shapes: table completion scanning every
    // object, and alias-resolved column completion.
    let cases = [
        "SELECT * FROM Budget",
        "SELECT b.Colu FROM sales.BudgetTable0001 b WHERE b.",
    ];
    for sql in cases {
        let start = Instant::now();
        let mut total = 0usize;
        for _ in 0..10 {
            total += complete(sql, sql.len(), &catalog).len();
        }
        let elapsed = start.elapsed();
        assert!(total > 0, "{sql:?} must produce completions");
        // Release-mode completion is sub-millisecond per call; ten debug-mode
        // calls in 2s only trips on an order-of-magnitude regression.
        assert!(
            elapsed.as_millis() < 2_000,
            "10x completion for {sql:?} took {elapsed:?}, over the 2000ms budget"
        );
    }
}

#[test]
fn rank_boosts_stay_within_latency_budget() {
    use sqlly_intel::intellisense::{AcceptHistory, apply_rank_boosts};

    let catalog = generated_catalog(2_000);
    let sql = "SELECT * FROM Budget";
    let mut items = complete(sql, sql.len(), &catalog);
    let mut history = AcceptHistory::default();
    for i in 0..512 {
        history.record(&format!("dbo.HistoryTable{i:04}"));
    }
    let start = Instant::now();
    for _ in 0..1_000 {
        apply_rank_boosts(&mut items, "BudgetTable0001", Some(&history));
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 2_000,
        "1000x rank boosts took {elapsed:?}, over the 2000ms budget"
    );
}
