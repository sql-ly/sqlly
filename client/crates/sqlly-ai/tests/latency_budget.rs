//! Coarse latency guard for catalog FTS search, mirroring the tokenizer /
//! sqlparse / intellisense budget tests: a generous order-of-magnitude
//! ceiling under plain `cargo test`, deliberately NOT a criterion gate
//! (exact numbers live in `cargo bench`).

use sqlly_ai::catalog::{
    CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, CatalogStore, SearchFilter,
    SourceKind,
};
use std::time::Instant;

fn object(i: usize) -> CatalogObject {
    let schemas = ["dbo", "sales", "hr", "audit"];
    CatalogObject {
        schema: Some(schemas[i % schemas.len()].to_string()),
        name: format!("BudgetTable{i:04}"),
        kind: CatalogObjectKind::Table,
        columns: (0..12)
            .map(|c| CatalogColumn {
                name: format!("Column{c:02}Name"),
                type_name: Some("int".to_string()),
                ordinal: c as i64,
                nullable: true,
            })
            .collect(),
        primary_key: vec!["Column00Name".to_string()],
        foreign_keys: Vec::new(),
        parameters: Vec::new(),
        definition: None,
        source: CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1),
            modify_date: Some(2),
        },
    }
}

#[test]
fn catalog_search_stays_within_latency_budget() {
    let store = CatalogStore::open_in_memory().unwrap();
    let objects: Vec<CatalogObject> = (0..2_000).map(object).collect();

    let sync_started = Instant::now();
    store.sync(&objects).unwrap();
    let sync_elapsed = sync_started.elapsed();
    assert!(
        sync_elapsed.as_millis() < 30_000,
        "syncing 2,000 objects took {sync_elapsed:?}; the sync path regressed by an order of magnitude"
    );

    let search_started = Instant::now();
    for query in ["budget", "table17", "column05", "sales", "nonexistent_zzz"] {
        let hits = store
            .search_schema(query, 25, SearchFilter::default())
            .unwrap();
        let _ = hits;
    }
    let search_elapsed = search_started.elapsed();
    assert!(
        search_elapsed.as_millis() < 5_000,
        "five FTS searches over 2,000 objects took {search_elapsed:?}; search regressed by an order of magnitude"
    );
}
