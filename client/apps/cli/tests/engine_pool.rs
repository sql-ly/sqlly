//! Live, end-to-end tests for the engine's connection pool.
//!
//! These drive the real `sqlly-cli engine` subprocess over its stdin/stdout
//! framing against a live SQL Server, and verify the behaviour the pool exists
//! to deliver:
//!
//!   1. **Warm reuse is fast** — the *second* query on a pool key completes far
//!      faster than the first, because it skips the ~170ms TCP+TLS+LOGIN7
//!      handshake (proven separately in `sqlly-tds` to be ~0.49ms warm vs
//!      ~169ms cold).
//!   2. **Per-tab isolation** — two distinct `connection_key`s each run their
//!      own query successfully and independently.
//!   3. **Poisoned clients are not reused** — a query that fails (bad SQL) must
//!      not leave a broken client in the pool; the next query on the same key
//!      still succeeds.
//!
//! All tests self-skip when no live SQL config is present (the gitignored
//! `.sqlly-local-sql.json` or `SQLLY_SQL_*` env vars), so the default
//! `cargo test` run stays hermetic.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;
use sqlly_testkit::LiveSqlConfig;

/// Frame tag bytes — must match `FRAME_TAG_*` in apps/cli/src/engine.rs.
const FRAME_TAG_READY: u8 = 0x03;
const FRAME_TAG_REQUEST_COMPLETED: u8 = 0x07;
const FRAME_TAG_REQUEST_FAILED: u8 = 0x08;

/// Inbound request tag bytes — must match `REQUEST_TAG_*` in engine.rs.
const REQUEST_TAG_SHUTDOWN: u8 = 0x02;
const REQUEST_TAG_EXECUTE_QUERY: u8 = 0x04;

/// Imported from the protocol crate so the pool test always encodes the
/// engine's current request layout version instead of a copied constant that
/// silently drifted to v1 while the engine moved to v2.
use sqlly_remote::engine_protocol::BINARY_REQUEST_LAYOUT_VERSION;

fn spawn_engine() -> Child {
    Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine")
}

/// Builder for binary request bodies. Mirrors `BinaryFrameWriter` in
/// engine_io.rs, trimmed to what the pool tests send.
struct BinaryFrameWriter {
    buf: Vec<u8>,
}

impl BinaryFrameWriter {
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn u8(mut self, v: u8) -> Self {
        self.buf.push(v);
        self
    }

    fn u16_le(mut self, v: u16) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn u32_le(mut self, v: u32) -> Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    fn string(mut self, s: &str) -> Self {
        let bytes = s.as_bytes();
        self.buf
            .extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.buf.extend_from_slice(bytes);
        self
    }

    fn opt_u32(self, v: Option<u32>) -> Self {
        match v {
            Some(n) => self.u8(1).u32_le(n),
            None => self.u8(0),
        }
    }

    fn opt_string(self, s: Option<&str>) -> Self {
        match s {
            Some(v) => self.u8(1).string(v),
            None => self.u8(0),
        }
    }

    fn done(self) -> Vec<u8> {
        self.buf
    }
}

/// Send a request as a binary frame, converting from a JSON value — same
/// ergonomic call style as the old JSON protocol, binary on the wire.
fn send(stdin: &mut ChildStdin, value: &Value) {
    let type_str = value
        .get("type")
        .and_then(|v| v.as_str())
        .expect("request JSON must have a type field");
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .expect("request JSON must have an id field");
    let (tag, body): (u8, Vec<u8>) = match type_str {
        "shutdown" => (
            REQUEST_TAG_SHUTDOWN,
            BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .done(),
        ),
        "execute_query" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("execute_query must have connection"),
            )
            .unwrap();
            let sql = value
                .get("sql")
                .and_then(|v| v.as_str())
                .expect("execute_query must have sql");
            let row_buffer = value
                .get("row_buffer")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let connection_key = value.get("connection_key").and_then(|v| v.as_str());
            (
                REQUEST_TAG_EXECUTE_QUERY,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(sql)
                    .opt_u32(row_buffer)
                    .opt_string(connection_key)
                    .done(),
            )
        }
        other => panic!("unsupported request type for binary frame: {other}"),
    };
    let mut frame = vec![tag];
    frame.extend_from_slice(&body);
    stdin
        .write_all(&(frame.len() as u32).to_le_bytes())
        .expect("write frame length");
    stdin.write_all(&frame).expect("write frame payload");
    stdin.flush().expect("flush request");
}

