//! Remote row editing end-to-end: `RemoteServices` → real `sqlly-cli engine`
//! subprocess → live SQL Server. Proves the full relay-shaped path — the wire
//! encoding, the engine's `row_edit` dispatch, and the SAME guarded
//! `DefaultRowEditingService` mutations direct connections use (provenance
//! discovery, primary-key guards, optimistic concurrency, conflict
//! detection).
//!
//! Gated on `SQLLY_SQL_*` env vars; skips silently when absent.

use std::path::PathBuf;
use std::process::Stdio;

use sqlly_core::{CellValue, ConnectionProfile, ResultColumn};
use sqlly_remote::engine_protocol::AsyncFramedTransport;
use sqlly_services::remote::RemoteServices;
use sqlly_services::row_editing::{
    DeleteRowOutcome, DeleteRowRequest, InsertRowRequest, RowEditOpenOutcome, RowEditOpenRequest,
    UpdateRowOutcome, UpdateRowRequest,
};
use sqlly_testkit::LiveSqlConfig;
use tokio::io::Join;
use tokio::process::{ChildStdin, ChildStdout, Command};

type EngineTransport = AsyncFramedTransport<Join<ChildStdout, ChildStdin>>;

struct Harness {
    _child: tokio::process::Child,
    remote: RemoteServices<EngineTransport>,
}

