use criterion::{Criterion, criterion_group, criterion_main};
use sqlly_lang::sqllex::StrictLexer;
use sqlly_lang::sqlmodel::Dialect;
use std::hint::black_box;

fn generated_script(statement_count: usize) -> String {
    let mut sql = String::with_capacity(statement_count * 220);
    for i in 0..statement_count {
        sql.push_str(&format!(
            "CREATE TABLE dbo.ParseTable{i:04} (Id INT NOT NULL PRIMARY KEY, ParentId INT REFERENCES dbo.ParseTable{i:04}(Id), Name NVARCHAR(100));\n"
        ));
        sql.push_str(&format!(
            "SELECT p.Id, p.Name FROM dbo.ParseTable{i:04} p WHERE p.Name LIKE 'value%';\n"
        ));
    }
    sql
}

fn bench_strict_lexer(c: &mut Criterion) {
    let sql = generated_script(250);

    c.bench_function("strict_lexer_sqlserver_500_statements", |b| {
        b.iter(|| {
            let tokens = StrictLexer::tokenize(black_box(sql.as_str()), Dialect::SqlServer);
            black_box(tokens.len())
        })
    });
}

fn bench_sql_parser(c: &mut Criterion) {
    let sql = generated_script(150);

    c.bench_function("parse_sqlserver_300_statements", |b| {
        b.iter(|| {
            let tree = sqlly_lang::sqlparse::parse(black_box(sql.as_str()), Dialect::SqlServer);
            black_box((tree.statements.len(), tree.references.len()))
        })
    });
}

criterion_group!(benches, bench_strict_lexer, bench_sql_parser);
criterion_main!(benches);
