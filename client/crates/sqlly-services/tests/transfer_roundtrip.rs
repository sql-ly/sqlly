//! Integration tests for the transfer runner with a fake query executor and recording sink.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use async_trait::async_trait;
use sqlly_core::{
    CellValue, ConnectionProfile, ResultColumn, SecretProvider, SecretRef, StudioResult,
};
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::{
    Services,
    intellisense::DefaultIntellisense,
    query::{QueryEvent, QueryExecutor, QueryHandle},
    transfer::{
        plan::{NameCasing, TransferColumn, TransferMode, TransferPlan, TransferTable},
        run::{TransferCancel, TransferProgress, run_transfer},
        sink::RecordingSink,
    },
};
use tokio::sync::mpsc;

struct FakeQueryExecutor {
    total_rows: usize,
    cancel_flag: Arc<AtomicBool>,
}

impl FakeQueryExecutor {
    fn row(n: i64) -> Vec<CellValue> {
        vec![
            CellValue::I64(n),
            CellValue::String(format!("row-{n}")),
            CellValue::I64(n * 10),
        ]
    }

    async fn pump_rows(
        tx: mpsc::Sender<QueryEvent>,
        total_rows: usize,
        cancel_flag: Arc<AtomicBool>,
    ) {
        let columns = vec![
            ResultColumn {
                name: "c1".into(),
                data_type: "int".into(),
                nullable: false,
            },
            ResultColumn {
                name: "c2".into(),
                data_type: "varchar".into(),
                nullable: true,
            },
            ResultColumn {
                name: "c3".into(),
                data_type: "int".into(),
                nullable: false,
            },
        ];
        let _ = tx.send(QueryEvent::ResultSet { index: 0, columns }).await;
        let mut sent = 0usize;
        while sent < total_rows {
            let chunk = (total_rows - sent).min(500);
            let rows: Vec<_> = (0..chunk)
                .map(|offset| Self::row((sent + offset) as i64))
                .collect();
            sent += chunk;
            let _ = tx.send(QueryEvent::Rows { index: 0, rows }).await;
            if cancel_flag.load(Ordering::SeqCst) {
                let _ = tx.send(QueryEvent::Cancelled).await;
                return;
            }
        }
        let _ = tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await;
    }
}

#[async_trait]
impl QueryExecutor for FakeQueryExecutor {
    async fn execute(
        &self,
        _profile: &ConnectionProfile,
        _sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<QueryHandle> {
        let (tx, rx) = mpsc::channel(64);
        let total_rows = self.total_rows;
        let cancel_flag = self.cancel_flag.clone();
        tokio::spawn(async move {
            Self::pump_rows(tx, total_rows, cancel_flag).await;
        });
        Ok(QueryHandle {
            task_id: "fake-transfer".into(),
            events: rx,
        })
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        self.cancel_flag.store(true, Ordering::SeqCst);
        Ok(())
    }
}

fn test_profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "transfer-test",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    )
}

fn three_col_table(name: &str) -> TransferTable {
    TransferTable {
        schema: Some("dbo".into()),
        name: name.into(),
        columns: vec![
            TransferColumn {
                name: "c1".into(),
                source_type: "int".into(),
                nullable: false,
                is_identity: false,
                pk_ordinal: Some(1),
            },
            TransferColumn {
                name: "c2".into(),
                source_type: "nvarchar(50)".into(),
                nullable: true,
                is_identity: false,
                pk_ordinal: None,
            },
            TransferColumn {
                name: "c3".into(),
                source_type: "int".into(),
                nullable: false,
                is_identity: false,
                pk_ordinal: None,
            },
        ],
        depends_on: Vec::new(),
    }
}

fn services(executor: Arc<dyn QueryExecutor>) -> Services {
    let metadata_executor = executor.clone();
    Services::from_parts(
        Arc::new(sqlly_services::connection::DefaultConnectionManager::new()),
        Arc::new(DefaultIntellisense),
        executor,
        Arc::new(sqlly_services::catalog::DefaultCatalogService::new()),
        Arc::new(sqlly_services::metadata::DefaultMetadataService::new(
            metadata_executor,
        )),
        Arc::new(sqlly_services::admin_tools::DefaultAdminToolsService),
        Arc::new(sqlly_services::row_editing::DefaultRowEditingService::new(
            sqlly_services::tds_pool::TdsPool::new(),
        )),
    )
}

