//! End-to-end engine tests (ADR-011): parallel collection from filesystem +
//! live SQLite, file-wins merge with shadows, ad-hoc query validation with
//! classified issues and slot typing, fixed/edit modes, targeted cache
//! invalidation, and IntelliSense symbol + value modes with FK pivot.

use sqlly_intel::modelengine::sqlite_driver::{SqliteCatalogCollector, SqliteDriver};
use sqlly_intel::modelengine::validate::{EngineMode, Exemption, ExemptionSet, ValidationScope};
use sqlly_intel::modelengine::{
    CompletionMode, CompletionRequest, EngineConcurrency, EngineConfig, IssueClass, ModelEngine,
    Notify, QueryValidationRequest, Severity, SourceConfig, ValueCompletionParams,
};
use sqlly_lang::sqlmodel::{Dialect, SqlName};
use std::num::NonZeroUsize;
use std::sync::Arc;

fn write(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write fixture");
    path
}

fn engine_with(concurrency: EngineConcurrency) -> Arc<ModelEngine> {
    ModelEngine::new(EngineConfig {
        concurrency,
        default_catalog: None,
        default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
        ..Default::default()
    })
}

fn default_engine() -> Arc<ModelEngine> {
    engine_with(EngineConcurrency::default())
}

fn sqlite_fixture(dir: &std::path::Path) -> String {
    let db_path = dir.join("fixture.db").display().to_string();
    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    conn.execute_batch(
        r#"
        CREATE TABLE Model (
            Id INTEGER NOT NULL PRIMARY KEY,
            Name TEXT NOT NULL
        );
        CREATE TABLE Car (
            Id INTEGER NOT NULL PRIMARY KEY,
            Plate TEXT NOT NULL,
            ModelId INTEGER NOT NULL REFERENCES Model(Id)
        );
        CREATE TABLE Person (
            Id INTEGER NOT NULL PRIMARY KEY,
            Name TEXT NOT NULL,
            Age INTEGER NULL
        );
        INSERT INTO Model (Id, Name) VALUES (1, 'Falcon'), (2, 'Aardvark'), (3, 'Macchiato');
        INSERT INTO Car (Id, Plate, ModelId) VALUES (10, 'ABC123', 1), (11, 'XYZ789', 2);
        INSERT INTO Person (Id, Name, Age) VALUES
            (1, 'Aaron', 31), (2, 'Aaliyah', 22), (3, 'Beatrix', 45), (4, 'Caleb', 28);
        "#,
    )
    .expect("seed sqlite");
    db_path
}

fn add_sqlite_source(engine: &Arc<ModelEngine>, db_path: &str) {
    engine.add_source(SourceConfig::Database {
        profile: "fixture".into(),
        dialect: Dialect::Sqlite,
        driver: Arc::new(SqliteDriver {
            path: db_path.to_string(),
        }),
        collector: Arc::new(SqliteCatalogCollector),
    });
}

// ---------------------------------------------------------------------------
// Collection + merge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn filesystem_collection_builds_model() {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "person.sql",
        "CREATE TABLE dbo.Person (Id int NOT NULL PRIMARY KEY, Name nvarchar(100) NOT NULL)",
    );
    write(
        dir.path(),
        "proc.sql",
        "CREATE PROCEDURE dbo.GetPerson @Id int AS SELECT 1",
    );

    let engine = default_engine();
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    let delta = engine.rebuild().await;
    assert_eq!(delta.added.len(), 2, "{delta:?}");
    assert_eq!(engine.snapshot().len(), 2);
    assert!(engine.collection_errors().is_empty());
}

#[tokio::test]
async fn live_sqlite_collection_builds_model() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);

    let delta = engine.rebuild().await;
    assert_eq!(delta.added.len(), 3, "{delta:?}");
    let snapshot = engine.snapshot();
    let car = snapshot.resolve(
        &sqlly_lang::sqlmodel::ObjectRef::bare(SqlName::new("Car", Dialect::Sqlite, false)),
        None,
        None,
    );
    assert_eq!(car.len(), 1);
    let table = car[0].table().expect("car is a table");
    assert_eq!(table.columns.len(), 3);
    // FK extracted from the live CREATE statement.
    assert_eq!(table.foreign_keys.len(), 1);
    assert_eq!(table.foreign_keys[0].target.name.spelling(), "Model");
}

