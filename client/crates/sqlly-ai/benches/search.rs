//! Catalog search benchmarks: FTS retrieval + rerank, brute-force vector
//! search, and hybrid merge, at small/medium/large catalog sizes.
//!
//! Purpose: a measured baseline before any indexed/ANN vector backend is
//! considered — the roadmap rule is that brute force stays until these
//! numbers prove it inadequate at the supported catalog sizes.

use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_ai::catalog::{
    CatalogColumn, CatalogObject, CatalogObjectKind, CatalogSource, CatalogStore, SearchFilter,
    SourceKind,
};
use std::hint::black_box;

const DIMS: usize = 64;

fn object(i: usize) -> CatalogObject {
    let names = [
        "Customer",
        "Order",
        "OrderLine",
        "Invoice",
        "Payment",
        "Product",
        "Shipment",
        "Employee",
    ];
    let base = names[i % names.len()];
    CatalogObject {
        schema: Some("dbo".into()),
        name: format!("{base}Archive{i:05}"),
        kind: CatalogObjectKind::Table,
        columns: (0..8)
            .map(|c| CatalogColumn {
                name: format!("col_{c}"),
                type_name: Some("INT".into()),
                ordinal: c as i64,
                nullable: c % 2 == 0,
            })
            .collect(),
        primary_key: vec!["col_0".into()],
        foreign_keys: vec![],
        parameters: vec![],
        definition: None,
        source: CatalogSource {
            kind: SourceKind::Database,
            path: None,
            create_date: Some(1),
            modify_date: Some(2),
        },
    }
}

/// Deterministic pseudo-embedding derived from the index (no RNG: benches
/// must be reproducible run to run).
fn embedding(i: usize) -> Vec<f32> {
    (0..DIMS)
        .map(|d| (((i * 31 + d * 17) % 97) as f32) / 97.0)
        .collect()
}

fn store_with(objects: usize) -> CatalogStore {
    let store = CatalogStore::open_in_memory().expect("in-memory store");
    let all: Vec<CatalogObject> = (0..objects).map(object).collect();
    store.sync(&all).expect("sync");
    for (i, obj) in all.iter().enumerate() {
        let key = format!("dbo.{}", obj.name);
        store
            .upsert_embedding(&key, "hash", "bench-provider", &embedding(i))
            .expect("embedding");
    }
    store
}

fn bench_search(c: &mut Criterion) {
    for size in [100usize, 1_000, 5_000] {
        let store = store_with(size);
        let query_vec = embedding(size / 2);

        c.bench_function(&format!("fts_search_{size}"), |b| {
            b.iter(|| {
                let hits = sqlly_ai::catalog::search_store_objects(
                    black_box(&store),
                    black_box("customer order archive"),
                    10,
                    SearchFilter::default(),
                )
                .expect("search");
                black_box(hits.len())
            })
        });

        c.bench_function(&format!("vector_search_{size}"), |b| {
            b.iter(|| {
                let hits = sqlly_ai::catalog::vector_search(
                    black_box(&store),
                    black_box(&query_vec),
                    10,
                    "bench-provider",
                )
                .expect("vector search");
                black_box(hits.len())
            })
        });

        let fts_hits = sqlly_ai::catalog::search_store_objects(
            &store,
            "customer order archive",
            20,
            SearchFilter::default(),
        )
        .expect("fts candidates");
        let vector_hits =
            sqlly_ai::catalog::vector_search(&store, &query_vec, 20, "bench-provider")
                .expect("vec");
        c.bench_function(&format!("hybrid_merge_{size}"), |b| {
            b.iter(|| {
                let merged = sqlly_ai::catalog::merge_hybrid(
                    black_box(&fts_hits),
                    black_box(&vector_hits),
                    &sqlly_ai::catalog::HybridSearchOptions::default(),
                );
                black_box(merged.len())
            })
        });
    }
}

criterion_group!(benches, bench_search);
criterion_main!(benches);