fn append_plan(target: DatabaseType, table: TransferTable) -> TransferPlan {
    TransferPlan {
        source: DatabaseType::SqlServer,
        target,
        target_schema: Some("dbo".into()),
        mode: TransferMode::Append,
        casing: NameCasing::AsIs,
        tables: vec![table],
    }
}

async fn drain_progress(rx: &mut mpsc::Receiver<TransferProgress>) -> Vec<TransferProgress> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

#[tokio::test]
async fn sql_server_append_batches() {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 2350,
        cancel_flag,
    });
    let services = services(executor);
    let plan = append_plan(DatabaseType::SqlServer, three_col_table("t"));
    let mut sink = RecordingSink {
        db: DatabaseType::SqlServer,
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel(64);
    let outcome = run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    assert_eq!(outcome.tables, vec![("dbo.t".to_string(), 2350)]);
    let param_counts: Vec<usize> = sink.statements.iter().map(|(_, n)| *n).collect();
    assert_eq!(param_counts, vec![1998, 1998, 1998, 1056]);
    assert!(sink.tx_events.contains(&"begin"));
    assert!(sink.tx_events.contains(&"commit"));
    let events = drain_progress(&mut rx).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, TransferProgress::Finished(_)))
    );
}

#[tokio::test]
async fn postgres_append_batches() {
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 2350,
        cancel_flag: Arc::new(AtomicBool::new(false)),
    });
    let services = services(executor);
    let plan = append_plan(DatabaseType::Postgres, three_col_table("t"));
    let mut sink = RecordingSink {
        db: DatabaseType::Postgres,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(64);
    let outcome = run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    assert_eq!(outcome.tables[0].1, 2350);
    let param_counts: Vec<usize> = sink.statements.iter().map(|(_, n)| *n).collect();
    assert_eq!(param_counts, vec![3000, 3000, 1050]);
}

#[tokio::test]
async fn create_table_precedes_inserts() {
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 10,
        cancel_flag: Arc::new(AtomicBool::new(false)),
    });
    let services = services(executor);
    let plan = TransferPlan {
        source: DatabaseType::SqlServer,
        target: DatabaseType::SqlServer,
        target_schema: Some("dbo".into()),
        mode: TransferMode::CreateTable,
        casing: NameCasing::AsIs,
        tables: vec![three_col_table("t")],
    };
    let mut sink = RecordingSink {
        db: DatabaseType::SqlServer,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(8);
    run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    assert!(sink.statements[0].0.starts_with("CREATE TABLE"));
    assert_eq!(sink.statements[0].1, 0);
}

#[tokio::test]
async fn sqlite_truncate_uses_delete() {
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 1,
        cancel_flag: Arc::new(AtomicBool::new(false)),
    });
    let services = services(executor);
    let mut table = three_col_table("t");
    table.schema = None;
    let plan = TransferPlan {
        source: DatabaseType::Sqlite,
        target: DatabaseType::Sqlite,
        target_schema: None,
        mode: TransferMode::TruncateThenInsert,
        casing: NameCasing::AsIs,
        tables: vec![table],
    };
    let mut sink = RecordingSink {
        db: DatabaseType::Sqlite,
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(8);
    run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    assert_eq!(sink.statements[0].0, r#"DELETE FROM "t""#);
}

#[tokio::test]
async fn cancel_before_second_batch() {
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 2350,
        cancel_flag: cancel_flag.clone(),
    });
    let services = services(executor);
    let plan = append_plan(DatabaseType::SqlServer, three_col_table("t"));
    let cancel = TransferCancel::default();
    let mut sink = CancelAfterFirstBatchSink {
        inner: RecordingSink {
            db: DatabaseType::SqlServer,
            ..Default::default()
        },
        cancel: cancel.clone(),
    };
    let (tx, _rx) = mpsc::channel(64);
    let outcome = run_transfer(&services, &test_profile(), &mut sink, &plan, tx, cancel).await;
    assert!(outcome.cancelled);
    assert!(sink.inner.tx_events.contains(&"rollback"));
    assert!(!sink.inner.tx_events.contains(&"commit"));
    assert!(cancel_flag.load(Ordering::SeqCst));
}

struct CancelAfterFirstBatchSink {
    inner: RecordingSink,
    cancel: TransferCancel,
}