#[tokio::test]
async fn file_wins_over_database_with_shadow_and_drift() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let fs_dir = tempfile::tempdir().unwrap();
    // Same table, different shape: file adds a column.
    write(
        fs_dir.path(),
        "person.sql",
        "CREATE TABLE Person (Id INTEGER NOT NULL PRIMARY KEY, Name TEXT NOT NULL, Age INTEGER NULL, Email TEXT NULL)",
    );

    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.add_source(SourceConfig::Filesystem {
        root: fs_dir.path().to_path_buf(),
        dialect: Dialect::Sqlite,
    });
    let delta = engine.rebuild().await;

    let snapshot = engine.snapshot();
    let person = snapshot
        .resolve(
            &sqlly_lang::sqlmodel::ObjectRef::bare(SqlName::new("person", Dialect::Sqlite, false)),
            None,
            None,
        )
        .into_iter()
        .find(|e| e.kind == sqlly_lang::sqlmodel::ObjectKind::Table)
        .expect("person resolved");
    // File definition (4 columns) wins; database (3 columns) shadows.
    assert!(
        person.provenance.is_file(),
        "file must win: {:?}",
        person.provenance
    );
    assert_eq!(person.table().unwrap().columns.len(), 4);
    let shadow = person.shadow.as_ref().expect("database version shadows");
    assert_eq!(shadow.table().unwrap().columns.len(), 3);
    // Drift reports the conflict.
    assert_eq!(engine.drift().len(), 1);
    assert!(
        delta
            .conflicts
            .iter()
            .any(|c| c.reference.name.spelling() == "Person")
    );
}

#[tokio::test]
async fn incremental_file_change_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "t.sql", "CREATE TABLE dbo.T (A int)");
    let engine = default_engine();
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    engine.rebuild().await;
    assert_eq!(engine.snapshot().len(), 1);

    // Change: add a column.
    std::fs::write(&path, "CREATE TABLE dbo.T (A int, B int)").unwrap();
    let delta = engine
        .notify(Notify::FileChanged { path: path.clone() })
        .await;
    assert_eq!(delta.changed.len(), 1, "{delta:?}");
    let snapshot = engine.snapshot();
    let t = snapshot.resolve(
        &sqlly_lang::sqlmodel::ObjectRef::bare(SqlName::new("T", Dialect::SqlServer, false)),
        None,
        None,
    )[0]
    .clone();
    assert_eq!(t.table().unwrap().columns.len(), 2);

    // Remove: object disappears.
    let delta = engine.notify(Notify::FileRemoved { path }).await;
    assert_eq!(delta.removed.len(), 1);
    assert_eq!(engine.snapshot().len(), 0);
}

#[tokio::test]
async fn fs_collector_respects_thread_cap_and_still_collects_all() {
    let dir = tempfile::tempdir().unwrap();
    for i in 0..40 {
        write(
            dir.path(),
            &format!("t{i:02}.sql"),
            &format!("CREATE TABLE dbo.T{i:02} (Id int NOT NULL)"),
        );
    }
    let engine = engine_with(EngineConcurrency {
        db_concurrency: NonZeroUsize::new(2).unwrap(),
        fs_read_threads: NonZeroUsize::new(2).unwrap(),
    });
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    let delta = engine.rebuild().await;
    assert_eq!(delta.added.len(), 40);
}

// ---------------------------------------------------------------------------
// Query validation (§5.4) + issue classification (§5.5)
// ---------------------------------------------------------------------------

async fn validation_fixture() -> Arc<ModelEngine> {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "schema.sql",
        r#"
CREATE TABLE dbo.Person (
  Id int NOT NULL PRIMARY KEY,
  Name nvarchar(100) NOT NULL,
  Age int NULL,
  AccountId uniqueidentifier NULL
);
CREATE TABLE dbo.Model (
  Id int NOT NULL PRIMARY KEY,
  Name nvarchar(50) NOT NULL
);
CREATE TABLE dbo.Car (
  Id int NOT NULL PRIMARY KEY,
  Plate nvarchar(20) NOT NULL,
  ModelId int NOT NULL REFERENCES dbo.Model(Id)
);
CREATE PROCEDURE dbo.ImportAccounts @BatchId int, @Label nvarchar(50) AS SELECT 1
"#,
    );
    let engine = default_engine();
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    engine.rebuild().await;
    // Keep the tempdir alive for the duration via leak (test-only).
    std::mem::forget(dir);
    engine
}

fn model_request(text: &str) -> QueryValidationRequest {
    QueryValidationRequest {
        text: text.into(),
        dialect: Dialect::SqlServer,
        default_catalog: None,
        default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
        scope: ValidationScope::Model,
    }
}

#[tokio::test]
async fn valid_query_passes() {
    let engine = validation_fixture().await;
    let report =
        engine.validate_query(model_request("select * from dbo.Person p where p.Age > 21"));
    assert!(report.valid, "{:#?}", report.issues);
    assert!(report.issues.is_empty());
    // Resolution info returned for tooling.
    assert!(report.resolved.iter().any(|r| r.resolved_to.is_some()));
}

#[tokio::test]
async fn unresolved_table_is_classified_with_span() {
    let engine = validation_fixture().await;
    let text = "select * from dbo.Persn where Age > 21";
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::UnresolvedObject)
        .expect("unresolved object issue");
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(issue.code, "SQM2001");
    // Span points at the table name.
    assert_eq!(&text[issue.span.start..issue.span.end], "dbo.Persn");
}

