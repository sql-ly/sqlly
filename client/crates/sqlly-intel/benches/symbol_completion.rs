use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_intel::modelengine::intellisense::{CompletionMode, CompletionRequest, complete_symbols};
use sqlly_lang::sqlmodel::{
    Dialect, Fidelity, IncompleteRegions, ModelSnapshot, ObjectBody, ObjectFragment, ObjectKind,
    ObjectRef, Provenance, SqlName, TableBody,
};
use std::collections::BTreeSet;
use std::hint::black_box;

fn name(text: impl Into<String>) -> SqlName {
    SqlName::new(text, Dialect::SqlServer, false)
}

fn table_fragment(index: usize) -> ObjectFragment {
    ObjectFragment {
        reference: ObjectRef::new(
            None,
            Some(name("dbo")),
            name(format!("SalesOrderLineArchive{index:04}")),
        ),
        kind: ObjectKind::Table,
        dialect: Dialect::SqlServer,
        body: ObjectBody::Table(TableBody::default()),
        provenance: Provenance::Database {
            profile: "bench".into(),
        },
        fidelity: Fidelity::Exact,
        references: Vec::new(),
    }
}

fn snapshot(object_count: usize) -> ModelSnapshot {
    let fragments = (0..object_count).map(table_fragment).collect();
    let (snapshot, _) =
        ModelSnapshot::empty().apply(1, fragments, &BTreeSet::new(), IncompleteRegions::default());
    snapshot
}

fn bench_symbol_completion(c: &mut Criterion) {
    let snapshot = snapshot(2_000);
    let text = "SELECT * FROM Sales".to_string();
    let request = CompletionRequest {
        cursor: text.len(),
        text,
        dialect: Dialect::SqlServer,
        default_catalog: None,
        default_namespace: Some(name("dbo")),
        mode: CompletionMode::Symbol,
    };

    c.bench_function("complete_symbols_2000_tables", |b| {
        b.iter(|| {
            let completions = complete_symbols(black_box(&request), black_box(&snapshot));
            black_box((completions.items.len(), completions.truncated))
        })
    });
}

criterion_group!(benches, bench_symbol_completion);
criterion_main!(benches);