/// Read a u32 LE from a byte slice at the given offset.
fn read_u32_le_at(bytes: &[u8], offset: usize) -> Option<(u32, usize)> {
    if offset + 4 > bytes.len() {
        return None;
    }
    let val = u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ]);
    Some((val, offset + 4))
}

/// Read a length-prefixed string from a byte slice at the given offset.
fn read_string_at(bytes: &[u8], offset: usize) -> Option<(String, usize)> {
    let (len, after_len) = read_u32_le_at(bytes, offset)?;
    let len = len as usize;
    if after_len + len > bytes.len() {
        return None;
    }
    let s = std::str::from_utf8(&bytes[after_len..after_len + len]).ok()?;
    Some((s.to_string(), after_len + len))
}

/// Decode the binary response frames the pool tests care about into the
/// JSON-like shape the assertions use (`type` / `id` / `error`). Frames the
/// tests do not consume (rows, events, started) decode to None and are
/// skipped by the reader.
fn decode_frame(tag: u8, body: &[u8]) -> Option<Value> {
    match tag {
        FRAME_TAG_READY => {
            let (protocol_version, _) = read_u32_le_at(body, 0)?;
            Some(serde_json::json!({
                "type": "ready",
                "protocol_version": protocol_version,
            }))
        }
        FRAME_TAG_REQUEST_COMPLETED => {
            let (id, _) = read_string_at(body, 0)?;
            Some(serde_json::json!({
                "type": "request_completed",
                "id": id,
            }))
        }
        FRAME_TAG_REQUEST_FAILED => {
            let (id, after_id) = read_string_at(body, 0)?;
            let (error, _) = read_string_at(body, after_id)?;
            Some(serde_json::json!({
                "type": "request_failed",
                "id": id,
                "error": error,
            }))
        }
        _ => None,
    }
}

/// Reader thread forwarding each decoded frame over a channel. Binary frames
/// the tests do not consume are read (to keep the pipe drained) and skipped.
fn frame_reader(child: &mut Child) -> Receiver<Value> {
    let mut stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            let mut len_buf = [0u8; 4];
            if stdout.read_exact(&mut len_buf).is_err() {
                break;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            if stdout.read_exact(&mut payload).is_err() {
                break;
            }
            let Some((tag, body)) = payload.split_first() else {
                break;
            };
            let Some(frame) = decode_frame(*tag, body) else {
                continue;
            };
            if tx.send(frame).is_err() {
                break;
            }
        }
    });
    rx
}

fn frame_type(frame: &Value) -> &str {
    frame.get("type").and_then(Value::as_str).unwrap_or("")
}

fn frame_id(frame: &Value) -> &str {
    frame.get("id").and_then(Value::as_str).unwrap_or("")
}

/// Wait for a terminal frame (`request_completed` / `request_failed`) for `id`,
/// returning its type. Bounded so a regression can never hang the test.
fn wait_terminal(rx: &Receiver<Value>, id: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(frame) => {
                let ty = frame_type(&frame);
                if frame_id(&frame) == id && (ty == "request_completed" || ty == "request_failed") {
                    return ty.to_string();
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                return String::new();
            }
        }
    }
}

/// Like [`wait_terminal`] but returns the whole terminal frame, so a test can
/// inspect the `error` string on a `request_failed`.
fn wait_terminal_frame(rx: &Receiver<Value>, id: &str, timeout: Duration) -> Option<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(frame) => {
                let ty = frame_type(&frame);
                if frame_id(&frame) == id && (ty == "request_completed" || ty == "request_failed") {
                    return Some(frame);
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn wait_ready(rx: &Receiver<Value>) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(frame) if frame_type(&frame) == "ready" => return,
            Ok(_) => continue,
            Err(_) => panic!("engine did not emit ready frame"),
        }
    }
}

