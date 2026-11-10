#![cfg(not(target_arch = "wasm32"))]

use std::io::Cursor;
use std::sync::Mutex;

use async_trait::async_trait;
use sqlly_core::StudioResult;
use sqlly_core::{CellValue, ConnectionProfile, SecretProvider, SecretRef};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_services::{
    ChunkReader, QueryEvent, QueryExecutor, ScriptRunOptions, TransactionMode, begin_sql,
    commit_sql, rollback_sql, run_script,
};
use tokio::sync::mpsc;

type FakeRoute = (&'static str, Result<Vec<Vec<CellValue>>, String>);

struct FakeQueries {
    routes: Vec<FakeRoute>,
    executed: Mutex<Vec<String>>,
}

impl FakeQueries {
    fn new(routes: Vec<FakeRoute>) -> Self {
        Self {
            routes,
            executed: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl QueryExecutor for FakeQueries {
    async fn execute(
        &self,
        _profile: &ConnectionProfile,
        sql: &str,
        _row_buffer: usize,
    ) -> StudioResult<sqlly_services::QueryHandle> {
        self.executed.lock().unwrap().push(sql.to_string());
        let (tx, rx) = mpsc::channel(16);
        let route = self
            .routes
            .iter()
            .find(|(needle, _)| sql.contains(needle))
            .map(|(_, result)| result.clone());
        match route {
            Some(Ok(rows)) => {
                tx.send(QueryEvent::Rows { index: 0, rows }).await.ok();
                tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
            }
            Some(Err(message)) => {
                tx.send(QueryEvent::Error { message }).await.ok();
            }
            None => {
                tx.send(QueryEvent::Completed { elapsed_ms: 1 }).await.ok();
            }
        }
        Ok(sqlly_services::QueryHandle {
            task_id: "t".into(),
            events: rx,
        })
    }

    async fn cancel(&self, _task_id: &str) -> StudioResult<()> {
        Ok(())
    }
}

fn profile() -> ConnectionProfile {
    ConnectionProfile::sql_login(
        "test-conn",
        "localhost",
        "sa",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: "k".into(),
        },
    )
}

#[tokio::test]
async fn deploy_wraps_in_begin_commit() {
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/admin_golden/sqlserver_deploy.sql"),
    )
    .unwrap();
    let total_bytes = script.len() as u64;
    let queries = FakeQueries::new(vec![]);
    let options = ScriptRunOptions {
        dialect: Dialect::SqlServer,
        on_error: sqlly_services::OnError::Stop,
        transaction: TransactionMode::Single,
        coalesce_inserts: false,
        session_key: "schema-deploy-test".into(),
        coalesce_max_rows: 500,
        coalesce_max_bytes: 1024 * 1024,
        definer: None,
    };
    let (progress_tx, _progress_rx) = mpsc::channel(8);
    let outcome = run_script(
        &queries,
        &profile(),
        ChunkReader::new(Cursor::new(script.into_bytes())),
        total_bytes,
        &options,
        progress_tx,
        sqlly_services::ScriptCancel::new(),
        |_| Box::pin(async { sqlly_services::ErrorDecision::Stop }),
    )
    .await
    .unwrap();
    let executed = queries.executed.lock().unwrap();
    assert!(
        executed
            .iter()
            .any(|sql| sql.contains(begin_sql(Dialect::SqlServer)))
    );
    assert!(
        executed
            .last()
            .map(|sql| sql.contains(commit_sql()))
            .unwrap_or(false)
    );
    assert_eq!(outcome.units_failed, 0);
}

#[tokio::test]
async fn deploy_failure_rolls_back_and_stops() {
    let script = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/admin_golden/sqlserver_deploy.sql"),
    )
    .unwrap();
    let total_bytes = script.len() as u64;
    let queries = FakeQueries::new(vec![("CREATE OR ALTER", Err("boom".into()))]);
    let options = ScriptRunOptions {
        dialect: Dialect::SqlServer,
        on_error: sqlly_services::OnError::Stop,
        transaction: TransactionMode::Single,
        coalesce_inserts: false,
        session_key: "schema-deploy-fail".into(),
        coalesce_max_rows: 500,
        coalesce_max_bytes: 1024 * 1024,
        definer: None,
    };
    let (progress_tx, _progress_rx) = mpsc::channel(8);
    let outcome = run_script(
        &queries,
        &profile(),
        ChunkReader::new(Cursor::new(script.into_bytes())),
        total_bytes,
        &options,
        progress_tx,
        sqlly_services::ScriptCancel::new(),
        |_| Box::pin(async { sqlly_services::ErrorDecision::Stop }),
    )
    .await
    .unwrap();
    let executed = queries.executed.lock().unwrap();
    assert!(executed.iter().any(|sql| sql.contains(rollback_sql())));
    assert!(outcome.units_failed > 0);
}
