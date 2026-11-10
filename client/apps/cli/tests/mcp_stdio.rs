//! Integration tests for `sqlly-cli mcp` stdio protocol.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rusqlite::Connection;
use uuid::Uuid;

fn spawn_mcp(
    home: &tempfile::TempDir,
    policy_json: Option<&str>,
) -> (std::process::Child, mpsc::Receiver<String>) {
    let config = home.path().join("config").join("SQLLY");
    let data = home.path().join("data").join("SQLLY");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(data.join("mcp-proposals")).unwrap();

    if let Some(policy) = policy_json {
        std::fs::write(data.join("mcp-policy.json"), policy).unwrap();
    }

    let proposals_dir = data.join("mcp-proposals");
    std::fs::create_dir_all(&proposals_dir).unwrap();

    let connections_path = config.join("connections.json");
    let policy_path = data.join("mcp-policy.json");

    let mut child = Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("mcp")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join("config"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("SQLLY_MCP_STORE_PATH", &connections_path)
        .env("SQLLY_MCP_POLICY_PATH", &policy_path)
        .env("SQLLY_MCP_PROPOSALS_DIR", &proposals_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn mcp");

    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    (child, rx)
}

fn write_json(child: &mut std::process::Child, value: serde_json::Value) {
    let stdin = child.stdin.as_mut().expect("stdin");
    writeln!(stdin, "{value}").unwrap();
    stdin.flush().unwrap();
}

fn recv_for(
    child: &mut std::process::Child,
    rx: &mpsc::Receiver<String>,
    id: i64,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(200))
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(&line)
            && value.get("id") == Some(&serde_json::json!(id))
        {
            return value;
        }
        if child.try_wait().ok().flatten().is_some() {
            break;
        }
    }
    panic!("timed out waiting for id={id}");
}

fn handshake(child: &mut std::process::Child, rx: &mpsc::Receiver<String>) {
    write_json(
        child,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" }
            }
        }),
    );
    let _ = recv_for(child, rx, 1);
    write_json(
        child,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
}

fn normalize_tools_list(mut value: serde_json::Value) -> serde_json::Value {
    let tools = value
        .pointer_mut("/result/tools")
        .and_then(|v| v.as_array_mut())
        .expect("tools array");
    tools.sort_by_key(|tool| {
        tool.get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string()
    });
    value
}

fn fixture_store(sqlite_path: &std::path::Path) -> String {
    let dev_id = Uuid::parse_str("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa").unwrap();
    let prod_id = Uuid::parse_str("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb").unwrap();
    serde_json::json!({
        "version": 1,
        "profiles": [
            {
                "id": dev_id.to_string(),
                "name": "dev",
                "server": sqlite_path.to_string_lossy(),
                "port": 0,
                "database": sqlite_path.to_string_lossy(),
                "database_type": "Sqlite",
                "environment": "DEV",
                "authentication": { "SqlLogin": { "username": "", "secret_ref": { "provider": "InMemory", "key": "" } } },
                "encrypt": false,
                "trust_server_certificate": true,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            },
            {
                "id": prod_id.to_string(),
                "name": "prod",
                "server": "prod.example.com",
                "port": 1433,
                "database": "app",
                "environment": "PRODUCTION",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            }
        ],
        "environments": []
    })
    .to_string()
}

fn write_connections(home: &tempfile::TempDir, sqlite_path: &std::path::Path) {
    let dir = home.path().join("config").join("SQLLY");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("connections.json");
    std::fs::write(path, fixture_store(sqlite_path)).unwrap();
}

fn call_tool(
    child: &mut std::process::Child,
    rx: &mpsc::Receiver<String>,
    id: i64,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    write_json(
        child,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }),
    );
    recv_for(child, rx, id)
}

#[test]
fn tools_list_matches_snapshot() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    Connection::open(&sqlite)
        .unwrap()
        .execute_batch("CREATE TABLE t(a INT);")
        .unwrap();
    write_connections(&home, &sqlite);

    let (mut child, rx) = spawn_mcp(&home, None);
    handshake(&mut child, &rx);
    write_json(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let response = normalize_tools_list(recv_for(&mut child, &rx, 2));
    let snapshot_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/mcp_tools_list.snapshot.json");
    if std::env::var("UPDATE_SNAPSHOTS").is_ok() {
        std::fs::create_dir_all(snapshot_path.parent().unwrap()).unwrap();
        std::fs::write(
            &snapshot_path,
            serde_json::to_string_pretty(&response).unwrap(),
        )
        .unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&snapshot_path).unwrap()).unwrap();
    assert_eq!(response, expected);
    let _ = child.kill();
}

#[test]
fn readonly_query_over_stdio_returns_rows() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    let conn = Connection::open(&sqlite).unwrap();
    conn.execute_batch("CREATE TABLE t(a INT); INSERT INTO t VALUES (1),(2),(3);")
        .unwrap();
    write_connections(&home, &sqlite);

    let (mut child, rx) = spawn_mcp(&home, None);
    handshake(&mut child, &rx);
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "run_readonly_query",
        serde_json::json!({
            "connection": "dev",
            "sql": "SELECT a FROM t ORDER BY a",
            "limit": 2
        }),
    );
    assert_eq!(response["result"]["isError"], false);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains('1'));
    assert!(text.contains('2'));
    assert!(text.contains("truncated_rows=true"));
    let _ = child.kill();
}