#[tokio::test]
async fn undefined_alias_qualifier_is_flagged() {
    // `t` is never introduced as a table source / alias, so `t.test` references
    // nothing. The defined alias `m` (here `p`) must NOT be flagged.
    let engine = validation_fixture().await;
    let text = "select * from dbo.Person p inner join dbo.Car c on t.test = p.Id where p.Age > 21";
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid, "should be invalid: {:#?}", report.issues);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::UnresolvedColumn)
        .expect("unresolved column / undefined alias issue");
    assert_eq!(issue.severity, Severity::Error);
    assert_eq!(issue.code, "SQM2004");
    // Span underlines the undefined qualifier `t`.
    assert_eq!(&text[issue.span.start..issue.span.end], "t");
    // Valid qualifiers (p, c) are not flagged.
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::UnresolvedColumn
                && (&text[i.span.start..i.span.end] == "p"
                    || &text[i.span.start..i.span.end] == "c")),
        "valid aliases must not be flagged: {:#?}",
        report.issues
    );
}

#[tokio::test]
async fn defined_aliases_are_not_flagged() {
    let engine = validation_fixture().await;
    let text = "select p.Id, c.Plate from dbo.Person p inner join dbo.Car c on c.Id = p.Id";
    let report = engine.validate_query(model_request(text));
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::UnresolvedColumn),
        "no qualifier should be flagged: {:#?}",
        report.issues
    );
}

#[tokio::test]
async fn schema_qualifier_is_not_flagged_as_undefined_alias() {
    // `dbo` is a schema, not a table alias. A schema-qualified reference caught
    // by the two-part qualifier scan must not be reported as an undefined alias.
    let engine = validation_fixture().await;
    let text = "select * from dbo.Person p where p.Id = dbo.Model";
    let report = engine.validate_query(model_request(text));
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::UnresolvedColumn),
        "schema qualifier `dbo` must not be flagged: {:#?}",
        report.issues
    );
}

#[tokio::test]
async fn undefined_alias_flagged_in_join_on_clause_user_repro() {
    // Mirrors the reported case: an `INNER JOIN ... ON t.test = m.MovementId`
    // where `t` is never aliased. `t.test` is fully invalid and must go red;
    // the defined aliases (`p`, `c` here) must stay clean. A second statement
    // with no alias is also present, as in the user's script.
    let engine = validation_fixture().await;
    let text = "SELECT *\nFROM dbo.Person p\n  INNER JOIN dbo.Car c\n    ON t.test = p.Id\nWHERE p.Age = 21\n\nSELECT * FROM dbo.Model where Name = 'OT-0004'";
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid, "should be invalid: {:#?}", report.issues);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::UnresolvedColumn)
        .expect("undefined alias `t` issue");
    assert_eq!(&text[issue.span.start..issue.span.end], "t");
    // Exactly one undefined-qualifier diagnostic (only `t`).
    assert_eq!(
        report
            .issues
            .iter()
            .filter(|i| i.class == IssueClass::UnresolvedColumn)
            .count(),
        1,
        "only `t` should be flagged: {:#?}",
        report.issues
    );
}

#[tokio::test]
async fn slot_type_mismatch_string_into_int_param() {
    // The slot-type-validation.md canonical case:
    // EXEC dbo.ImportAccounts @BatchId = 'abc' where @BatchId is int.
    let engine = validation_fixture().await;
    let text = "EXEC dbo.ImportAccounts @BatchId = 'abc'";
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::TypeMismatch)
        .expect("type mismatch: {report:?}");
    assert_eq!(issue.code, "SQM3001");
    // Span underlines the literal.
    assert_eq!(&text[issue.span.start..issue.span.end], "'abc'");
    // Related span points at the argument position.
    assert!(!issue.related.is_empty());
}

#[tokio::test]
async fn slot_type_rules_warn_and_error_cases() {
    let engine = validation_fixture().await;

    // Fractional into integer column → TypeRisk warning, still valid.
    let report = engine.validate_query(model_request(
        "update dbo.Person set Age = 12.5 where Id = 1",
    ));
    assert!(
        report.valid,
        "warnings don't invalidate: {:#?}",
        report.issues
    );
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::TypeRisk && i.code == "SQM3002")
    );

    // Out-of-range int → TypeRisk.
    let report = engine.validate_query(model_request(
        "select * from dbo.Person where Age = 999999999999",
    ));
    assert!(
        report.issues.iter().any(|i| i.code == "SQM3003"),
        "{:#?}",
        report.issues
    );

    // Bad GUID → TypeMismatch error.
    let report = engine.validate_query(model_request(
        "select * from dbo.Person where AccountId = 'not-a-guid'",
    ));
    assert!(report.issues.iter().any(|i| i.code == "SQM3005"));
    assert!(!report.valid);

    // Good GUID → clean.
    let report = engine.validate_query(model_request(
        "select * from dbo.Person where AccountId = 'A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11'",
    ));
    assert!(report.valid, "{:#?}", report.issues);

    // String into int predicate that parses numerically → TypeRisk only.
    let report = engine.validate_query(model_request("select * from dbo.Person where Age = '42'"));
    assert!(report.valid);
    assert!(report.issues.iter().any(|i| i.code == "SQM3004"));
}

