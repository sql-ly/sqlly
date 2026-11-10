use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_lang::fsmodel::{FileIndex, FileSourceRef, FilesystemDatabaseProfile, SqlModelSnapshot};
use sqlly_lang::tokenizer::{FileSnapshotCache, SimpleSqlTokenizer, SqlTokenizer, index_profile};
use std::hint::black_box;
use std::path::{Path, PathBuf};

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

struct BenchDir {
    path: PathBuf,
}

impl BenchDir {
    fn new(tag: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "sqlly_bench_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for BenchDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

fn populate_index_fixture(root: &Path, file_count: usize) {
    for i in 0..file_count {
        std::fs::write(
            root.join(format!("bench_table_{i:04}.sql")),
            format!(
                "CREATE TABLE dbo.BenchTable{i:04} (Id INT NOT NULL, ParentId INT NULL, Name NVARCHAR(80), CONSTRAINT PK_BenchTable{i:04} PRIMARY KEY (Id));\n\
                 CREATE VIEW dbo.BenchView{i:04} AS SELECT Id, Name FROM dbo.BenchTable{i:04};\n"
            ),
        )
        .unwrap();
    }
}

/// Raw `tokenize` throughput in isolation — the dialect-invariant scan behind
/// safety classification, parameter detection, and paren diagnostics. The
/// model benches below are dominated by model building, so a keyword-lookup
/// or scanner regression only shows up here.
fn bench_raw_tokenize(c: &mut Criterion) {
    let sql = generated_schema(250);
    c.bench_function("raw_tokenize_250_tables", |b| {
        b.iter(|| black_box(sqlly_lang::tokenizer::tokenize(black_box(sql.as_str()))).len())
    });
}

fn bench_tokenize_model(c: &mut Criterion) {
    let sql = generated_schema(250);
    let source = FileSourceRef {
        source_file_name: "bench.sql".into(),
        source_file_path: "/bench/bench.sql".into(),
        file_hash: "bench".into(),
    };

    c.bench_function("simple_tokenize_model_250_tables", |b| {
        b.iter(|| {
            let result = SimpleSqlTokenizer
                .tokenize_source(black_box(sql.as_str()), black_box(source.clone()));
            black_box(result.snapshot.schemas.len())
        })
    });
}

fn bench_filesystem_indexing(c: &mut Criterion) {
    let fixture = BenchDir::new("indexing");
    populate_index_fixture(fixture.path(), 200);
    let profile = FilesystemDatabaseProfile::new(
        "bench-db",
        vec![fixture.path().to_string_lossy().into_owned()],
    );
    let previous = SqlModelSnapshot::default();
    let prior_index = FileIndex::new();
    let prior_snapshots = FileSnapshotCache::new();

    c.bench_function("index_profile_200_files", |b| {
        b.iter(|| {
            let outcome = index_profile(
                black_box(&profile),
                black_box(&previous),
                black_box(&prior_index),
                black_box(&prior_snapshots),
            );
            black_box(outcome.snapshot.schemas.len())
        })
    });
}

criterion_group!(
    benches,
    bench_raw_tokenize,
    bench_tokenize_model,
    bench_filesystem_indexing
);
criterion_main!(benches);