/// Build an `execute_query` request value from the live config, with an optional
/// per-tab `connection_key`.
fn execute_query(cfg: &LiveSqlConfig, id: &str, key: Option<&str>, sql: &str) -> Value {
    let mut req = serde_json::json!({
        "type": "execute_query",
        "id": id,
        "connection": {
            "server": cfg.server,
            "port": cfg.port,
            "database": cfg.database,
            "username": cfg.username,
            "password": cfg.password,
            "encrypt": cfg.encrypt,
            "trust_certificate": cfg.trust_server_certificate,
            "timeout_seconds": 15
        },
        "sql": sql
    });
    if let Some(key) = key {
        req["connection_key"] = Value::String(key.to_string());
    }
    req
}

/// Run one query and return (terminal type, elapsed).
fn run_one(
    stdin: &mut ChildStdin,
    rx: &Receiver<Value>,
    cfg: &LiveSqlConfig,
    id: &str,
    key: Option<&str>,
    sql: &str,
) -> (String, Duration) {
    let start = Instant::now();
    send(stdin, &execute_query(cfg, id, key, sql));
    let terminal = wait_terminal(rx, id, Duration::from_secs(30));
    (terminal, start.elapsed())
}

fn shutdown(stdin: &mut ChildStdin, child: &mut Child) {
    send(stdin, &serde_json::json!({"type": "shutdown", "id": "s1"}));
    let _ = child.wait();
}

/// Warm reuse: the second query on the same key is dramatically faster than the
/// first because it reuses the warm client instead of re-handshaking.
#[test]
fn warm_reuse_is_faster_than_cold_connect() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    let key = Some("tab-warm");
    let (t1, cold) = run_one(&mut stdin, &rx, &cfg, "q1", key, "SELECT 1");
    assert_eq!(t1, "request_completed", "cold query should succeed");

    let (t2, warm) = run_one(&mut stdin, &rx, &cfg, "q2", key, "SELECT 1");
    assert_eq!(t2, "request_completed", "warm query should succeed");

    eprintln!("cold={cold:?} warm={warm:?}");
    // The warm path skips the handshake; require a clear margin (>=3x). The
    // handshake alone is ~170ms vs ~0.5ms warm, so this is a conservative bar.
    assert!(
        warm * 3 < cold,
        "warm reuse ({warm:?}) should be at least 3x faster than cold connect ({cold:?})"
    );

    shutdown(&mut stdin, &mut child);
}

/// Per-tab isolation: two distinct connection keys each execute successfully.
#[test]
fn distinct_keys_run_independently() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    // Prime both lanes.
    assert_eq!(
        run_one(&mut stdin, &rx, &cfg, "a1", Some("tab-a"), "SELECT 1").0,
        "request_completed"
    );
    assert_eq!(
        run_one(&mut stdin, &rx, &cfg, "b1", Some("tab-b"), "SELECT 1").0,
        "request_completed"
    );

    // Each key's second query should reuse its own warm client and succeed.
    let (a2, a_warm) = run_one(&mut stdin, &rx, &cfg, "a2", Some("tab-a"), "SELECT 1");
    let (b2, b_warm) = run_one(&mut stdin, &rx, &cfg, "b2", Some("tab-b"), "SELECT 1");
    assert_eq!(a2, "request_completed");
    assert_eq!(b2, "request_completed");
    eprintln!("tab-a warm={a_warm:?} tab-b warm={b_warm:?}");

    shutdown(&mut stdin, &mut child);
}