#[tokio::test]
async fn bare_predicate_columns_are_not_resolved_as_schema_objects() {
    let engine = validation_fixture().await;
    for text in [
        "select * from dbo.Person where Id = 1",
        "update dbo.Person set Age = 42 where Id = 1",
        "delete from dbo.Person where Id = 1",
    ] {
        let report = engine.validate_query(model_request(text));
        assert!(
            !report
                .issues
                .iter()
                .any(|issue| issue.class == IssueClass::UnresolvedObject),
            "predicate column was treated as an object for {text}: {:#?}",
            report.issues
        );
    }

    let report =
        engine.validate_query(model_request("select * from dbo.DoesNotExist where Id = 1"));
    assert!(
        report
            .issues
            .iter()
            .any(|issue| issue.class == IssueClass::UnresolvedObject)
    );
}

#[tokio::test]
async fn insert_arity_mismatch_classified() {
    let engine = validation_fixture().await;
    let report = engine.validate_query(model_request(
        "INSERT INTO dbo.Model (Id, Name) VALUES (1, 'x', 'extra')",
    ));
    assert!(!report.valid);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::ArityMismatch)
        .expect("arity");
    assert_eq!(issue.code, "SQM4001");
    assert!(issue.message.contains("3 value(s) for 2 column(s)"));
}

#[tokio::test]
async fn insert_value_type_checked_by_named_column() {
    let engine = validation_fixture().await;
    let report = engine.validate_query(model_request(
        "INSERT INTO dbo.Person (Id, Name) VALUES ('zzz', 'ok')",
    ));
    assert!(!report.valid, "{:#?}", report.issues);
    assert!(
        report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::TypeMismatch && i.message.contains("Id"))
    );
}

#[tokio::test]
async fn tree_scope_validates_isolated_file() {
    let engine = default_engine();
    // Self-consistent file: fine at tree scope even though nothing is loaded.
    let diags = engine.validate_tree_text(
        "CREATE TABLE dbo.X (Id int NOT NULL PRIMARY KEY, Name varchar(10))",
        Dialect::SqlServer,
    );
    assert!(diags.is_empty(), "{diags:#?}");

    // Internal inconsistency: PK references a missing column.
    let diags = engine.validate_tree_text(
        "CREATE TABLE dbo.X (Id int, CONSTRAINT PK PRIMARY KEY (Nope))",
        Dialect::SqlServer,
    );
    assert!(
        diags
            .iter()
            .any(|d| d.class == IssueClass::ScopeInconsistency)
    );

    // Duplicate column names.
    let diags = engine.validate_tree_text(
        "CREATE TABLE dbo.X (A int, A varchar(5))",
        Dialect::SqlServer,
    );
    assert!(diags.iter().any(|d| d.code == "SQM1101"));
}

#[tokio::test]
async fn recovered_syntax_error_in_fixed_mode_hint_in_edit_mode() {
    let engine = default_engine();
    let broken = "CREATE TABLE dbo.T (\n  Id int NOT NULL\n  Name varchar(50)\n)";

    // Fixed mode: recovery is an error.
    let diags = engine.validate_tree_text(broken, Dialect::SqlServer);
    let syntax: Vec<_> = diags
        .iter()
        .filter(|d| d.class == IssueClass::Syntax)
        .collect();
    assert!(!syntax.is_empty());
    assert!(syntax.iter().all(|d| d.severity == Severity::Error));

    // Edit mode: same recovery is a hint.
    engine.set_mode(EngineMode::Edit {
        exemptions: ExemptionSet::default(),
    });
    let diags = engine.validate_tree_text(broken, Dialect::SqlServer);
    let syntax: Vec<_> = diags
        .iter()
        .filter(|d| d.class == IssueClass::Syntax)
        .collect();
    assert!(syntax.iter().all(|d| d.severity == Severity::Hint));
}

#[tokio::test]
async fn edit_mode_exemptions_suppress_unresolved_and_slot_issues() {
    let engine = validation_fixture().await;
    let text = "select * from dbo.WorkInProgress where x = 1";

    // Fixed: unresolved error.
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid);

    // Edit with exemption covering the entity: suppressed.
    engine.set_mode(EngineMode::Edit {
        exemptions: ExemptionSet {
            entries: vec![Exemption {
                pattern: "workinprogress".into(),
                reason: "actively rewriting".into(),
            }],
        },
    });
    let report = engine.validate_query(model_request(text));
    assert!(
        !report
            .issues
            .iter()
            .any(|i| i.class == IssueClass::UnresolvedObject),
        "exempted entity must not error: {:#?}",
        report.issues
    );

    // Back to fixed: revalidates (cache cleared on transition).
    engine.set_mode(EngineMode::Fixed);
    let report = engine.validate_query(model_request(text));
    assert!(!report.valid);
}

