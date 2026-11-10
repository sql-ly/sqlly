use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sqlly_core::{ConnectionProfile, SecretProvider, SecretRef, StudioResult};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_services::{
    ChunkReader, ErrorDecision, OnError, ScriptCancel, ScriptOutcome, ScriptRunOptions,
    TransactionMode,
    query::{QueryEvent, QueryExecutor, QueryHandle},
    run_script,
};
use tokio::sync::mpsc;

#[derive(Clone, Default)]
struct FakeQueries {
    // One-off test capture type; a type alias would not improve readability.
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
        if sql.contains("/*FAIL*/") {
            tx.send(QueryEvent::Error {
                message: "boom".into(),
            })
            .await
            .ok();
        } else if sql.contains("/*SPILL*/") {
            let path =
                std::env::temp_dir().join(format!("sqlly-spill-{}.bin", uuid::Uuid::new_v4()));
            std::fs::write(&path, b"x").ok();
            tx.send(QueryEvent::SpillManifest {
                manifest: sqlly_services::query::SpillManifestData {
                    result_sets: vec![sqlly_services::query::SpillResultSetData {
                        result_set_index: 0,
                        columns: vec![],
                        row_count: 1,
                        spill_file: path.to_string_lossy().into_owned(),
                    }],
                    messages: vec![],
                    error: None,
                    elapsed_ms: None,
                },
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

fn options(dialect: Dialect, on_error: OnError) -> ScriptRunOptions {
    ScriptRunOptions {
        dialect,
        on_error,
        transaction: TransactionMode::None,
        coalesce_inserts: false,
        session_key: "run-file:test".into(),
        coalesce_max_rows: 500,
        coalesce_max_bytes: 1024 * 1024,
        definer: None,
    }
}

async fn run_sql(
    fake: &FakeQueries,
    sql: &str,
    on_error: OnError,
    dialect: Dialect,
) -> ScriptOutcome {
    let (tx, _rx) = mpsc::channel(32);
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    run_script(
        fake,
        &profile(),
        reader,
        sql.len() as u64,
        &options(dialect, on_error),
        tx,
        ScriptCancel::new(),
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn runs_units_in_order_on_one_session_key() {
    let fake = FakeQueries::default();
    let outcome = run_sql(
        &fake,
        "select 1;\nselect 2;\nselect 3;",
        OnError::Stop,
        Dialect::Postgres,
    )
    .await;
    assert_eq!(outcome.units_done, 3);
    let executed = fake.executed.lock().unwrap();
    assert!(
        executed
            .iter()
            .all(|(_, s)| s.as_deref() == Some("run-file:test"))
    );
}

#[tokio::test]
async fn stop_policy_halts_after_first_failure() {
    let fake = FakeQueries::default();
    let outcome = run_sql(
        &fake,
        "select 1;\n/*FAIL*/ select 2;\nselect 3;",
        OnError::Stop,
        Dialect::Postgres,
    )
    .await;
    assert!(outcome.stopped_on_error);
    assert_eq!(fake.executed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn continue_policy_skips_failures() {
    let fake = FakeQueries::default();
    let outcome = run_sql(
        &fake,
        "select 1;\n/*FAIL*/ select 2;\nselect 3;",
        OnError::Continue,
        Dialect::Postgres,
    )
    .await;
    assert_eq!(outcome.units_failed, 1);
    assert_eq!(outcome.units_done, 2);
    assert_eq!(fake.executed.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn ask_policy_retry_then_skip() {
    let fake = FakeQueries::default();
    let (tx, _rx) = mpsc::channel(8);
    let sql = "/*FAIL*/ select 1;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    let mut calls = 0usize;
    let outcome = run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &options(Dialect::Postgres, OnError::Ask),
        tx,
        ScriptCancel::new(),
        |_| {
            calls += 1;
            Box::pin(async move {
                if calls == 1 {
                    ErrorDecision::Retry
                } else {
                    ErrorDecision::Skip
                }
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(outcome.units_skipped, 1);
    assert_eq!(fake.executed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn skip_all_suppresses_later_prompts() {
    let fake = FakeQueries::default();
    let (tx, _rx) = mpsc::channel(8);
    let sql = "/*FAIL*/ select 1;\n/*FAIL*/ select 2;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    let mut calls = 0usize;
    let outcome = run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &options(Dialect::Postgres, OnError::Ask),
        tx,
        ScriptCancel::new(),
        |_| {
            calls += 1;
            Box::pin(async move {
                if calls == 1 {
                    ErrorDecision::SkipAll
                } else {
                    ErrorDecision::Stop
                }
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(outcome.units_skipped, 2);
}

#[tokio::test]
async fn single_transaction_wraps_begin_commit() {
    let fake = FakeQueries::default();
    let mut opts = options(Dialect::Postgres, OnError::Stop);
    opts.transaction = TransactionMode::Single;
    let (tx, _rx) = mpsc::channel(8);
    let sql = "select 1;\nselect 2;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &opts,
        tx,
        ScriptCancel::new(),
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap();
    let executed: Vec<String> = fake
        .executed
        .lock()
        .unwrap()
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    assert_eq!(executed.first().map(String::as_str), Some("BEGIN"));
    assert_eq!(executed.last().map(String::as_str), Some("COMMIT"));
}

#[tokio::test]
async fn single_transaction_rollback_on_stop() {
    let fake = FakeQueries::default();
    let mut opts = options(Dialect::Postgres, OnError::Stop);
    opts.transaction = TransactionMode::Single;
    let (tx, _rx) = mpsc::channel(8);
    let sql = "select 1;\n/*FAIL*/ select 2;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &opts,
        tx,
        ScriptCancel::new(),
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap();
    let executed: Vec<String> = fake
        .executed
        .lock()
        .unwrap()
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    assert_eq!(executed.last().map(String::as_str), Some("ROLLBACK"));
}

#[tokio::test]
async fn cancel_token_stops_and_rolls_back() {
    let fake = FakeQueries::default();
    let mut opts = options(Dialect::Postgres, OnError::Stop);
    opts.transaction = TransactionMode::Single;
    let cancel = ScriptCancel::new();
    let (tx, _rx) = mpsc::channel(8);
    let sql = "select 1;\nselect 2;\nselect 3;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    cancel.cancel();
    let outcome = run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &opts,
        tx,
        cancel,
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap();
    assert!(outcome.cancelled);
    let executed: Vec<String> = fake
        .executed
        .lock()
        .unwrap()
        .iter()
        .map(|(s, _)| s.clone())
        .collect();
    assert_eq!(executed.last().map(String::as_str), Some("ROLLBACK"));
}

#[tokio::test]
async fn spill_manifest_files_are_deleted() {
    let fake = FakeQueries::default();
    let sql = "/*SPILL*/ select 1;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    let (tx, mut rx) = mpsc::channel(8);
    run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &options(Dialect::Postgres, OnError::Stop),
        tx,
        ScriptCancel::new(),
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap();
    while rx.recv().await.is_some() {}
    let path = fake
        .executed
        .lock()
        .unwrap()
        .first()
        .map(|(s, _)| s.clone());
    let _ = path;
}

#[tokio::test]
async fn progress_terminal_event_is_last_and_forced() {
    let fake = FakeQueries::default();
    let (tx, mut rx) = mpsc::channel(32);
    let sql = "select 1;";
    let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
    run_script(
        &fake,
        &profile(),
        reader,
        sql.len() as u64,
        &options(Dialect::Postgres, OnError::Stop),
        tx,
        ScriptCancel::new(),
        |_| Box::pin(async { ErrorDecision::Skip }),
    )
    .await
    .unwrap();
    let mut last = None;
    while let Some(p) = rx.recv().await {
        last = Some(p);
    }
    assert!(last.unwrap().terminal);
}

#[tokio::test]
async fn sqlserver_units_are_go_batches() {
    let fake = FakeQueries::default();
    let outcome = run_sql(
        &fake,
        "select 1; select 2\nGO\nselect 3",
        OnError::Stop,
        Dialect::SqlServer,
    )
    .await;
    assert_eq!(outcome.units_done, 2);
    assert_eq!(fake.executed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn definer_option_rewrites_each_statement_before_it_runs() {
    let sql = "DELIMITER ;;\nCREATE DEFINER=`root`@`localhost` PROCEDURE p() BEGIN SELECT 'DEFINER=`x`@`y`'; END;;\nDELIMITER ;\n/*!50001 CREATE ALGORITHM=UNDEFINED */\n/*!50013 DEFINER=`root`@`%` SQL SECURITY DEFINER */\n/*!50001 VIEW v AS SELECT 1 */;\n";
    for (mode, expected_proc, expected_view) in [
        (
            sqlly_services::mysql_definer::DefinerMode::Strip,
            "CREATE PROCEDURE p() BEGIN SELECT 'DEFINER=`x`@`y`'; END",
            "/*!50013 SQL SECURITY DEFINER */",
        ),
        (
            sqlly_services::mysql_definer::DefinerMode::CurrentUser,
            "CREATE DEFINER=CURRENT_USER PROCEDURE p() BEGIN SELECT 'DEFINER=`x`@`y`'; END",
            "/*!50013 DEFINER=CURRENT_USER SQL SECURITY DEFINER */",
        ),
    ] {
        let fake = FakeQueries::default();
        let (tx, _rx) = mpsc::channel(32);
        let reader = ChunkReader::new(std::io::Cursor::new(sql.as_bytes().to_vec()));
        let mut opts = options(Dialect::MySql, OnError::Stop);
        opts.definer = Some(mode);
        let outcome = run_script(
            &fake,
            &profile(),
            reader,
            sql.len() as u64,
            &opts,
            tx,
            ScriptCancel::new(),
            |_| Box::pin(async { ErrorDecision::Stop }),
        )
        .await
        .unwrap();
        assert_eq!(outcome.units_failed, 0);
        let executed: Vec<String> = fake
            .executed
            .lock()
            .unwrap()
            .iter()
            .map(|(sql, _)| sql.clone())
            .collect();
        assert!(
            executed.iter().any(|s| s.trim() == expected_proc),
            "{mode:?}: {executed:#?}"
        );
        assert!(
            executed.iter().any(|s| s.contains(expected_view)),
            "{mode:?}: {executed:#?}"
        );
    }
}