/// A failed query must not poison the pool: the next query on the same key still
/// succeeds (the broken client was dropped, not checked back in).
#[test]
fn failed_query_does_not_poison_the_pool() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    let key = Some("tab-poison");
    // Prime a warm client.
    assert_eq!(
        run_one(&mut stdin, &rx, &cfg, "p1", key, "SELECT 1").0,
        "request_completed"
    );
    // Force a failure on the same key (syntax error).
    let (bad, _) = run_one(&mut stdin, &rx, &cfg, "p2", key, "SELECT FROM WHERE bogus");
    assert_eq!(bad, "request_failed", "bad SQL should fail");

    // The next query on the same key must still succeed — proving no broken
    // client lingered in the pool.
    let (recover, _) = run_one(&mut stdin, &rx, &cfg, "p3", key, "SELECT 1");
    assert_eq!(
        recover, "request_completed",
        "pool must recover after a failed query (no poisoned client reused)"
    );

    shutdown(&mut stdin, &mut child);
}

/// A failed batch reports the REAL server error text in its terminal frame (and
/// therefore in the engine correlation log), not a generic "completed with
/// errors" string.
#[test]
fn batch_error_surfaces_the_real_server_error() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    send(
        &mut stdin,
        &execute_query(
            &cfg,
            "e1",
            Some("tab-err"),
            "SELECT * FROM dbo.__sqlly_no_such_table_xyz",
        ),
    );
    let frame = wait_terminal_frame(&rx, "e1", Duration::from_secs(30)).expect("terminal frame");
    assert_eq!(frame_type(&frame), "request_failed");
    let error = frame.get("error").and_then(Value::as_str).unwrap_or("");
    // SQL Server's message for a missing object; the point is it is the actual
    // reason, not the old generic "the query completed with errors".
    assert!(
        error.contains("Invalid object name"),
        "expected the real server error, got: {error:?}"
    );
    assert_ne!(error, "the query completed with errors");

    shutdown(&mut stdin, &mut child);
}

/// A transaction left open by an errored batch must be rolled back before the
/// warm client is re-pooled, so the NEXT query on the same key does not run
/// inside a leaked transaction. The follow-up throws if `@@TRANCOUNT > 0`, so a
/// leak would surface as a failure.
#[test]
fn open_transaction_is_rolled_back_before_repool() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    let key = Some("tab-txn");
    // Open a transaction, then hit a runtime error inside it. @@TRANCOUNT is 1
    // when the batch ends; the engine must roll it back before check-in.
    let (bad, _) = run_one(
        &mut stdin,
        &rx,
        &cfg,
        "t1",
        key,
        "BEGIN TRANSACTION;\nSELECT 1/0;",
    );
    assert_eq!(
        bad, "request_failed",
        "divide-by-zero should fail the batch"
    );

    // Reuse the same key: if the transaction leaked into the pooled client this
    // THROWs; a clean session completes.
    send(
        &mut stdin,
        &execute_query(
            &cfg,
            "t2",
            key,
            "IF @@TRANCOUNT > 0 THROW 50000, 'transaction leaked into pooled connection', 1;\nSELECT 1;",
        ),
    );
    let frame = wait_terminal_frame(&rx, "t2", Duration::from_secs(30)).expect("terminal frame");
    assert_eq!(
        frame_type(&frame),
        "request_completed",
        "the pooled connection must not carry a leaked transaction: {:?}",
        frame.get("error")
    );

    shutdown(&mut stdin, &mut child);
}

/// A multi-batch (GO-separated) script where an early batch errors still runs to
/// a terminal state and leaves the pool usable for the next query.
#[test]
fn multi_batch_error_still_terminates_and_pool_recovers() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping: no live SQL config");
        return;
    };
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_reader(&mut child);
    wait_ready(&rx);

    let key = Some("tab-multi");
    // Batch 1 errors (bad object); batch 2 is valid. The run reports failed, but
    // the connection stays healthy.
    let (term, _) = run_one(
        &mut stdin,
        &rx,
        &cfg,
        "m1",
        key,
        "SELECT * FROM dbo.__sqlly_missing_xyz;\nGO\nSELECT 1;\nGO",
    );
    assert_eq!(term, "request_failed", "the errored batch fails the run");

    let (recover, _) = run_one(&mut stdin, &rx, &cfg, "m2", key, "SELECT 1");
    assert_eq!(
        recover, "request_completed",
        "pool must stay usable after a multi-batch error"
    );

    shutdown(&mut stdin, &mut child);
}