// ---------------------------------------------------------------------------
// Cache behavior (§6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn validation_cache_hits_and_targeted_invalidation() {
    let dir = tempfile::tempdir().unwrap();
    let person = write(
        dir.path(),
        "person.sql",
        "CREATE TABLE dbo.Person (Id int NOT NULL, Name nvarchar(100))",
    );
    write(
        dir.path(),
        "model.sql",
        "CREATE TABLE dbo.Model (Id int NOT NULL)",
    );
    let engine = default_engine();
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    engine.rebuild().await;

    let person_query = "select * from dbo.Person where Id = 1";
    let model_query = "select * from dbo.Model where Id = 1";
    let r1 = engine.validate_query(model_request(person_query));
    let r2 = engine.validate_query(model_request(model_query));
    assert!(r1.valid && r2.valid);
    assert_eq!(engine.cache_len(), 2);

    // Same query again: cache hit (same Arc).
    let r1b = engine.validate_query(model_request(person_query));
    assert!(Arc::ptr_eq(&r1, &r1b), "expected a cache hit");

    // Change Person only → Person's entry invalidated, Model's survives.
    std::fs::write(&person, "CREATE TABLE dbo.Person (Id int NOT NULL)").unwrap();
    engine.notify(Notify::FileChanged { path: person }).await;
    assert_eq!(
        engine.cache_len(),
        1,
        "only the Person-dependent entry should drop"
    );

    // Person query revalidates against the new shape and now fails (Name gone).
    let r3 = engine.validate_query(model_request("select * from dbo.Person where Name = 'x'"));
    // The table resolves; the slot for the missing column just doesn't bind,
    // so the query stays valid — but the cache must have recomputed.
    assert!(!Arc::ptr_eq(&r1, &r3));
}

// ---------------------------------------------------------------------------
// IntelliSense (§8)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn symbol_mode_tables_after_from_with_prefix() {
    let engine = validation_fixture().await;
    let text = "select * from Pe";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::SqlServer,
            default_catalog: None,
            default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
            mode: CompletionMode::Symbol,
        })
        .await;
    assert!(
        completions.items.iter().any(|i| i.label.contains("Person")),
        "{:#?}",
        completions.items
    );
    // Prefix filtered: Model/Car excluded.
    assert!(!completions.items.iter().any(|i| i.label.contains("Model")));
    // Replace span covers the typed prefix.
    let item = completions
        .items
        .iter()
        .find(|i| i.label.contains("Person"))
        .unwrap();
    let span = item.replace_span.expect("replace span");
    assert_eq!(&text[span.start..span.end], "Pe");
}

#[tokio::test]
async fn symbol_mode_columns_after_alias_dot() {
    let engine = validation_fixture().await;
    let text = "select * from dbo.Person p where p.";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::SqlServer,
            default_catalog: None,
            default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
            mode: CompletionMode::Symbol,
        })
        .await;
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert!(labels.contains(&"Id"), "{labels:?}");
    assert!(labels.contains(&"Name"));
    assert!(labels.contains(&"Age"));
    // Column detail carries the native type.
    let name = completions
        .items
        .iter()
        .find(|i| i.label == "Name")
        .unwrap();
    assert_eq!(name.detail.as_deref(), Some("nvarchar(100)"));
}

#[tokio::test]
async fn symbol_mode_procedures_after_exec() {
    let engine = validation_fixture().await;
    let text = "EXEC Imp";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::SqlServer,
            default_catalog: None,
            default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
            mode: CompletionMode::Symbol,
        })
        .await;
    assert!(
        completions
            .items
            .iter()
            .any(|i| i.label.contains("ImportAccounts"))
    );
}

#[tokio::test]
async fn value_mode_distinct_values_with_prefix_filter() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // No prefix: top distinct values, sorted asc.
    let text = "select * from Person p where p.Name = ";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Aaliyah", "Aaron", "Beatrix", "Caleb"],
        "note: {:?}",
        completions.slot_note
    );

    // Typed prefix 'aa' → LIKE '%aa%' (case-insensitive in sqlite for ASCII).
    let text = "select * from Person p where p.Name = 'Aa'";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len() - 1, // inside the literal
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Aaliyah", "Aaron"],
        "{:?}",
        completions.slot_note
    );
    // Inserted text is a quoted string literal for a TEXT column.
    assert!(completions.items[0].insert_text.starts_with('\''));
    // Replace span covers the existing literal.
    assert!(completions.items[0].replace_span.is_some());
}

#[tokio::test]
async fn value_mode_limit_and_sort_params() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * from Person where Name = ";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams {
                limit: Some(2),
                sort: Some(sqlly_intel::modelengine::SortOrder::Desc),
                match_type: None,
                display_column: None,
                trigger: false,
            }),
        })
        .await;
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["Caleb", "Beatrix"]);
    assert!(completions.truncated);
}

#[tokio::test]
async fn value_mode_fk_pivot_returns_key_and_display() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // ModelId is an FK to Model(Id); display column defaults to "Name".
    let text = "select * from Car c where c.ModelId = ";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    assert_eq!(completions.items.len(), 3, "{:?}", completions.slot_note);
    // Labels are display values, insert texts are key literals.
    let aardvark = completions
        .items
        .iter()
        .find(|i| i.label == "Aardvark")
        .expect("display label");
    assert_eq!(
        aardvark.insert_text, "2 /* Aardvark */",
        "insert the key followed by the display value as a comment"
    );
    assert_eq!(aardvark.detail.as_deref(), Some("key: 2"));
    assert!(
        completions
            .slot_note
            .as_deref()
            .unwrap_or("")
            .contains("foreign key")
    );
}

