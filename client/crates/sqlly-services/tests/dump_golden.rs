//! Golden-file integration tests for `run_database_dump`.

use std::sync::Mutex;
use std::sync::{Arc, atomic::AtomicBool};

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, ObjectKind, SecretProvider, SecretRef, StudioResult,
};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::{
    BulkColumnRow, DefaultCatalogService, DefaultIntellisense, DumpContent, DumpLayout,
    DumpOptions, ForeignKeyRow, MetadataNode, MetadataService, QueryEvent, QueryExecutor,
    QueryHandle, Services, run_database_dump,
};
use sqlly_testkit::golden::GoldenTest;
use tempfile::tempdir;
use tokio::sync::mpsc;

struct FakeMetadata;

#[async_trait]
impl MetadataService for FakeMetadata {
    async fn list_schemas(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
    ) -> StudioResult<Vec<String>> {
        Ok(vec!["dbo".into()])
    }

    async fn list_tables(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        let _ = schema;
        Ok(vec![
            MetadataNode::new("customers", ObjectKind::Table).with_schema("dbo"),
            MetadataNode::new("orders", ObjectKind::Table).with_schema("dbo"),
            MetadataNode::new("audit", ObjectKind::Table).with_schema("dbo"),
        ])
    }

    async fn list_views(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_programmable_objects(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: Option<&str>,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_columns(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _object: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_all_columns(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
    ) -> StudioResult<Vec<BulkColumnRow>> {
        Ok(vec![
            col("dbo", "customers", "id"),
            col("dbo", "customers", "name"),
            col("dbo", "orders", "id"),
            col("dbo", "orders", "customer_id"),
            col("dbo", "audit", "id"),
            col("dbo", "audit", "note"),
        ])
    }

    async fn list_foreign_keys(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
    ) -> StudioResult<Vec<ForeignKeyRow>> {
        Ok(vec![ForeignKeyRow {
            constraint_name: "fk_orders_customers".into(),
            from_schema: "dbo".into(),
            from_table: "orders".into(),
            from_column: "customer_id".into(),
            to_schema: "dbo".into(),
            to_table: "customers".into(),
            to_column: "id".into(),
        }])
    }

    async fn list_procedure_parameters(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _object: &str,
    ) -> StudioResult<Vec<sqlly_services::ProcedureParameter>> {
        Ok(Vec::new())
    }

    async fn list_indexes(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_triggers(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_constraints(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }

    async fn list_statistics(
        &self,
        _profile: &ConnectionProfile,
        _database: &str,
        _schema: &str,
        _table: &str,
    ) -> StudioResult<Vec<MetadataNode>> {
        Ok(Vec::new())
    }
}

fn col(schema: &str, object: &str, column: &str) -> BulkColumnRow {
    BulkColumnRow {
        schema: schema.into(),
        object: object.into(),
        column: column.into(),
        type_name: Some("int".into()),
        nullable: Some(true),
        is_identity: column == "id",
    }
}

struct FakeDumpQueries {
    executed: Mutex<Vec<String>>,
}

impl FakeDumpQueries {
    fn new() -> Self {
        Self {
            executed: Mutex::new(Vec::new()),
        }
    }

    fn column_row(name: &str) -> Vec<CellValue> {
        vec![
            CellValue::String(name.into()),
            CellValue::String("int".into()),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
            CellValue::Bool(true),
            CellValue::Bool(name == "id"),
            CellValue::Null,
            CellValue::Null,
            CellValue::Bool(false),
            CellValue::Null,
            CellValue::Null,
            CellValue::Null,
        ]
    }

    fn data_rows(table: &str) -> Vec<Vec<CellValue>> {
        match table {
            "customers" => vec![
                vec![CellValue::I64(1), CellValue::String("Ann".into())],
                vec![CellValue::I64(2), CellValue::String("Bob".into())],
            ],
            "orders" => vec![
                vec![CellValue::I64(10), CellValue::I64(1)],
                vec![CellValue::I64(11), CellValue::I64(2)],
            ],
            _ => vec![
                vec![CellValue::I64(1), CellValue::String("seed".into())],
                vec![CellValue::I64(2), CellValue::String("audit".into())],
            ],
        }
    }
}

#[async_trait]
impl QueryExecutor for FakeDumpQueries {
    async fn execute(
        &self,
        _profile: &ConnectionProfile,
        sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.executed.lock().unwrap().push(sql.to_string());
        let (tx, rx) = mpsc::channel(16);
        if sql.to_ascii_uppercase().starts_with("SELECT * FROM") {
            let table = sql
                .rsplit('.')
                .next()
                .unwrap_or("")
                .trim_matches(|c| c == ';' || c == ']' || c == '"');
            let table = table.trim_matches(|c| c == '[');
            let columns: Vec<&str> = match table {
                "customers" => vec!["id", "name"],
                "orders" => vec!["id", "customer_id"],
                _ => vec!["id", "note"],
            };
            tx.send(QueryEvent::ResultSet {
                index: 0,
                columns: columns
                    .into_iter()
                    .map(|name| sqlly_core::ResultColumn {
                        name: name.into(),
                        data_type: "int".into(),
                        nullable: true,
                    })
                    .collect(),
            })
            .await
            .ok();
            tx.send(QueryEvent::Rows {
                index: 0,
                rows: Self::data_rows(table),
            })
            .await
            .ok();
        } else if sql.to_ascii_uppercase().contains("FROM") {
            let table = ["customers", "orders", "audit"]
                .into_iter()
                .find(|t| sql.contains(t))
                .unwrap_or("audit");
            for col in match table {
                "customers" => ["id", "name"],
                "orders" => ["id", "customer_id"],
                _ => ["id", "note"],
            } {
                tx.send(QueryEvent::Rows {
                    index: 0,
                    rows: vec![Self::column_row(col)],
                })
                .await
                .ok();
            }
        }
        tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
        Ok(QueryHandle {
            task_id: "dump-test".into(),
            events: rx,
        })
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        Ok(())
    }
}

fn profile(db: DatabaseType) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        "dump-test",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".into(),
        },
    );
    profile.database_type = db;
    profile.database = Some("appdb".into());
    profile
}

fn services() -> Services {
    let queries: Arc<dyn QueryExecutor> = Arc::new(FakeDumpQueries::new());
    Services::from_parts(
        Arc::new(sqlly_services::DefaultConnectionManager::new()),
        Arc::new(DefaultIntellisense),
        queries,
        Arc::new(DefaultCatalogService::new()),
        Arc::new(FakeMetadata),
        Arc::new(sqlly_services::DefaultAdminToolsService),
        Arc::new(sqlly_services::DefaultRowEditingService::new(
            sqlly_services::TdsPool::new(),
        )),
    )
}

fn redact_timestamp(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.starts_with("-- Generated:") {
                "-- Generated: <redacted>"
            } else if line.starts_with("-- Version:") {
                // The dump header stamps the crate version, which changes on
                // every release. Redact it so the goldens don't churn per bump.
                "-- Version: <redacted>"
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_golden(db: DatabaseType, engine: &str) {
    let dir = tempdir().unwrap();
    let output = dir.path().join("dump.sql");
    let cancel = Arc::new(AtomicBool::new(false));
    let report = run_database_dump(
        &services(),
        &profile(db),
        "appdb",
        &output,
        &DumpOptions::default(),
        cancel,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(!report.cancelled);
    assert!(report.rows_written >= 6);
    let text = std::fs::read_to_string(&output).unwrap();
    let normalized = redact_timestamp(&text);
    let golden = format!(
        "{}/tests/golden/dump/{engine}.sql",
        env!("CARGO_MANIFEST_DIR")
    );
    if !GoldenTest::compare(&normalized, &golden).unwrap() {
        eprintln!("--- expected ({golden})");
        eprintln!("{}", std::fs::read_to_string(&golden).unwrap_or_default());
        eprintln!("--- actual");
        eprintln!("{normalized}");
        panic!("golden mismatch for {engine}; set SQLLY_UPDATE_GOLDEN=1 to refresh");
    }
}

#[tokio::test]
async fn golden_sqlserver() {
    run_golden(DatabaseType::SqlServer, "sqlserver").await;
}

#[tokio::test]
async fn golden_postgres() {
    run_golden(DatabaseType::Postgres, "postgres").await;
}

#[tokio::test]
async fn golden_mysql() {
    run_golden(DatabaseType::MySql, "mysql").await;
}

#[tokio::test]
async fn golden_sqlite() {
    run_golden(DatabaseType::Sqlite, "sqlite").await;
}

#[tokio::test]
async fn file_per_object_manifest_orders_customers_before_orders() {
    let dir = tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let report = run_database_dump(
        &services(),
        &profile(DatabaseType::Postgres),
        "appdb",
        dir.path(),
        &DumpOptions {
            layout: DumpLayout::FilePerObject,
            ..Default::default()
        },
        cancel,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(!report.cancelled);
    let manifest = std::fs::read_to_string(dir.path().join("manifest.sql")).unwrap();
    let customers = manifest.find("customers").unwrap_or(usize::MAX);
    let orders = manifest.find("orders").unwrap_or(usize::MAX);
    assert!(customers < orders);
}

/// An explicit structure order numbers the per-object files in that order
/// (the Deploy Cart's dependency order), and objects it doesn't name follow.
#[tokio::test]
async fn file_per_object_numbers_files_in_an_explicit_structure_order() {
    let dir = tempdir().unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let report = run_database_dump(
        &services(),
        &profile(DatabaseType::Postgres),
        "appdb",
        dir.path(),
        &DumpOptions {
            layout: DumpLayout::FilePerObject,
            content: DumpContent::StructureOnly,
            structure_order: Some(vec!["dbo.orders".into(), "dbo.customers".into()]),
            ..Default::default()
        },
        cancel,
        &mut |_| {},
    )
    .await
    .unwrap();
    let mut structure: Vec<String> = report
        .files
        .iter()
        .filter(|p| p.parent().and_then(|d| d.file_name()) == Some("structure".as_ref()))
        .filter_map(|p| p.file_name()?.to_str().map(str::to_string))
        .collect();
    structure.sort();
    assert_eq!(
        structure,
        vec![
            "001_dbo.orders.sql",
            "002_dbo.customers.sql",
            "003_dbo.audit.sql"
        ]
    );
    let manifest = std::fs::read_to_string(dir.path().join("manifest.sql")).unwrap();
    let orders = manifest.find("001_dbo.orders").expect("orders listed");
    let customers = manifest
        .find("002_dbo.customers")
        .expect("customers listed");
    assert!(
        orders < customers,
        "the manifest runs the files in number order"
    );
}

#[tokio::test]
async fn cancel_before_data_leaves_marker() {
    let dir = tempdir().unwrap();
    let output = dir.path().join("partial.sql");
    let cancel = Arc::new(AtomicBool::new(true));
    let report = run_database_dump(
        &services(),
        &profile(DatabaseType::Postgres),
        "appdb",
        &output,
        &DumpOptions {
            content: DumpContent::DataOnly,
            ..Default::default()
        },
        cancel,
        &mut |_| {},
    )
    .await
    .unwrap();
    assert!(report.cancelled);
    let text = std::fs::read_to_string(&output).unwrap();
    assert!(text.trim_end().ends_with("-- CANCELLED"));
}
