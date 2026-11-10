use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_intel::intellisense::{
    AcceptHistory, CompletionCatalog, CompletionItem, CompletionObject, apply_rank_boosts,
    complete, default_snippets,
};
use std::hint::black_box;

/// A catalog shaped like a real production database: many schemas, wide
/// tables, FK-free (join candidates are exercised separately by unit tests).
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

fn run(c: &mut Criterion, name: &str, sql: &str, catalog: &CompletionCatalog) {
    // Cursor at the end of the text — the position a keystroke completes at.
    let cursor = sql.len();
    c.bench_function(name, |b| {
        b.iter(|| {
            let items = complete(black_box(sql), black_box(cursor), black_box(catalog));
            black_box(items.len())
        })
    });
}

fn bench_completion(c: &mut Criterion) {
    for &size in &[100usize, 1000] {
        let catalog = generated_catalog(size);

        // Table-name completion in FROM: matches against every object.
        run(
            c,
            &format!("complete_table_after_from_{size}_tables"),
            "SELECT * FROM Bench",
            &catalog,
        );

        // Column completion behind a resolved alias: hot per-keystroke path.
        run(
            c,
            &format!("complete_alias_column_{size}_tables"),
            "SELECT b.Colu FROM sales.BenchTable0001 b WHERE b.",
            &catalog,
        );

        // Bare keyword/snippet prefix at statement start.
        run(
            c,
            &format!("complete_keyword_prefix_{size}_tables"),
            "SEL",
            &catalog,
        );
    }

    let catalog = generated_catalog(2_000);
    let sql = "SELECT * FROM Budget";
    let mut items: Vec<CompletionItem> = complete(sql, sql.len(), &catalog)
        .into_iter()
        .map(|mut item| {
            item.score = 80;
            item
        })
        .collect();
    let mut history = AcceptHistory::default();
    for i in 0..512 {
        history.record(&format!("dbo.HistoryTable{i:04}"));
    }
    c.bench_function("apply_rank_boosts_2000_items_512_history", |b| {
        b.iter(|| {
            apply_rank_boosts(
                black_box(&mut items),
                black_box("BudgetTable0001"),
                black_box(Some(&history)),
            );
        });
    });
}

criterion_group!(benches, bench_completion);
criterion_main!(benches);