#[tokio::test]
async fn value_mode_fk_pivot_prefix_matches_display_column() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * from Car c where c.ModelId = 'acc'";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len() - 1,
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    // 'acc' matches Macchiato via LIKE on the display column.
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["Macchiato"], "{:?}", completions.slot_note);
    assert_eq!(completions.items[0].insert_text, "3 /* Macchiato */");
}

#[tokio::test]
async fn value_mode_no_slot_returns_note_not_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * ";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    assert!(completions.items.is_empty());
    assert!(completions.slot_note.is_some());
}

#[tokio::test]
async fn value_mode_file_only_object_returns_note() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let fs_dir = tempfile::tempdir().unwrap();
    write(
        fs_dir.path(),
        "newtable.sql",
        "CREATE TABLE Brand (Id INTEGER PRIMARY KEY, Title TEXT)",
    );
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.add_source(SourceConfig::Filesystem {
        root: fs_dir.path().to_path_buf(),
        dialect: Dialect::Sqlite,
    });
    engine.rebuild().await;

    let text = "select * from Brand where Title = ";
    let completions = engine
        .complete(CompletionRequest {
            text: text.into(),
            cursor: text.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams::default()),
        })
        .await;
    assert!(completions.items.is_empty());
    assert!(
        completions
            .slot_note
            .as_deref()
            .unwrap_or("")
            .contains("project files"),
        "{:?}",
        completions.slot_note
    );
}

// ---------------------------------------------------------------------------
// Value-mode pool discipline: completion queries share the db budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn value_mode_queries_respect_db_concurrency() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = engine_with(EngineConcurrency {
        db_concurrency: NonZeroUsize::new(2).unwrap(),
        fs_read_threads: NonZeroUsize::new(2).unwrap(),
    });
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // Fire 10 concurrent value completions; all succeed under a cap of 2.
    let mut handles = Vec::new();
    for _ in 0..10 {
        let engine = Arc::clone(&engine);
        handles.push(tokio::spawn(async move {
            let text = "select * from Person where Name = ";
            engine
                .complete(CompletionRequest {
                    text: text.into(),
                    cursor: text.len(),
                    dialect: Dialect::Sqlite,
                    default_catalog: None,
                    default_namespace: None,
                    mode: CompletionMode::Value(ValueCompletionParams::default()),
                })
                .await
        }));
    }
    for h in handles {
        let completions = h.await.expect("join");
        assert_eq!(completions.items.len(), 4, "{:?}", completions.slot_note);
    }
}

#[tokio::test]
async fn diagnostics_carry_line_column_and_qualified_column_name() {
    // The user-reported case: PaymentMethodId is bigint, the literal is
    // 'test'. The diagnostic must name the column (not just the type), and
    // pinpoint line 9 / the literal's position — not merely "somewhere in
    // this statement".
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "po.sql",
        "CREATE TABLE Accounting.PurchaseOrder (\n  Id bigint NOT NULL PRIMARY KEY,\n  PaymentBankAccountId bigint NULL,\n  CanceledByTenantUserId bigint NULL,\n  CreatedByTenantUserId bigint NULL,\n  InternalReferenceNumber nvarchar(50) NOT NULL,\n  PaymentMethodId bigint NOT NULL\n)",
    );
    let engine = default_engine();
    engine.add_source(SourceConfig::Filesystem {
        root: dir.path().to_path_buf(),
        dialect: Dialect::SqlServer,
    });
    engine.rebuild().await;

    let text = "SELECT\n    po.PaymentBankAccountId,\n    po.CanceledByTenantUserId,\n    po.CreatedByTenantUserId\nFROM\n    Accounting.PurchaseOrder po\nwhere\n    po.InternalReferenceNumber = 'OCMO-000010' and\n    po.PaymentMethodId = 'test'\n";
    let report = engine.validate_query(QueryValidationRequest {
        text: text.into(),
        dialect: Dialect::SqlServer,
        default_catalog: None,
        default_namespace: Some(SqlName::new("Accounting", Dialect::SqlServer, false)),
        scope: ValidationScope::Model,
    });
    assert!(!report.valid, "{:#?}", report.issues);
    let issue = report
        .issues
        .iter()
        .find(|i| i.class == IssueClass::TypeMismatch)
        .expect("type mismatch issue");
    // Message names the column and the offending value, not just the type.
    assert!(
        issue.message.contains("PaymentMethodId"),
        "must name the column: {}",
        issue.message
    );
    assert!(
        issue.message.contains("'test'"),
        "must name the value: {}",
        issue.message
    );
    assert!(
        issue.message.contains("bigint"),
        "must name the type: {}",
        issue.message
    );
    // Span covers exactly the bad literal; line/column point at it (1-based).
    assert_eq!(&text[issue.span.start..issue.span.end], "'test'");
    assert_eq!(issue.line, 9, "literal is on line 9");
    let line9 = text.lines().nth(8).unwrap();
    let expected_col = line9.find("'test'").unwrap() as u32 + 1;
    assert_eq!(issue.column, expected_col);
    // The valid string predicate on line 8 produced no issue.
    assert!(
        !report.issues.iter().any(|i| i.line == 8),
        "{:#?}",
        report.issues
    );
}

