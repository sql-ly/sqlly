//! `sqlly notebook` through the binary: exit codes, the JSON envelope and
//! help text. Hermetic: a temp SQLite database, connections file and
//! notebooks data directory.

use sqlly_core::{
    ConnectionEnvironment, ConnectionProfile, DatabaseType, SecretProvider, SecretRef,
};
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(args: &[&str], env: &[(&str, &Path)]) -> (i32, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_sqlly-cli"));
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn sqlly-cli");
    (
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

struct Setup {
    dir: tempfile::TempDir,
    notebook: PathBuf,
}

impl Setup {
    fn new(cells: serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("sales.db");
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch("CREATE TABLE t(id INTEGER); INSERT INTO t VALUES (1),(2),(2);")
            .unwrap();
        let mut profile = ConnectionProfile::sql_login(
            "Sales",
            db.to_string_lossy(),
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.name = "Sales".into();
        profile.database_type = DatabaseType::Sqlite;
        profile.environment = ConnectionEnvironment::Dev;
        std::fs::write(
            dir.path().join("connections.json"),
            serde_json::to_string(&serde_json::json!({"version": 1, "profiles": [profile]}))
                .unwrap(),
        )
        .unwrap();
        let notebook = dir.path().join("nb.sqllynb");
        std::fs::write(
            &notebook,
            serde_json::to_string(&serde_json::json!({
                "format": "sqlly.notebook", "format_version": 2, "notebook_id": "nb_cli",
                "title": "CLI", "connection_refs": [{"id": "primary", "label": "Sales"}],
                "cells": cells,
            }))
            .unwrap(),
        )
        .unwrap();
        Self { dir, notebook }
    }

    fn env(&self) -> Vec<(&'static str, PathBuf)> {
        vec![
            (
                "SQLLY_CONNECTIONS_FILE",
                self.dir.path().join("connections.json"),
            ),
            ("SQLLY_NOTEBOOK_DATA_DIR", self.dir.path().join("data")),
            ("HOME", self.dir.path().to_path_buf()),
        ]
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let env = self.env();
        let env: Vec<(&str, &Path)> = env.iter().map(|(k, v)| (*k, v.as_path())).collect();
        run(args, &env)
    }
}

#[test]
fn run_records_and_exit_codes_follow_checks() {
    let setup = Setup::new(serde_json::json!([
        {"id": "c_ids", "name": "ids", "kind": "sql", "source": "SELECT id FROM t"},
        {"id": "c_u", "name": "unique_ids", "kind": "assertion",
         "spec": {"input": "ids", "kind": "unique", "columns": ["id"]}}
    ]));
    let nb = setup.notebook.to_string_lossy().to_string();
    // Unmapped slot: refused by policy before anything runs.
    let (code, _, stderr) = setup.run(&["notebook", "run", &nb]);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("--connection primary="), "{stderr}");
    // Mapped but never trusted here: refused, with the way out.
    let (code, _, stderr) = setup.run(&["notebook", "run", &nb, "--connection", "primary=Sales"]);
    assert_eq!(code, 3, "{stderr}");
    assert!(stderr.contains("--trust"), "{stderr}");
    // Trusted for the run: the duplicate id fails the check (exit 4).
    let (code, stdout, stderr) = setup.run(&[
        "notebook",
        "run",
        &nb,
        "--connection",
        "primary=Sales",
        "--trust",
    ]);
    assert_eq!(code, 4, "{stdout}{stderr}");
    assert!(stdout.contains("1 failed"), "{stdout}");
    let (code, _, _) = setup.run(&[
        "notebook",
        "run",
        &nb,
        "--connection",
        "primary=Sales",
        "--fail-on",
        "error",
        "--trust",
    ]);
    assert_eq!(code, 0);
    // History, as JSON.
    let (code, stdout, _) = setup.run(&["notebook", "runs", &nb, "--format", "json"]);
    assert_eq!(code, 0);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["data"]["runs"].as_array().unwrap().len(), 2);
    let (code, stdout, stderr) = setup.run(&[
        "notebook",
        "runs",
        "compare",
        "latest~1",
        "latest",
        "--notebook",
        &nb,
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("->"), "{stdout}");
}

#[test]
fn validate_is_machine_readable_for_ci() {
    let setup = Setup::new(serde_json::json!([
        {"id": "c_a", "name": "a", "kind": "sql", "source": "SELECT * FROM {{b}}"},
        {"id": "c_b", "name": "b", "kind": "sql", "source": "SELECT * FROM {{a}}"}
    ]));
    let nb = setup.notebook.to_string_lossy().to_string();
    let (code, stdout, _) = setup.run(&["notebook", "validate", &nb, "--format", "json"]);
    assert_eq!(code, 1);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["data"]["valid"], false);
    let codes: Vec<&str> = value["data"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|d| d["code"].as_str())
        .collect();
    assert!(codes.contains(&"cycle"), "{codes:?}");
}

#[test]
fn diff_help_documents_the_git_recipe() {
    let (code, stdout, _) = run(&["notebook", "diff", "--help"], &[]);
    assert_eq!(code, 0);
    assert!(
        stdout.contains("textconv") && stdout.contains("difftool"),
        "{stdout}"
    );
    assert!(
        stdout.contains("\"sqlly-cli notebook cat\"")
            && stdout.contains("'sqlly-cli notebook diff ")
            && !stdout.contains(" sqlly notebook "),
        "the recipe must name the installed binary: {stdout}"
    );
}
