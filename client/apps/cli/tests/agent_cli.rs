use sqlly_core::{
    ConnectionEnvironment, ConnectionProfile, DatabaseType, SecretProvider, SecretRef,
};
use sqlly_testkit::golden::GoldenTest;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_sqlly-cli"))
}

fn run(args: &[&str], env: &[(&str, &str)]) -> (i32, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = cmd.output().expect("spawn sqlly-cli");
    let status = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (status, stdout, stderr)
}

fn normalize_help(text: &str) -> String {
    // On Windows the invoked binary is `sqlly-cli.exe`, so clap prints that in
    // the usage line; strip the suffix so the snapshot stays platform-neutral.
    text.replace("sqlly-cli.exe", "sqlly-cli")
        .lines()
        .map(|line| {
            if line.starts_with("sqlly ") && line.contains(char::is_numeric) {
                "sqlly <VERSION>".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn write_store(path: &Path, profiles: &[ConnectionProfile]) {
    let json = serde_json::json!({
        "version": 1,
        "profiles": profiles,
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_string_pretty(&json).unwrap()).unwrap();
}

fn sqlite_profile(name: &str, path: &Path, env: ConnectionEnvironment) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        name,
        path.to_string_lossy(),
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.name = name.to_string();
    profile.database_type = DatabaseType::Sqlite;
    profile.environment = env;
    profile
}

fn seed_sqlite_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE t(a INTEGER, b TEXT);
         INSERT INTO t VALUES (1, NULL), (2, 'héllo');",
    )
    .unwrap();
}

#[test]
fn help_matches_snapshot() {
    let (_, stdout, _) = run(&["--help"], &[]);
    let normalized = normalize_help(&stdout);
    let snapshot = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots/help.txt");
    if std::env::var("UPDATE_SNAPSHOTS").as_deref() == Ok("1") {
        GoldenTest::update(&snapshot, &normalized).unwrap();
    }
    assert!(
        GoldenTest::compare(&normalized, &snapshot).unwrap(),
        "help text changed; run UPDATE_SNAPSHOTS=1 cargo test -p sqlly-cli --locked --test agent_cli help_matches_snapshot"
    );
}

#[test]
fn usage_error_exits_2() {
    let (status, _, _) = run(&["query"], &[]);
    assert_eq!(status, 2);
}

#[test]
fn unknown_connection_exits_1_with_json_envelope() {
    let store = std::env::temp_dir().join(format!(
        "sqlly-agent-store-{}-{}.json",
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    write_store(&store, &[]);
    let store_path = store.to_string_lossy().to_string();
    let env = [("SQLLY_CONNECTIONS_FILE", store_path.as_str())];
    let (status, stdout, _) = run(&["--json", "query", "nope", "--sql", "select 1"], &env);
    let _ = fs::remove_file(store);
    assert_eq!(status, 1);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not_found");
}

#[test]
fn write_without_allow_writes_exits_3() {
    let dir = std::env::temp_dir().join(format!("sqlly-agent-policy-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("dev.sqlite");
    seed_sqlite_db(&db);
    let store = dir.join("connections.json");
    write_store(
        &store,
        &[sqlite_profile("dev", &db, ConnectionEnvironment::Dev)],
    );
    let store_path = store.to_string_lossy().to_string();
    let env = [("SQLLY_CONNECTIONS_FILE", store_path.as_str())];
    let (status, stdout, _) = run(&["--json", "query", "dev", "--sql", "delete from t"], &env);
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(status, 3);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["error"]["code"], "policy_refused");
}

#[test]
fn write_on_production_refused_even_with_allow_writes() {
    let dir = std::env::temp_dir().join(format!("sqlly-agent-prod-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("prod.sqlite");
    seed_sqlite_db(&db);
    let store = dir.join("connections.json");
    write_store(
        &store,
        &[sqlite_profile(
            "dev",
            &db,
            ConnectionEnvironment::Production,
        )],
    );
    let store_path = store.to_string_lossy().to_string();
    let env = [("SQLLY_CONNECTIONS_FILE", store_path.as_str())];
    let (status, stdout, _) = run(
        &[
            "--json",
            "query",
            "dev",
            "--sql",
            "delete from t",
            "--allow-writes",
        ],
        &env,
    );
    let _ = fs::remove_dir_all(&dir);
    assert_eq!(status, 3);
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["error"]["code"], "policy_refused");
}

#[test]
fn select_on_sqlite_returns_rows_in_each_format() {
    let dir = std::env::temp_dir().join(format!("sqlly-agent-formats-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("dev.sqlite");
    seed_sqlite_db(&db);
    let store = dir.join("connections.json");
    write_store(
        &store,
        &[sqlite_profile("dev", &db, ConnectionEnvironment::Dev)],
    );
    let store_path = store.to_string_lossy().to_string();
    let env = [("SQLLY_CONNECTIONS_FILE", store_path.as_str())];

    for format in ["table", "csv", "json", "jsonl", "md"] {
        let (status, stdout, _) = run(
            &[
                "query",
                "dev",
                "--sql",
                "select * from t order by a",
                "--format",
                format,
            ],
            &env,
        );
        assert_eq!(status, 0, "format {format} failed");
        match format {
            "table" => {
                assert!(stdout.contains("NULL"));
                assert!(stdout.contains("héllo"));
            }
            "csv" => assert!(stdout.contains("1,")),
            "json" => {
                let value: serde_json::Value = serde_json::from_str(
                    stdout
                        .lines()
                        .find(|l| l.trim_start().starts_with('{'))
                        .unwrap_or(stdout.trim()),
                )
                .unwrap_or_else(|_| serde_json::from_str(stdout.trim()).expect("json output"));
                let rows = &value["result_sets"][0]["rows"];
                assert_eq!(rows[0][1], serde_json::Value::Null);
            }
            "jsonl" => assert_eq!(stdout.lines().filter(|l| !l.is_empty()).count(), 2),
            "md" => assert!(stdout.lines().any(|l| l.starts_with('|'))),
            _ => {}
        }
    }

    let _ = fs::remove_dir_all(&dir);
}

/// `sqlly query` writes values out, so a 20 KiB cell reaches stdout whole —
/// streamed (csv) and enveloped (json) alike — with no truncation warning.
#[test]
fn query_writes_a_20_kib_cell_whole() {
    let dir = std::env::temp_dir().join(format!("sqlly-agent-bigcell-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&dir).unwrap();
    let db = dir.join("dev.sqlite");
    seed_sqlite_db(&db);
    let store = dir.join("connections.json");
    write_store(
        &store,
        &[sqlite_profile("dev", &db, ConnectionEnvironment::Dev)],
    );
    let store_path = store.to_string_lossy().to_string();
    let env = [("SQLLY_CONNECTIONS_FILE", store_path.as_str())];
    let sql = "select replace(hex(zeroblob(10240)), '00', 'ab') as body";
    let whole = "ab".repeat(10 * 1024);
    for format in ["csv", "json"] {
        let (status, stdout, stderr) =
            run(&["query", "dev", "--sql", sql, "--format", format], &env);
        assert_eq!(status, 0, "format {format} failed: {stderr}");
        assert!(stdout.contains(&whole), "format {format}: the cell was cut");
        assert!(!stderr.contains("cut to an 8 KiB preview"), "{stderr}");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn legacy_commands_still_run_but_hidden() {
    let (status, stdout, _) = run(&["templates", "list"], &[]);
    assert_eq!(status, 0);
    assert!(!stdout.is_empty());
    let (help_status, help_stdout, _) = run(&["--help"], &[]);
    assert_eq!(help_status, 0);
    assert!(!help_stdout.contains("templates"));
}