// ---------------------------------------------------------------------------
// Value-mode FK comment + per-trigger caching (§8.2 / §8.3)
// ---------------------------------------------------------------------------

/// Build a value-mode request for the FK column `Car.ModelId`, optionally with
/// a typed literal and an explicit trigger.
fn fk_value_request(text: &str, cursor: usize, trigger: bool) -> CompletionRequest {
    CompletionRequest {
        text: text.into(),
        cursor,
        dialect: Dialect::Sqlite,
        default_catalog: None,
        default_namespace: None,
        mode: CompletionMode::Value(ValueCompletionParams {
            trigger,
            ..Default::default()
        }),
    }
}

#[tokio::test]
async fn fk_value_insert_text_sanitizes_comment_delimiters() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    // A display value containing `*/` must not be able to terminate the comment.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute(
            "INSERT INTO Model (Id, Name) VALUES (4, 'Evil */ DROP')",
            [],
        )
        .unwrap();
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * from Car c where c.ModelId = ";
    let completions = engine
        .complete(fk_value_request(text, text.len(), true))
        .await;
    let evil = completions
        .items
        .iter()
        .find(|i| i.label == "Evil */ DROP")
        .expect("display label present");
    // Key first, then a SAFE comment: the `*/` is neutralized to `* /`.
    assert_eq!(evil.insert_text, "4 /* Evil * / DROP */");
    assert!(
        !evil.insert_text[3..evil.insert_text.len() - 3].contains("*/"),
        "no inner `*/` may survive: {}",
        evil.insert_text
    );
}

#[tokio::test]
async fn trigger_fetches_and_populates_cache() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;
    assert_eq!(engine.value_cache_len(), 0, "cache starts empty");

    let text = "select * from Car c where c.ModelId = ";
    let completions = engine
        .complete(fk_value_request(text, text.len(), true))
        .await;
    assert_eq!(completions.items.len(), 3);
    assert_eq!(
        engine.value_cache_len(),
        1,
        "an explicit trigger primes the slot cache"
    );
}

#[tokio::test]
async fn keystroke_filters_cached_rows_without_requery() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // Prime the cache via an explicit trigger (empty literal).
    let trigger_text = "select * from Car c where c.ModelId = ";
    let primed = engine
        .complete(fk_value_request(trigger_text, trigger_text.len(), true))
        .await;
    assert_eq!(primed.items.len(), 3);

    // Mutate the database AFTER priming by adding a row that WOULD match the
    // upcoming keystroke. A keystroke must NOT re-query, so the new row must be
    // invisible (only the cached candidate set is filtered).
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("INSERT INTO Model (Id, Name) VALUES (4, 'Aardwolf')", [])
        .unwrap();

    // Keystroke 'Aa' (trigger=false) filters the cached rows locally: it sees
    // the cached 'Aardvark' but not the post-trigger 'Aardwolf'.
    let typed = "select * from Car c where c.ModelId = 'Aa'";
    let filtered = engine
        .complete(fk_value_request(typed, typed.len() - 1, false))
        .await;
    let labels: Vec<&str> = filtered.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Aardvark"],
        "keystroke filters the cached set, ignoring the post-trigger insert"
    );
    assert_eq!(filtered.items[0].insert_text, "2 /* Aardvark */");
}

#[tokio::test]
async fn retrigger_invalidates_and_refetches() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * from Car c where c.ModelId = ";
    let first = engine
        .complete(fk_value_request(text, text.len(), true))
        .await;
    assert_eq!(first.items.len(), 3);

    // Add a candidate, then explicitly re-trigger: the stale cache must be
    // replaced by a fresh query that includes the new row.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("INSERT INTO Model (Id, Name) VALUES (4, 'Aardwolf')", [])
        .unwrap();
    let second = engine
        .complete(fk_value_request(text, text.len(), true))
        .await;
    let labels: Vec<&str> = second.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Aardwolf"),
        "re-trigger must refetch and include the new row, got {labels:?}"
    );
    assert_eq!(second.items.len(), 4);
}

#[tokio::test]
async fn keystroke_cache_miss_does_a_live_query_without_caching() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // No prior trigger: a keystroke (trigger=false) still returns live results
    // but must NOT seed the cache (only explicit triggers do).
    let typed = "select * from Car c where c.ModelId = 'Aa'";
    let live = engine
        .complete(fk_value_request(typed, typed.len() - 1, false))
        .await;
    let labels: Vec<&str> = live.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels, vec!["Aardvark"]);
    assert_eq!(
        engine.value_cache_len(),
        0,
        "a keystroke miss must not populate the cache"
    );
}

#[tokio::test]
async fn rebuild_clears_value_cache() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    let text = "select * from Car c where c.ModelId = ";
    engine
        .complete(fk_value_request(text, text.len(), true))
        .await;
    assert_eq!(engine.value_cache_len(), 1);

    // A snapshot rebuild invalidates cached candidates (§8.2 lifetime rule).
    engine.rebuild().await;
    assert_eq!(
        engine.value_cache_len(),
        0,
        "rebuild must clear the value cache"
    );
}