#[async_trait]
impl sqlly_services::transfer::sink::TransferSink for CancelAfterFirstBatchSink {
    fn database_type(&self) -> DatabaseType {
        self.inner.database_type()
    }

    async fn execute(&mut self, sql: &str, params: &[sqlly_core::SqlParam]) -> StudioResult<u64> {
        let inserts_before = self
            .inner
            .statements
            .iter()
            .filter(|(_, param_count)| *param_count > 0)
            .count();
        let result =
            sqlly_services::transfer::sink::TransferSink::execute(&mut self.inner, sql, params)
                .await?;
        let inserts_after = self
            .inner
            .statements
            .iter()
            .filter(|(_, param_count)| *param_count > 0)
            .count();
        if inserts_before == 0 && inserts_after == 1 {
            self.cancel.request();
        }
        Ok(result)
    }

    async fn begin(&mut self) -> StudioResult<()> {
        self.inner.begin().await
    }

    async fn commit(&mut self) -> StudioResult<()> {
        self.inner.commit().await
    }

    async fn rollback(&mut self) -> StudioResult<()> {
        self.inner.rollback().await
    }
}

#[tokio::test]
async fn sink_failure_rolls_back() {
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 2350,
        cancel_flag: Arc::new(AtomicBool::new(false)),
    });
    let services = services(executor);
    let plan = append_plan(DatabaseType::SqlServer, three_col_table("t"));
    let mut sink = RecordingSink {
        db: DatabaseType::SqlServer,
        fail_on_statement: Some(2),
        ..Default::default()
    };
    let (tx, _rx) = mpsc::channel(64);
    let outcome = run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    assert!(outcome.error.is_some());
    assert!(sink.tx_events.contains(&"rollback"));
    assert!(!sink.tx_events.contains(&"commit"));
}

#[tokio::test]
async fn fk_tables_run_parent_first() {
    let batches = Arc::new(AtomicUsize::new(0));
    let cancel_flag = Arc::new(AtomicBool::new(false));
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 1,
        cancel_flag,
    });
    let services = services(executor);
    let parent = TransferTable {
        schema: Some("dbo".into()),
        name: "parent".into(),
        columns: vec![TransferColumn {
            name: "id".into(),
            source_type: "int".into(),
            nullable: false,
            is_identity: false,
            pk_ordinal: Some(1),
        }],
        depends_on: Vec::new(),
    };
    let child = TransferTable {
        schema: Some("dbo".into()),
        name: "child".into(),
        columns: vec![TransferColumn {
            name: "id".into(),
            source_type: "int".into(),
            nullable: false,
            is_identity: false,
            pk_ordinal: Some(1),
        }],
        depends_on: vec!["dbo.parent".into()],
    };
    let plan = TransferPlan {
        source: DatabaseType::SqlServer,
        target: DatabaseType::SqlServer,
        target_schema: Some("dbo".into()),
        mode: TransferMode::Append,
        casing: NameCasing::AsIs,
        tables: vec![child, parent],
    };
    let (tx, mut rx) = mpsc::channel(32);
    let mut sink = RecordingSink {
        db: DatabaseType::SqlServer,
        ..Default::default()
    };
    let _ = run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    let mut started = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let TransferProgress::TableStarted { table, .. } = event {
            started.push(table);
        }
    }
    assert_eq!(started, vec!["dbo.parent", "dbo.child"]);
    let _ = batches;
}

#[tokio::test]
async fn progress_emits_rows_and_finished() {
    let executor = Arc::new(FakeQueryExecutor {
        total_rows: 1500,
        cancel_flag: Arc::new(AtomicBool::new(false)),
    });
    let services = services(executor);
    let plan = append_plan(DatabaseType::SqlServer, three_col_table("t"));
    let mut sink = RecordingSink {
        db: DatabaseType::SqlServer,
        ..Default::default()
    };
    let (tx, mut rx) = mpsc::channel(64);
    run_transfer(
        &services,
        &test_profile(),
        &mut sink,
        &plan,
        tx,
        TransferCancel::default(),
    )
    .await;
    let mut row_events = 0usize;
    let mut finished = 0usize;
    while let Ok(event) = rx.try_recv() {
        match event {
            TransferProgress::Rows { .. } => row_events += 1,
            TransferProgress::Finished(_) => finished += 1,
            _ => {}
        }
    }
    assert!(row_events >= 2);
    assert_eq!(finished, 1);
}