#[test]
fn delete_over_stdio_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    let conn = Connection::open(&sqlite).unwrap();
    conn.execute_batch("CREATE TABLE t(a INT); INSERT INTO t VALUES (1);")
        .unwrap();
    write_connections(&home, &sqlite);

    let (mut child, rx) = spawn_mcp(&home, None);
    handshake(&mut child, &rx);
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "run_readonly_query",
        serde_json::json!({
            "connection": "dev",
            "sql": "DELETE FROM t"
        }),
    );
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("refused"));
    let check = call_tool(
        &mut child,
        &rx,
        4,
        "run_readonly_query",
        serde_json::json!({
            "connection": "dev",
            "sql": "SELECT COUNT(*) AS c FROM t"
        }),
    );
    let body = check["result"]["content"][0]["text"].as_str().unwrap();
    assert!(body.contains('1'));
    let _ = child.kill();
}

#[test]
fn production_connection_refused_over_stdio() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    Connection::open(&sqlite)
        .unwrap()
        .execute_batch("CREATE TABLE t(a INT);")
        .unwrap();
    write_connections(&home, &sqlite);

    let (mut child, rx) = spawn_mcp(&home, None);
    handshake(&mut child, &rx);
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "list_tables",
        serde_json::json!({ "connection": "prod", "database": "app" }),
    );
    assert_eq!(response["result"]["isError"], true);
    let text = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("Production"));
    let _ = child.kill();
}

#[test]
fn propose_write_in_read_only_policy_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    write_connections(&home, &sqlite);
    let policy = r#"{"version":1,"mode":"read_only","connections":"all"}"#;

    let (mut child, rx) = spawn_mcp(&home, Some(policy));
    handshake(&mut child, &rx);
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "propose_write",
        serde_json::json!({
            "connection": "dev",
            "database": "main",
            "sql": "UPDATE t SET a = 2"
        }),
    );
    assert_eq!(response["result"]["isError"], true);
    let proposals = home.path().join("data").join("SQLLY").join("mcp-proposals");
    assert!(std::fs::read_dir(proposals).unwrap().next().is_none());
    let _ = child.kill();
}

#[test]
fn propose_write_in_read_write_policy_writes_pending_file() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    write_connections(&home, &sqlite);
    let policy = r#"{"version":1,"mode":"read_write_confirm","connections":"all"}"#;

    let (mut child, rx) = spawn_mcp(&home, Some(policy));
    handshake(&mut child, &rx);
    let sql = "UPDATE t SET a = 2";
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "propose_write",
        serde_json::json!({
            "connection": "dev",
            "database": "main",
            "sql": sql
        }),
    );
    assert_eq!(response["result"]["isError"], false);
    let proposals_dir = home.path().join("data").join("SQLLY").join("mcp-proposals");
    let file = std::fs::read_dir(&proposals_dir)
        .unwrap()
        .next()
        .expect("proposal file")
        .unwrap();
    let proposal: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(file.path()).unwrap()).unwrap();
    assert_eq!(proposal["status"], "pending");
    assert_eq!(proposal["sql"], sql);
    let _ = child.kill();
}

#[test]
fn env_cannot_widen_policy() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    write_connections(&home, &sqlite);
    let policy = r#"{"version":1,"mode":"read_only","connections":"all"}"#;

    let config_root = home.path().join("config");
    let data_root = home.path().join("data");
    std::fs::create_dir_all(config_root.join("SQLLY")).unwrap();
    std::fs::create_dir_all(data_root.join("SQLLY").join("mcp-proposals")).unwrap();
    std::fs::write(data_root.join("SQLLY").join("mcp-policy.json"), policy).unwrap();
    std::fs::write(
        config_root.join("SQLLY").join("connections.json"),
        fixture_store(&sqlite),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("mcp")
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config_root)
        .env("XDG_DATA_HOME", &data_root)
        .env(
            "SQLLY_MCP_STORE_PATH",
            config_root.join("SQLLY").join("connections.json"),
        )
        .env(
            "SQLLY_MCP_POLICY_PATH",
            data_root.join("SQLLY").join("mcp-policy.json"),
        )
        .env(
            "SQLLY_MCP_PROPOSALS_DIR",
            data_root.join("SQLLY").join("mcp-proposals"),
        )
        .env("SQLLY_MCP_MODE", "read_write_confirm")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    handshake(&mut child, &rx);
    let response = call_tool(
        &mut child,
        &rx,
        3,
        "propose_write",
        serde_json::json!({
            "connection": "dev",
            "sql": "UPDATE t SET a = 1"
        }),
    );
    assert_eq!(response["result"]["isError"], true);
    let _ = child.kill();
}

#[test]
fn stdout_is_protocol_only() {
    let home = tempfile::tempdir().unwrap();
    let sqlite = home.path().join("test.sqlite");
    write_connections(&home, &sqlite);
    let (mut child, rx) = spawn_mcp(&home, None);
    handshake(&mut child, &rx);
    write_json(
        &mut child,
        serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    let _ = recv_for(&mut child, &rx, 2);
    drop(rx);
    let _ = child.kill();
}