#[tokio::test]
async fn trigger_with_existing_prefix_seeds_full_set_and_filters_display() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // Explicit trigger while the cursor already sits inside a typed literal.
    // The cache must be seeded with the FULL candidate set (so later keystrokes
    // can broaden/narrow locally), while the VISIBLE items are filtered by the
    // current prefix.
    let typed = "select * from Car c where c.ModelId = 'Aa'";
    let completions = engine
        .complete(fk_value_request(typed, typed.len() - 1, true))
        .await;
    let labels: Vec<&str> = completions.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        labels,
        vec!["Aardvark"],
        "visible items are filtered by the typed prefix, got {labels:?}"
    );
    assert_eq!(completions.items[0].insert_text, "2 /* Aardvark */");
    assert_eq!(engine.value_cache_len(), 1, "the trigger primed the slot");

    // The cached set is the FULL list, not just the filtered view: a follow-up
    // keystroke that broadens the prefix to empty must surface every candidate
    // without a re-query.
    let broadened = "select * from Car c where c.ModelId = ''";
    let all = engine
        .complete(fk_value_request(broadened, broadened.len() - 1, false))
        .await;
    let all_labels: Vec<&str> = all.items.iter().map(|i| i.label.as_str()).collect();
    assert_eq!(
        all_labels.len(),
        3,
        "the cached set holds the full candidate list, got {all_labels:?}"
    );
}

#[tokio::test]
async fn non_like_match_type_keystroke_bypasses_cache_filter() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    let engine = default_engine();
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // Prime the cache with a non-`Like` match type. Local substring filtering
    // is only valid for `Like`; a comparison operator treats the prefix as an
    // operand, so a keystroke must re-query the database rather than filter the
    // cached rows.
    let request = |text: &str, cursor: usize, trigger: bool| CompletionRequest {
        text: text.into(),
        cursor,
        dialect: Dialect::Sqlite,
        default_catalog: None,
        default_namespace: None,
        mode: CompletionMode::Value(ValueCompletionParams {
            match_type: Some(sqlly_intel::modelengine::MatchType::Equals),
            trigger,
            ..Default::default()
        }),
    };

    let text = "select * from Car c where c.ModelId = ";
    engine.complete(request(text, text.len(), true)).await;
    assert_eq!(engine.value_cache_len(), 1, "trigger primed the slot");

    // Add an exact-match candidate after priming. Because the cached plan is a
    // non-`Like` comparison, the keystroke must NOT reuse the stale cache; it
    // must re-query and therefore see the freshly inserted row.
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute("INSERT INTO Model (Id, Name) VALUES (4, 'Aardwolf')", [])
        .unwrap();
    let typed = "select * from Car c where c.ModelId = 'Aardwolf'";
    let keystroke = engine
        .complete(request(typed, typed.len() - 1, false))
        .await;
    let labels: Vec<&str> = keystroke.items.iter().map(|i| i.label.as_str()).collect();
    assert!(
        labels.contains(&"Aardwolf"),
        "a non-LIKE keystroke must re-query and see the new row, got {labels:?}"
    );
}

#[tokio::test]
async fn value_cache_evicts_least_recently_used_past_capacity() {
    let dir = tempfile::tempdir().unwrap();
    let db = sqlite_fixture(dir.path());
    // Capacity of 1 makes the LRU policy observable with two distinct slots.
    let engine =
        sqlly_intel::modelengine::ModelEngine::new(sqlly_intel::modelengine::EngineConfig {
            default_namespace: Some(SqlName::new("dbo", Dialect::SqlServer, false)),
            value_cache_capacity: 1,
            ..Default::default()
        });
    add_sqlite_source(&engine, &db);
    engine.rebuild().await;

    // Slot A: Car.ModelId (FK to Model).
    let slot_a = "select * from Car c where c.ModelId = ";
    engine
        .complete(fk_value_request(slot_a, slot_a.len(), true))
        .await;
    assert_eq!(engine.value_cache_len(), 1);

    // Slot B: a distinct column (Person.Name distinct values) primed by trigger.
    let slot_b = "select * from Person p where p.Name = ";
    engine
        .complete(CompletionRequest {
            text: slot_b.into(),
            cursor: slot_b.len(),
            dialect: Dialect::Sqlite,
            default_catalog: None,
            default_namespace: None,
            mode: CompletionMode::Value(ValueCompletionParams {
                trigger: true,
                ..Default::default()
            }),
        })
        .await;
    assert_eq!(
        engine.value_cache_len(),
        1,
        "capacity of 1 must evict the older slot when a new one is primed"
    );

    // The evicted (older) slot A now misses: a keystroke returns live results
    // but does not repopulate the cache (only triggers do).
    let typed_a = "select * from Car c where c.ModelId = 'Aa'";
    let after = engine
        .complete(fk_value_request(typed_a, typed_a.len() - 1, false))
        .await;
    assert!(
        after.items.iter().any(|i| i.label == "Aardvark"),
        "evicted slot still answers via a live query"
    );
    assert_eq!(
        engine.value_cache_len(),
        1,
        "a keystroke miss on the evicted slot must not grow the cache"
    );
}
