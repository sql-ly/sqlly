//! A structure dump of a real SQLite file must restore into an empty file
//! with the stock `sqlite3` shell. SQLite stores each `CREATE TABLE` without
//! its terminator, and the dump appends the table's index DDL right after it;
//! without a `;` between them `sqlite3` stopped at a syntax error (SQLly's own
//! runner is more forgiving, so it is the shell that proves the script).
//! A view whose source ends in a `-- comment` (SQLite keeps it verbatim) must
//! get its `;` on a fresh line, or the comment swallows it.
//! Runs hermetically on temp files; skips when `sqlite3` is not on PATH.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use sqlly_core::{CellValue, ConnectionProfile, DatabaseType, SecretProvider, SecretRef};
use sqlly_services::query::QueryEvent;
use sqlly_services::{DumpContent, DumpOptions, Services, run_database_dump};

fn sqlite_profile(path: &std::path::Path) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        "sqlite-dump",
        path.to_string_lossy().as_ref(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = DatabaseType::Sqlite;
    profile
}

async fn run(services: &Services, profile: &ConnectionProfile, sql: &str) -> Result<(), String> {
    let mut handle = services
        .queries
        .execute(profile, sql, 100)
        .await
        .map_err(|e| e.to_string())?;
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Error { message } => return Err(message),
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    Ok(())
}

async fn names(services: &Services, profile: &ConnectionProfile) -> Vec<String> {
    let mut handle = services
        .queries
        .execute(
            profile,
            "SELECT type || ':' || name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY 1",
            100,
        )
        .await
        .unwrap();
    let mut out = Vec::new();
    while let Some(event) = handle.events.recv().await {
        match event {
            QueryEvent::Rows { rows, .. } => {
                for row in rows {
                    if let Some(CellValue::String(s)) = row.into_iter().next() {
                        out.push(s);
                    }
                }
            }
            QueryEvent::Completed { .. } | QueryEvent::Cancelled => break,
            _ => {}
        }
    }
    out
}

#[tokio::test]
async fn sqlite_structure_dump_restores_into_an_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let source = sqlite_profile(&dir.path().join("source.db"));
    let services = Services::new();
    run(
        &services,
        &source,
        "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT); \
         CREATE INDEX idx_parent_name ON parent (name); \
         CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id), note TEXT); \
         CREATE INDEX idx_child_parent ON child (parent_id); \
         CREATE INDEX idx_child_note ON child (note); \
         CREATE VIEW child_names AS SELECT c.id, p.name FROM child c JOIN parent p ON p.id = c.parent_id; \
         CREATE VIEW parent_names AS SELECT name FROM parent -- stored verbatim, comment included\n",
    )
    .await
    .unwrap();

    let output = dir.path().join("dump.sql");
    let options = DumpOptions {
        content: DumpContent::StructureOnly,
        ..DumpOptions::default()
    };
    let database = dir.path().join("source.db").to_string_lossy().to_string();
    run_database_dump(
        &services,
        &source,
        &database,
        &output,
        &options,
        Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
    .await
    .unwrap();
    let script = std::fs::read_to_string(&output).unwrap();

    let restored = dir.path().join("restored.db");
    let shell = match std::process::Command::new("sqlite3")
        .arg("-bail")
        .arg(&restored)
        .stdin(std::fs::File::open(&output).unwrap())
        .output()
    {
        Ok(shell) => shell,
        Err(_) => {
            eprintln!("skipping: sqlite3 shell not on PATH");
            return;
        }
    };
    assert!(
        shell.status.success(),
        "sqlite3 restore failed: {}\n{script}",
        String::from_utf8_lossy(&shell.stderr)
    );
    let restored = sqlite_profile(&restored);
    assert_eq!(
        names(&services, &restored).await,
        names(&services, &source).await,
        "{script}"
    );
}
