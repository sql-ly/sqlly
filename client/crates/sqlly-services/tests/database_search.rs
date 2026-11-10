use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, ObjectKind, ResultColumn, SecretProvider, SecretRef, StudioResult,
};
use sqlly_services::{
    DefaultCatalogService, DefaultIntellisense, ScriptCancel,
    database_search::{MatchMode, SearchKind, SearchOptions, build_table_sql, run_search},
    metadata::{BulkColumnRow, MetadataNode, MetadataService},
    query::{QueryEvent, QueryExecutor, QueryHandle},
    services::Services,
};
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct FakeQueries {
    #[allow(clippy::type_complexity)]
    executed: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

#[async_trait]
impl QueryExecutor for FakeQueries {
    async fn execute(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.execute_in_session(profile, None, sql, row_buffer)
            .await
    }

    async fn execute_in_session(
        &self,
        _profile: &ConnectionProfile,
        session: Option<&str>,
        sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        self.executed
            .lock()
            .unwrap()
            .push((sql.to_string(), session.map(str::to_string)));
        let (tx, rx) = mpsc::channel(8);
        if sql.contains("/*FAIL*/")
            || sql.contains("[bad]")
            || sql.contains("`bad`")
            || sql.contains(r#""bad""#)
        {
            tx.send(QueryEvent::Error {
                message: "boom".into(),
            })
            .await
            .ok();
        } else if sql.contains("/*HIT*/") {
            tx.send(QueryEvent::ResultSet {
                index: 0,
                columns: vec![ResultColumn {
                    name: "name".into(),
                    data_type: "nvarchar".into(),
                    nullable: true,
                }],
            })
            .await
            .ok();
            tx.send(QueryEvent::Rows {
                index: 0,
                rows: vec![vec![CellValue::String("/*HIT*/".into())]],
            })
            .await
            .ok();
            tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
        } else {
            tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
        }
        Ok(QueryHandle {
            task_id: "t".into(),
            events: rx,
        })
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeMetadata {
    tables: Vec<(String, String)>,
    columns: Vec<BulkColumnRow>,
}

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
        Ok(self
            .tables
            .iter()
            .filter(|(s, _)| schema.is_none_or(|filter| s == filter))
            .map(|(schema, name)| {
                MetadataNode::new(name.clone(), ObjectKind::Table).with_schema(schema.clone())
            })
            .collect())
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
        Ok(self.columns.clone())
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

fn profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "id",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".into(),
        },
    )
}

fn services(fake_q: FakeQueries, fake_m: FakeMetadata) -> Services {
    let queries: Arc<dyn QueryExecutor> = Arc::new(fake_q);
    Services::from_parts(
        Arc::new(sqlly_services::DefaultConnectionManager::new()),
        Arc::new(DefaultIntellisense),
        queries,
        Arc::new(DefaultCatalogService::new()),
        Arc::new(fake_m),
        Arc::new(sqlly_services::DefaultAdminToolsService),
        Arc::new(sqlly_services::DefaultRowEditingService::new(
            sqlly_services::TdsPool::new(),
        )),
    )
}

fn options(value: &str) -> SearchOptions {
    SearchOptions {
        value: value.into(),
        kind: SearchKind::Text,
        match_mode: MatchMode::Exact,
        case_insensitive: true,
        max_tables: 10,
        rows_per_table: 5,
        schema_filter: None,
    }
}

fn metadata_two_tables() -> FakeMetadata {
    FakeMetadata {
        tables: vec![
            ("dbo".into(), "alpha".into()),
            ("dbo".into(), "beta".into()),
        ],
        columns: vec![
            BulkColumnRow {
                schema: "dbo".into(),
                object: "alpha".into(),
                column: "name".into(),
                type_name: Some("varchar".into()),
                nullable: Some(true),
                is_identity: false,
            },
            BulkColumnRow {
                schema: "dbo".into(),
                object: "beta".into(),
                column: "name".into(),
                type_name: Some("varchar".into()),
                nullable: Some(true),
                is_identity: false,
            },
        ],
    }
}

#[tokio::test]
async fn search_runs_one_select_per_table_and_collects_hits() {
    let fake_q = FakeQueries::default();
    let fake_m = metadata_two_tables();
    let services = services(fake_q.clone(), fake_m);
    let (tx, mut rx) = mpsc::channel(8);
    let summary = run_search(
        &services,
        &profile(),
        "app",
        &options("/*HIT*/"),
        tx,
        ScriptCancel::new(),
    )
    .await
    .unwrap();
    assert_eq!(summary.hits.len(), 2);
    assert_eq!(fake_q.executed.lock().unwrap().len(), 2);
    let mut terminal = None;
    while let Ok(progress) = rx.try_recv() {
        if progress.terminal {
            terminal = Some(progress);
        }
    }
    assert_eq!(terminal.unwrap().hits, 2);
}

#[tokio::test]
async fn search_continues_after_a_table_error() {
    let fake_q = FakeQueries::default();
    let fake_m = FakeMetadata {
        tables: vec![("dbo".into(), "bad".into()), ("dbo".into(), "good".into())],
        columns: vec![
            BulkColumnRow {
                schema: "dbo".into(),
                object: "bad".into(),
                column: "name".into(),
                type_name: Some("varchar".into()),
                nullable: Some(true),
                is_identity: false,
            },
            BulkColumnRow {
                schema: "dbo".into(),
                object: "good".into(),
                column: "name".into(),
                type_name: Some("varchar".into()),
                nullable: Some(true),
                is_identity: false,
            },
        ],
    };
    let services = services(fake_q.clone(), fake_m);
    let (tx, _rx) = mpsc::channel(8);
    let summary = run_search(
        &services,
        &profile(),
        "app",
        &options("/*HIT*/"),
        tx,
        ScriptCancel::new(),
    )
    .await
    .unwrap();
    assert_eq!(summary.errors.len(), 1);
    assert_eq!(summary.hits.len(), 1);
    let executed = fake_q.executed.lock().unwrap();
    assert!(executed[0].0.contains("bad"));
    assert!(executed[1].0.contains("good"));
}

#[tokio::test]
async fn search_stops_when_cancelled() {
    let fake_q = FakeQueries::default();
    let services = services(fake_q.clone(), metadata_two_tables());
    let cancel = ScriptCancel::new();
    cancel.cancel();
    let (tx, _rx) = mpsc::channel(8);
    let summary = run_search(
        &services,
        &profile(),
        "app",
        &options("/*HIT*/"),
        tx,
        cancel,
    )
    .await
    .unwrap();
    assert!(summary.cancelled);
    assert!(fake_q.executed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn search_reports_terminal_progress() {
    let services = services(FakeQueries::default(), metadata_two_tables());
    let (tx, mut rx) = mpsc::channel(8);
    let _summary = run_search(
        &services,
        &profile(),
        "app",
        &options("miss"),
        tx,
        ScriptCancel::new(),
    )
    .await
    .unwrap();
    let mut saw_terminal = false;
    while let Some(progress) = rx.recv().await {
        if progress.terminal {
            saw_terminal = true;
            assert_eq!(progress.tables_total, 2);
        }
    }
    assert!(saw_terminal);
}

#[test]
fn build_table_sql_marks_fail_tables() {
    let sql = build_table_sql(
        profile().database_type,
        &sqlly_services::database_search::SearchTarget {
            schema: "dbo".into(),
            table: "bad".into(),
            columns: vec!["name".into()],
        },
        &options("/*FAIL*/"),
    );
    assert!(sql.contains("bad"));
}