fn locate_sqlly_cli() -> PathBuf {
    if let Ok(p) = std::env::var("SQLLY_CLI_BIN")
        && !p.trim().is_empty()
    {
        return PathBuf::from(p);
    }
    let mut dir = std::env::current_exe().expect("current_exe");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let candidate = dir.join(format!("sqlly-cli{}", std::env::consts::EXE_SUFFIX));
    if candidate.exists() {
        return candidate;
    }
    let cargo = option_env!("CARGO").unwrap_or("cargo");
    let status = std::process::Command::new(cargo)
        .args(["build", "-p", "sqlly-cli"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .status()
        .expect("run `cargo build -p sqlly-cli`");
    assert!(status.success(), "`cargo build -p sqlly-cli` failed");
    candidate
}

fn spawn_harness() -> Harness {
    let bin = locate_sqlly_cli();
    let mut child = Command::new(bin)
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine");
    let stdin = child.stdin.take().expect("child stdin");
    let stdout = child.stdout.take().expect("child stdout");
    let transport = AsyncFramedTransport::new(tokio::io::join(stdout, stdin));
    Harness {
        _child: child,
        remote: RemoteServices::from_transport(transport),
    }
}

fn sql_profile() -> Option<ConnectionProfile> {
    let cfg = LiveSqlConfig::load()?;
    Some(cfg.to_profile("row-edit-live"))
}

async fn run_sql(remote: &RemoteServices<EngineTransport>, profile: &ConnectionProfile, sql: &str) {
    use sqlly_services::QueryEvent;
    let handle = remote
        .services()
        .queries
        .execute(profile, sql, 64)
        .await
        .unwrap_or_else(|e| panic!("execute {sql}: {e}"));
    let mut events = handle.events;
    while let Some(event) = events.recv().await {
        match event {
            QueryEvent::Error { message } => panic!("query failed: {sql}: {message}"),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_row_editing_full_lifecycle_live() {
    let Some(profile) = sql_profile() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let harness = spawn_harness();
    let remote = &harness.remote;
    let row_editing = &remote.services().row_editing;

    run_sql(
        remote,
        &profile,
        "IF OBJECT_ID('dbo.sqlly_rowedit', 'U') IS NOT NULL DROP TABLE dbo.sqlly_rowedit",
    )
    .await;
    run_sql(
        remote,
        &profile,
        "CREATE TABLE dbo.sqlly_rowedit (id INT PRIMARY KEY, label NVARCHAR(100) NOT NULL)",
    )
    .await;
    run_sql(
        remote,
        &profile,
        "INSERT INTO dbo.sqlly_rowedit (id, label) VALUES (1, 'original')",
    )
    .await;

    // Open an edit session on the row exactly as the grid does: provenance
    // comes from the engine, keys included.
    let open = row_editing
        .open_session(
            &profile,
            RowEditOpenRequest {
                sql: "SELECT id, label FROM dbo.sqlly_rowedit".into(),
                result_set_index: 0,
                columns: vec![
                    ResultColumn {
                        name: "id".into(),
                        data_type: "int".into(),
                        nullable: false,
                    },
                    ResultColumn {
                        name: "label".into(),
                        data_type: "nvarchar".into(),
                        nullable: false,
                    },
                ],
                row: vec![CellValue::I64(1), CellValue::String("original".into())],
            },
        )
        .await
        .expect("open_session round trip");
    let session = match open {
        RowEditOpenOutcome::Editable(session) => session,
        RowEditOpenOutcome::NotEditable { reason, .. } => panic!("row not editable: {reason}"),
    };
    assert_eq!(session.key_columns, vec!["id".to_string()]);
    let target = session.target.clone();
    let key = session.key_values();

    // Guarded update with optimistic originals.
    let updated = row_editing
        .update_row(
            &profile,
            UpdateRowRequest {
                target: target.clone(),
                key: key.clone(),
                rowversion: None,
                changes: vec![("label".into(), CellValue::String("edited".into()))],
                original_values: vec![("label".into(), CellValue::String("original".into()))],
                expressions: Vec::new(),
            },
        )
        .await
        .expect("update_row round trip");
    match updated {
        UpdateRowOutcome::Updated { columns, row } => {
            let label_at = columns.iter().position(|c| c == "label").unwrap();
            assert_eq!(row[label_at], CellValue::String("edited".into()));
        }
        other => panic!("expected Updated, got {other:?}"),
    }

    // A second update with STALE originals must report a conflict, not write.
    let conflict = row_editing
        .update_row(
            &profile,
            UpdateRowRequest {
                target: target.clone(),
                key: key.clone(),
                rowversion: None,
                changes: vec![("label".into(), CellValue::String("stale write".into()))],
                original_values: vec![("label".into(), CellValue::String("original".into()))],
                expressions: Vec::new(),
            },
        )
        .await
        .expect("conflicting update round trip");
    assert!(
        matches!(conflict, UpdateRowOutcome::Conflict { .. }),
        "stale originals must conflict: {conflict:?}"
    );

    // Insert a sibling row and delete it again, both guarded.
    let inserted = row_editing
        .insert_row(
            &profile,
            InsertRowRequest {
                target: target.clone(),
                values: vec![
                    ("id".into(), CellValue::I64(2)),
                    ("label".into(), CellValue::String("second".into())),
                ],
                expressions: Vec::new(),
            },
        )
        .await
        .expect("insert_row round trip");
    let sqlly_services::row_editing::InsertRowOutcome::Inserted { columns, row } = inserted;
    let id_at = columns.iter().position(|c| c == "id").unwrap();
    assert_eq!(row[id_at], CellValue::I64(2));

    let fetched = row_editing
        .fetch_row(&profile, &target, &[("id".into(), CellValue::I64(2))])
        .await
        .expect("fetch_row round trip")
        .expect("row exists");
    assert!(fetched.1.contains(&CellValue::String("second".into())));

    let deleted = row_editing
        .delete_row(
            &profile,
            DeleteRowRequest {
                target: target.clone(),
                key: vec![("id".into(), CellValue::I64(2))],
                rowversion: None,
                original_values: vec![
                    ("id".into(), CellValue::I64(2)),
                    ("label".into(), CellValue::String("second".into())),
                ],
            },
        )
        .await
        .expect("delete_row round trip");
    assert!(matches!(deleted, DeleteRowOutcome::Deleted), "{deleted:?}");

    let gone = row_editing
        .fetch_row(&profile, &target, &[("id".into(), CellValue::I64(2))])
        .await
        .expect("fetch_row after delete");
    assert!(gone.is_none(), "deleted row must be gone");

    run_sql(remote, &profile, "DROP TABLE dbo.sqlly_rowedit").await;
}
