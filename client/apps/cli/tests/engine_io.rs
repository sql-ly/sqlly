//! Integration tests for the `sqlly-cli engine` subprocess protocol.
//!
//! These exercise the real binary over its stdin/stdout length-prefixed framing
//! and guard two regressions that previously left clients hung on
//! "Query Running" forever:
//!
//!   1. Isolated/trailing frames (a terminal frame emitted after a time gap)
//!      must actually flush to the OS pipe. The async `tokio::io::stdout()`
//!      writer buffered these and they never reached the client; the engine now
//!      uses a synchronous blocking-thread writer.
//!   2. A query against an unreachable / unresponsive server must always yield a
//!      terminal frame within the connection deadline (connect timeout + the
//!      per-query no-response watchdog), never hang indefinitely.

use std::io::{Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

// Frame tags, request tags, and the layout version are the shared wire codec
// constants from `sqlly-engine-protocol` — the same values `engine.rs` speaks —
// rather than a copy that could silently drift. The `BinaryWriter` builder is
// likewise the shared one, aliased to this test's historical local name.
use sqlly_remote::engine_protocol::{
    BINARY_REQUEST_LAYOUT_VERSION, BinaryWriter as BinaryFrameWriter,
    FRAME_TAG_AI_CACHE_BUILD_REPORT, FRAME_TAG_CATALOG_SYNC_REPORT,
    FRAME_TAG_EMBEDDINGS_BUILD_REPORT, FRAME_TAG_EVENT, FRAME_TAG_INVALIDATE_AI_CACHE_REPORT,
    FRAME_TAG_JOIN_PATH_REPORT, FRAME_TAG_PONG, FRAME_TAG_READY, FRAME_TAG_REQUEST_CANCELLED,
    FRAME_TAG_REQUEST_COMPLETED, FRAME_TAG_REQUEST_FAILED, FRAME_TAG_REQUEST_STARTED,
    FRAME_TAG_ROWS_BATCH, FRAME_TAG_SCHEMA_ANSWER, FRAME_TAG_SCHEMA_SEARCH_RESULTS,
    FRAME_TAG_SHUTDOWN_ACK, FRAME_TAG_SQL_GENERATION_REPORT, FRAME_TAG_SQL_VALIDATION_REPORT,
    FRAME_TAG_VALIDATION, REQUEST_TAG_ASK_SCHEMA, REQUEST_TAG_BUILD_AI_CACHE,
    REQUEST_TAG_BUILD_EMBEDDINGS, REQUEST_TAG_CANCEL, REQUEST_TAG_EXECUTE_QUERY,
    REQUEST_TAG_FIND_JOIN_PATH, REQUEST_TAG_GENERATE_SQL, REQUEST_TAG_INVALIDATE_AI_CACHE,
    REQUEST_TAG_PING, REQUEST_TAG_SEARCH_SCHEMA, REQUEST_TAG_SHUTDOWN, REQUEST_TAG_SYNC_CATALOG,
    REQUEST_TAG_VALIDATE, REQUEST_TAG_VALIDATE_SQL, RawFrame, read_length_prefixed_blocking,
    write_length_prefixed_blocking,
};

/// Static empty JSON array for `unwrap_or(&EMPTY_ARRAY)` patterns.
static EMPTY_ARRAY: Vec<serde_json::Value> = Vec::new();

fn spawn_engine() -> Child {
    Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("engine")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine")
}

/// Spawns an engine with `SQLLY_ENGINE_LOG` pointed at `log_path`, so the
/// request/response correlation log is written there for assertion.
fn spawn_engine_with_log(log_path: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("engine")
        .env("SQLLY_ENGINE_LOG", log_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine")
}

/// Send a request as a binary frame, converting from a JSON value. This
/// preserves the ergonomic `send(stdin, &json!({...}))` call style while
/// sending fully binary frames on the wire.
fn send(stdin: &mut ChildStdin, value: &serde_json::Value) {
    let frame = json_to_binary_frame(value);
    send_raw(stdin, &frame);
}

/// Convert a JSON request value into a binary frame `[tag][body]`.
/// The "type" field maps to the request tag; remaining fields are encoded
/// in the binary body.
fn json_to_binary_frame(value: &serde_json::Value) -> Vec<u8> {
    let type_str = value
        .get("type")
        .and_then(|v| v.as_str())
        .expect("request JSON must have a type field");
    let id = value
        .get("id")
        .and_then(|v| v.as_str())
        .expect("request JSON must have an id field");

    let (tag, body): (u8, Vec<u8>) = match type_str {
        "ping" => (
            REQUEST_TAG_PING,
            BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .done(),
        ),
        "shutdown" => (
            REQUEST_TAG_SHUTDOWN,
            BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .done(),
        ),
        "cancel" => {
            let target_id = value
                .get("target_id")
                .and_then(|v| v.as_str())
                .expect("cancel must have target_id");
            (
                REQUEST_TAG_CANCEL,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(target_id)
                    .done(),
            )
        }
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
        "provider_status" => (
            0x14,
            BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .done(),
        ),
        "review_sql" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("review_sql must have connection"),
            )
            .unwrap();
            let args = serde_json::to_string(value.get("args").expect("review_sql must have args"))
                .unwrap();
            (
                0x15,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(&args)
                    .done(),
            )
        }
        "summarize_query" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("summarize_query must have connection"),
            )
            .unwrap();
            let args =
                serde_json::to_string(value.get("args").expect("summarize_query must have args"))
                    .unwrap();
            (
                0x16,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(&args)
                    .done(),
            )
        }
        "catalog_maintenance" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("catalog_maintenance must have connection"),
            )
            .unwrap();
            let options = serde_json::to_string(
                value
                    .get("options")
                    .expect("catalog_maintenance must have options"),
            )
            .unwrap();
            (
                0x13, // REQUEST_TAG_CATALOG_MAINTENANCE
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(&options)
                    .done(),
            )
        }
        "sync_catalog" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("sync_catalog must have connection"),
            )
            .unwrap();
            let objects = value
                .get("objects")
                .and_then(|v| v.as_array())
                .unwrap_or(&EMPTY_ARRAY);
            let mut w = BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .string(&connection)
                .u32_le(objects.len() as u32);
            for obj in objects {
                w = encode_json_catalog_object(w, obj);
            }
            let body = w.done();
            (REQUEST_TAG_SYNC_CATALOG, body)
        }
        "build_ai_cache" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("build_ai_cache must have connection"),
            )
            .unwrap();
            (
                REQUEST_TAG_BUILD_AI_CACHE,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .done(),
            )
        }
        "invalidate_ai_cache" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("invalidate_ai_cache must have connection"),
            )
            .unwrap();
            (
                REQUEST_TAG_INVALIDATE_AI_CACHE,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .done(),
            )
        }
        "ask_schema" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("ask_schema must have connection"),
            )
            .unwrap();
            let question = value
                .get("question")
                .and_then(|v| v.as_str())
                .expect("ask_schema must have question");
            let max_tokens = value
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let system = value.get("system").and_then(|v| v.as_str());
            let purpose = value
                .get("purpose")
                .and_then(|v| v.as_str())
                .unwrap_or("general");
            let purpose_byte = match purpose {
                "sql_generation" => 1u8,
                _ => 0u8,
            };
            (
                REQUEST_TAG_ASK_SCHEMA,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(question)
                    .opt_u32(max_tokens)
                    .opt_string(system)
                    .u8(purpose_byte)
                    .done(),
            )
        }
        "search_schema" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("search_schema must have connection"),
            )
            .unwrap();
            let query = value
                .get("query")
                .and_then(|v| v.as_str())
                .expect("search_schema must have query");
            let limit = value
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let object_types = value
                .get("object_types")
                .and_then(|v| v.as_array())
                .unwrap_or(&EMPTY_ARRAY);
            let mut w = BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .string(&connection)
                .string(query)
                .opt_u32(limit)
                .u32_le(object_types.len() as u32);
            for ot in object_types {
                if let Some(s) = ot.as_str() {
                    w = w.string(s);
                }
            }
            (REQUEST_TAG_SEARCH_SCHEMA, w.done())
        }
        "find_join_path" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("find_join_path must have connection"),
            )
            .unwrap();
            let source = value
                .get("source")
                .and_then(|v| v.as_str())
                .expect("find_join_path must have source");
            let target = value
                .get("target")
                .and_then(|v| v.as_str())
                .expect("find_join_path must have target");
            let max_depth = value.get("max_depth").and_then(|v| v.as_u64()).unwrap_or(4) as u32;
            let include_inferred = value
                .get("include_inferred")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let min_confidence = value
                .get("min_confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            (
                REQUEST_TAG_FIND_JOIN_PATH,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(source)
                    .string(target)
                    .u32_le(max_depth)
                    .bool(include_inferred)
                    .f64_le(min_confidence)
                    .done(),
            )
        }
        "build_embeddings" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("build_embeddings must have connection"),
            )
            .unwrap();
            (
                REQUEST_TAG_BUILD_EMBEDDINGS,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .done(),
            )
        }
        "generate_sql" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("generate_sql must have connection"),
            )
            .unwrap();
            let question = value
                .get("question")
                .and_then(|v| v.as_str())
                .expect("generate_sql must have question");
            let dialect = value.get("dialect").and_then(|v| v.as_str());
            let max_tokens = value
                .get("max_tokens")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            let system = value.get("system").and_then(|v| v.as_str());
            let max_attempts = value
                .get("max_attempts")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32);
            (
                REQUEST_TAG_GENERATE_SQL,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(question)
                    .opt_string(dialect)
                    .opt_u32(max_tokens)
                    .opt_string(system)
                    .opt_u32(max_attempts)
                    .done(),
            )
        }
        "validate_sql" => {
            let connection = serde_json::to_string(
                value
                    .get("connection")
                    .expect("validate_sql must have connection"),
            )
            .unwrap();
            let sql = value
                .get("sql")
                .and_then(|v| v.as_str())
                .expect("validate_sql must have sql");
            let dialect = value.get("dialect").and_then(|v| v.as_str());
            (
                REQUEST_TAG_VALIDATE_SQL,
                BinaryFrameWriter::new()
                    .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                    .string(id)
                    .string(&connection)
                    .string(sql)
                    .opt_string(dialect)
                    .done(),
            )
        }
        _ => {
            // Unknown type — use an invalid tag so the engine rejects it.
            let body = BinaryFrameWriter::new()
                .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
                .string(id)
                .done();
            (0xFF, body)
        }
    };

    let mut frame = vec![tag];
    frame.extend_from_slice(&body);
    frame
}

/// Encode a JSON catalog object into a BinaryFrameWriter for sync_catalog.
fn encode_json_catalog_object(
    mut w: BinaryFrameWriter,
    obj: &serde_json::Value,
) -> BinaryFrameWriter {
    let kind_byte = |k: &str| -> u8 {
        match k {
            "table" => 0,
            "view" => 1,
            "stored_procedure" => 2,
            "function" => 3,
            "type" => 4,
            _ => 0,
        }
    };
    let source_byte = |k: &str| -> u8 {
        match k {
            "database" => 0,
            "file" => 1,
            _ => 0,
        }
    };

    let schema = obj.get("schema").and_then(|v| v.as_str());
    let name = obj.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let kind = obj.get("kind").and_then(|v| v.as_str()).unwrap_or("table");
    w = w.opt_string(schema).string(name).u8(kind_byte(kind));

    let columns = obj
        .get("columns")
        .and_then(|v| v.as_array())
        .unwrap_or(&EMPTY_ARRAY);
    w = w.u32_le(columns.len() as u32);
    for col in columns {
        let col_name = col.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let col_type = col.get("type_name").and_then(|v| v.as_str());
        let ordinal = col.get("ordinal").and_then(|v| v.as_i64()).unwrap_or(0);
        let nullable = col
            .get("nullable")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        w = w
            .string(col_name)
            .opt_string(col_type)
            .i64_le(ordinal)
            .bool(nullable);
    }

    // Primary-key block sits between columns and foreign keys in the v2 layout
    // (see `decode_sync_catalog_binary`). Omitting it made the decoder read the
    // FK count as the PK count.
    let primary_key = obj
        .get("primary_key")
        .and_then(|v| v.as_array())
        .unwrap_or(&EMPTY_ARRAY);
    w = w.u32_le(primary_key.len() as u32);
    for col in primary_key {
        if let Some(s) = col.as_str() {
            w = w.string(s);
        }
    }

    let fks = obj
        .get("foreign_keys")
        .and_then(|v| v.as_array())
        .unwrap_or(&EMPTY_ARRAY);
    w = w.u32_le(fks.len() as u32);
    for fk in fks {
        let columns = fk
            .get("columns")
            .and_then(|v| v.as_array())
            .unwrap_or(&EMPTY_ARRAY);
        w = w.u32_le(columns.len() as u32);
        for col in columns {
            if let Some(s) = col.as_str() {
                w = w.string(s);
            }
        }
        let target_schema = fk.get("target_schema").and_then(|v| v.as_str());
        let target_table = fk
            .get("target_table")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let target_columns = fk
            .get("target_columns")
            .and_then(|v| v.as_array())
            .unwrap_or(&EMPTY_ARRAY);
        w = w
            .opt_string(target_schema)
            .string(target_table)
            .u32_le(target_columns.len() as u32);
        for col in target_columns {
            if let Some(s) = col.as_str() {
                w = w.string(s);
            }
        }
    }

    let params = obj
        .get("parameters")
        .and_then(|v| v.as_array())
        .unwrap_or(&EMPTY_ARRAY);
    w = w.u32_le(params.len() as u32);
    for param in params {
        let param_name = param.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let param_type = param.get("type_name").and_then(|v| v.as_str());
        let is_output = param
            .get("is_output")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        w = w.string(param_name).opt_string(param_type).bool(is_output);
    }

    let definition = obj.get("definition").and_then(|v| v.as_str());
    let source = obj.get("source").unwrap_or(&serde_json::Value::Null);
    let source_kind = source
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("database");
    let path = source.get("path").and_then(|v| v.as_str());
    let create_date = source.get("create_date").and_then(|v| v.as_i64());
    let modify_date = source.get("modify_date").and_then(|v| v.as_i64());
    w = w
        .opt_string(definition)
        .u8(source_byte(source_kind))
        .opt_string(path)
        .opt_i64(create_date)
        .opt_i64(modify_date);
    w
}

/// Send a raw payload (already constructed bytes) as a length-prefixed frame.
/// Used for binary tagged request frames (protocol v2).
fn send_raw(stdin: &mut ChildStdin, payload: &[u8]) {
    write_length_prefixed_blocking(stdin, payload).expect("write request frame");
}

/// Build a binary ping frame (tag 0x01).
fn ping_frame(id: &str) -> Vec<u8> {
    let body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .done();
    let mut frame = vec![REQUEST_TAG_PING];
    frame.extend_from_slice(&body);
    frame
}

/// Build a binary shutdown frame (tag 0x02).
fn shutdown_frame(id: &str) -> Vec<u8> {
    let body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .done();
    let mut frame = vec![REQUEST_TAG_SHUTDOWN];
    frame.extend_from_slice(&body);
    frame
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

/// Decode a binary response body (tag + body) into a JSON-like
/// `serde_json::Value` that matches the old JSON format, so existing test
/// assertions like `frame["type"]`, `frame["id"]`, `frame["report"]` still work.
fn decode_binary_frame_to_json(tag: u8, body: &[u8]) -> Option<serde_json::Value> {
    let (type_name, payload_field) = match tag {
        FRAME_TAG_READY => {
            let (pv, _) = read_u32_le_at(body, 0)?;
            return Some(serde_json::json!({
                "type": "ready",
                "protocol_version": pv,
            }));
        }
        FRAME_TAG_PONG => ("pong", None),
        FRAME_TAG_REQUEST_STARTED => ("request_started", None),
        FRAME_TAG_EVENT => ("event", Some("event")),
        FRAME_TAG_REQUEST_COMPLETED => ("request_completed", None),
        FRAME_TAG_REQUEST_FAILED => ("request_failed", None),
        FRAME_TAG_REQUEST_CANCELLED => ("request_cancelled", None),
        FRAME_TAG_SHUTDOWN_ACK => ("shutdown_ack", None),
        FRAME_TAG_CATALOG_SYNC_REPORT => ("catalog_sync_report", Some("report")),
        0x15 => ("catalog_maintenance_report", Some("report")),
        0x16 => ("provider_status_report", Some("report")),
        0x17 => ("sql_review_report", Some("report")),
        0x18 => ("query_summary_report", Some("report")),
        FRAME_TAG_AI_CACHE_BUILD_REPORT => ("ai_cache_build_report", Some("report")),
        FRAME_TAG_INVALIDATE_AI_CACHE_REPORT => ("invalidate_ai_cache_report", Some("report")),
        FRAME_TAG_SCHEMA_ANSWER => ("schema_answer", Some("response")),
        FRAME_TAG_SCHEMA_SEARCH_RESULTS => ("schema_search_results", Some("results")),
        FRAME_TAG_JOIN_PATH_REPORT => ("join_path_report", Some("paths")),
        FRAME_TAG_EMBEDDINGS_BUILD_REPORT => ("embeddings_build_report", Some("report")),
        FRAME_TAG_SQL_GENERATION_REPORT => ("sql_generation_report", Some("report")),
        FRAME_TAG_SQL_VALIDATION_REPORT => ("sql_validation_report", Some("report")),
        _ => return None,
    };

    let (id, after_id) = read_string_at(body, 0)?;
    let mut json = serde_json::json!({
        "type": type_name,
        "id": id,
    });

    match tag {
        FRAME_TAG_REQUEST_FAILED => {
            let (error, _) = read_string_at(body, after_id)?;
            json["error"] = serde_json::Value::String(error);
        }
        FRAME_TAG_REQUEST_CANCELLED => {
            let (target_id, _) = read_string_at(body, after_id)?;
            json["target_id"] = serde_json::Value::String(target_id);
        }
        FRAME_TAG_EMBEDDINGS_BUILD_REPORT => {
            let (json_len, after_json_len) = read_u32_le_at(body, after_id)?;
            let json_bytes = &body[after_json_len..after_json_len + json_len as usize];
            let payload: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
            if let Some(field) = payload_field {
                json[field] = payload;
            }
            let after_json = after_json_len + json_len as usize;
            if after_json < body.len() {
                let note_present = body[after_json];
                if note_present == 1
                    && let Some((note, _)) = read_string_at(body, after_json + 1)
                {
                    json["note"] = serde_json::Value::String(note);
                }
            }
        }
        _ => {
            if let Some(field) = payload_field {
                let (json_len, after_json_len) = read_u32_le_at(body, after_id)?;
                let json_bytes = &body[after_json_len..after_json_len + json_len as usize];
                let payload: serde_json::Value = serde_json::from_slice(json_bytes).ok()?;
                json[field] = payload;
            }
        }
    }

    Some(json)
}

/// Encodes a sync_catalog request as a binary frame (tag 0x11 + body) from
/// JSON test fixtures. This lets integration tests reuse the existing
/// `catalog_connection()` / `catalog_objects()` JSON helpers without changing
/// the fixture functions themselves.
fn encode_sync_catalog_binary(
    id: &str,
    connection: &serde_json::Value,
    objects: &serde_json::Value,
) -> Vec<u8> {
    let connection_json = serde_json::to_string(connection).expect("serialize connection");
    let objects_arr = objects.as_array().expect("objects is an array");
    let mut w = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(&connection_json)
        .u32_le(objects_arr.len() as u32);
    for obj in objects_arr {
        let schema = obj.get("schema").and_then(|v| v.as_str());
        let name = obj.get("name").and_then(|v| v.as_str()).expect("name");
        let type_str = obj.get("type").and_then(|v| v.as_str()).expect("type");
        let kind = match type_str {
            "table" => 0u8,
            "view" => 1,
            "stored_procedure" => 2,
            "function" => 3,
            "type" => 4,
            _ => 0,
        };
        w = w.opt_string(schema).string(name).u8(kind);
        let columns = obj
            .get("columns")
            .and_then(|v| v.as_array())
            .expect("columns");
        w = w.u32_le(columns.len() as u32);
        for col in columns {
            let col_name = col.get("name").and_then(|v| v.as_str()).expect("col name");
            let type_name = col.get("type_name").and_then(|v| v.as_str());
            let ordinal = col
                .get("ordinal")
                .and_then(|v| v.as_i64())
                .expect("ordinal");
            let nullable = col
                .get("nullable")
                .and_then(|v| v.as_bool())
                .expect("nullable");
            w = w
                .string(col_name)
                .opt_string(type_name)
                .i64_le(ordinal)
                .bool(nullable);
        }
        // Primary-key block between columns and foreign keys (v2 layout).
        let primary_key = obj
            .get("primary_key")
            .and_then(|v| v.as_array())
            .unwrap_or(&EMPTY_ARRAY);
        w = w.u32_le(primary_key.len() as u32);
        for col in primary_key {
            if let Some(s) = col.as_str() {
                w = w.string(s);
            }
        }
        let fks = obj
            .get("foreign_keys")
            .and_then(|v| v.as_array())
            .expect("foreign_keys");
        w = w.u32_le(fks.len() as u32);
        for fk in fks {
            let fk_cols = fk
                .get("columns")
                .and_then(|v| v.as_array())
                .expect("fk columns");
            w = w.u32_le(fk_cols.len() as u32);
            for c in fk_cols {
                w = w.string(c.as_str().expect("fk col"));
            }
            let target_schema = fk.get("target_schema").and_then(|v| v.as_str());
            let target_table = fk
                .get("target_table")
                .and_then(|v| v.as_str())
                .expect("target_table");
            let target_cols = fk
                .get("target_columns")
                .and_then(|v| v.as_array())
                .expect("target_columns");
            w = w
                .opt_string(target_schema)
                .string(target_table)
                .u32_le(target_cols.len() as u32);
            for c in target_cols {
                w = w.string(c.as_str().expect("target col"));
            }
        }
        let params = obj
            .get("parameters")
            .and_then(|v| v.as_array())
            .expect("parameters");
        w = w.u32_le(params.len() as u32);
        for param in params {
            let name = param
                .get("name")
                .and_then(|v| v.as_str())
                .expect("param name");
            let type_name = param.get("type_name").and_then(|v| v.as_str());
            let is_output = param
                .get("is_output")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            w = w.string(name).opt_string(type_name).bool(is_output);
        }
        let definition = obj.get("definition").and_then(|v| v.as_str());
        w = w.opt_string(definition);
        let source = obj.get("source").expect("source");
        let source_type = source
            .get("type")
            .and_then(|v| v.as_str())
            .expect("source type");
        let source_kind = match source_type {
            "database" => 0u8,
            "file" => 1,
            _ => 0,
        };
        let path = source.get("path").and_then(|v| v.as_str());
        let create_date = source.get("create_date").and_then(|v| v.as_i64());
        let modify_date = source.get("modify_date").and_then(|v| v.as_i64());
        w = w
            .u8(source_kind)
            .opt_string(path)
            .opt_i64(create_date)
            .opt_i64(modify_date);
    }
    let mut frame = vec![REQUEST_TAG_SYNC_CATALOG];
    frame.extend_from_slice(&w.done());
    frame
}

/// Encodes a validate request as a binary frame (tag 0x10 + body) from
/// JSON test fixtures.
fn encode_validate_binary(
    id: &str,
    sql: &str,
    dialect: Option<&str>,
    default_schema: Option<&str>,
    objects: &[(&str, &str, &str, &[&str])], // (schema, name, kind_str, column_names)
) -> Vec<u8> {
    let kind_byte = |k: &str| -> u8 {
        match k {
            "table" => 0,
            "view" => 1,
            "sproc" => 2,
            "function" => 3,
            "type" => 4,
            _ => 0,
        }
    };
    let mut w = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string(id)
        .string(sql)
        .opt_string(dialect)
        .opt_string(default_schema)
        .u32_le(objects.len() as u32);
    for (schema, name, kind, columns) in objects {
        w = w
            .string(schema)
            .string(name)
            .u8(kind_byte(kind))
            .u32_le(columns.len() as u32);
        for col in *columns {
            w = w.string(col);
        }
        w = w.u32_le(0); // parameter_count
    }
    w = w.u32_le(0); // foreign_key_count
    let mut frame = vec![REQUEST_TAG_VALIDATE];
    frame.extend_from_slice(&w.done());
    frame
}

/// Spawns a reader thread that decodes length-prefixed frames from the child's
/// stdout and forwards each frame's type name over a channel. Bounding every
/// `recv` with a timeout guarantees the test can never hang if a regression
/// reintroduces a missing terminal frame.
///
/// All frames are binary: the type name is derived from the tag byte, falling
/// back to JSON decoding for complex payloads.
fn frame_type_reader(child: &mut Child) -> Receiver<String> {
    let mut stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(Some(payload)) = read_length_prefixed_blocking(&mut stdout) {
            let frame = match RawFrame::from_payload(&payload) {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let (tag, body) = (frame.tag, frame.body.as_slice());
            let frame_type = match tag {
                FRAME_TAG_VALIDATION => "validation_report".to_string(),
                FRAME_TAG_ROWS_BATCH => "rows_batch".to_string(),
                _ => {
                    if let Some(json) = decode_binary_frame_to_json(tag, body) {
                        json.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else {
                        format!("unknown_tag_0x{:02x}", tag)
                    }
                }
            };
            if tx.send(frame_type).is_err() {
                break;
            }
        }
    });
    rx
}

fn frame_value_reader(child: &mut Child) -> Receiver<serde_json::Value> {
    let mut stdout = child.stdout.take().expect("child stdout");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(Some(payload)) = read_length_prefixed_blocking(&mut stdout) {
            let frame = match RawFrame::from_payload(&payload) {
                Ok(frame) => frame,
                Err(_) => break,
            };
            let (tag, body) = (frame.tag, frame.body.as_slice());
            // Skip binary frames that don't decode to JSON (validation, rows).
            if tag == FRAME_TAG_VALIDATION || tag == FRAME_TAG_ROWS_BATCH {
                continue;
            }
            if let Some(json) = decode_binary_frame_to_json(tag, body)
                && tx.send(json).is_err()
            {
                break;
            }
        }
    });
    rx
}

/// Spawns a reader thread that decodes length-prefixed frames from the child's
/// stdout and forwards `(tag, body)` tuples for every frame — binary or JSON.
/// The tag is the raw frame tag byte (0x00 JSON, 0x01 validation, 0x02 rows);
/// the body is the raw bytes after the tag byte. For JSON frames the body is
/// the JSON payload; for binary frames it is the flat binary body.
fn frame_raw_reader(child: &mut Child) -> Receiver<(u8, Vec<u8>)> {
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
            let (tag, body) = match payload.split_first() {
                Some((tag, rest)) => (*tag, rest.to_vec()),
                None => break,
            };
            if tx.send((tag, body)).is_err() {
                break;
            }
        }
    });
    rx
}

/// Waits for a JSON frame (tag 0x00) whose `type` field matches `wanted`.
fn wait_for_raw_json(
    rx: &Receiver<(u8, Vec<u8>)>,
    wanted: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((tag, body)) => {
                if tag == FRAME_TAG_VALIDATION || tag == FRAME_TAG_ROWS_BATCH {
                    continue;
                }
                if let Some(json) = decode_binary_frame_to_json(tag, &body)
                    && json.get("type").and_then(|v| v.as_str()) == Some(wanted)
                {
                    return Some(json);
                }
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Waits for a binary frame with the specified tag and returns its raw body.
fn wait_for_raw_binary(
    rx: &Receiver<(u8, Vec<u8>)>,
    wanted_tag: u8,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok((tag, body)) if tag == wanted_tag => return Some(body),
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn wait_for(rx: &Receiver<String>, wanted: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(frame_type) if frame_type == wanted => return true,
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return false,
        }
    }
}

fn wait_for_value(
    rx: &Receiver<serde_json::Value>,
    wanted: &str,
    timeout: Duration,
) -> Option<serde_json::Value> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(remaining) {
            Ok(frame) if frame.get("type").and_then(|value| value.as_str()) == Some(wanted) => {
                return Some(frame);
            }
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

const CHILD_EXIT_POLL: Duration = Duration::from_millis(10);

/// Poll until the child process exits or `timeout` elapses.
fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(Some(_)) = child.try_wait() {
            return true;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        thread::sleep(remaining.min(CHILD_EXIT_POLL));
    }
    matches!(child.try_wait(), Ok(Some(_)))
}

/// Wait until at least `duration` has elapsed (bounded poll, not one fixed sleep).
fn wait_for_minimum_elapsed(duration: Duration) {
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        thread::sleep(remaining.min(CHILD_EXIT_POLL));
    }
}

/// Regression: an isolated frame written after a gap must reach the client.
/// We read the startup `ready`, then send a `ping`, wait past any write-batching
/// window, and send a second `ping`. Both `pong` frames must arrive — the second
/// one is the "isolated trailing frame" that the buffered async writer dropped.
#[test]
fn isolated_frames_are_flushed_to_the_pipe() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "engine should emit a ready frame on startup"
    );

    send(&mut stdin, &serde_json::json!({"type": "ping", "id": "p1"}));
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "first ping should be answered"
    );

    // Force the second frame to be written in isolation, after a gap.
    wait_for_minimum_elapsed(Duration::from_millis(400));

    send(&mut stdin, &serde_json::json!({"type": "ping", "id": "p2"}));
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "isolated trailing pong must flush to the pipe (regression: buffered stdout)"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Regression: a query against an unreachable server must always terminate with
/// a `request_failed` frame within the connection deadline — never hang. We use
/// a documentation/test-net address (RFC 5737) with a short timeout. Whether the
/// host refuses immediately, the connect times out, or the no-response watchdog
/// fires, the engine must emit `request_failed`.
#[test]
fn unreachable_server_always_yields_a_terminal_frame() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "execute_query",
            "id": "q1",
            "connection": {
                "server": "192.0.2.1",
                "port": 1433,
                "database": "x",
                "username": "sa",
                "password": "x",
                "encrypt": true,
                "trust_certificate": true,
                "timeout_seconds": 2
            },
            "sql": "SELECT 1"
        }),
    );

    assert!(
        wait_for(&rx, "request_failed", Duration::from_secs(10)),
        "engine must emit a terminal request_failed for an unreachable server"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// A SQLite connection routed through the engine executes end-to-end: the
/// `database_type` field on the connection dispatches the query to the SQLite
/// driver instead of TDS, rows stream back, and the request completes.
/// Hermetic — the database is a temp file the driver creates on open.
#[test]
fn sqlite_connection_executes_through_the_engine() {
    let dir = std::env::temp_dir().join(format!("sqlly-engine-sqlite-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("engine-io.db");
    let _ = std::fs::remove_file(&db_path);

    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);
    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "execute_query",
            "id": "sq1",
            "connection": {
                "server": db_path.to_string_lossy(),
                "port": 0,
                "username": "",
                "password": "",
                "database_type": "Sqlite"
            },
            "sql": "SELECT 40 + 2 AS answer"
        }),
    );

    assert!(
        wait_for(&rx, "rows_batch", Duration::from_secs(15)),
        "sqlite query must stream a rows batch"
    );
    assert!(
        wait_for(&rx, "request_completed", Duration::from_secs(15)),
        "sqlite query must complete"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_file(&db_path);
}

/// `catalog_maintenance` runs end-to-end against a real subprocess: sync two
/// objects, tombstone one via an empty full sync, then purge with a zero-day
/// window and confirm the maintenance report frame arrives.
#[test]
fn catalog_maintenance_round_trips_through_the_engine() {
    let dir = std::env::temp_dir().join(format!("sqlly-engine-maint-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    let connection = catalog_connection();
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "sync_catalog",
            "id": "m-sync",
            "connection": connection.clone(),
            "objects": [
                {"schema": "dbo", "name": "users", "kind": "table", "columns": [], "source": {"kind": "database"}},
            ],
        }),
    );
    assert!(
        wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(10)).is_some(),
        "sync must report"
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "sync_catalog",
            "id": "m-sync2",
            "connection": connection.clone(),
            "objects": [],
        }),
    );
    assert!(
        wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(10)).is_some(),
        "tombstoning sync must report"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "catalog_maintenance",
            "id": "m-1",
            "connection": connection,
            "options": {
                "purge_tombstones_older_than_days": 0,
                "drop_orphan_embeddings": true,
                "optimize_search_index": true
            },
        }),
    );
    let report = wait_for_value(&rx, "catalog_maintenance_report", Duration::from_secs(10))
        .expect("maintenance report frame");
    let payload = report.get("report").expect("report payload");
    assert_eq!(
        payload.get("tombstones_purged").and_then(|v| v.as_u64()),
        Some(1),
        "{payload:?}"
    );
    assert_eq!(
        payload
            .get("search_index_optimized")
            .and_then(|v| v.as_bool()),
        Some(true),
        "{payload:?}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

/// Provider status, grounded review, and query summary all round-trip through
/// a real subprocess on the deterministic stub providers.
#[test]
fn provider_status_review_and_summary_round_trip() {
    let dir = std::env::temp_dir().join(format!("sqlly-engine-ai-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    send(
        &mut stdin,
        &serde_json::json!({"type": "provider_status", "id": "ps-1"}),
    );
    let status = wait_for_value(&rx, "provider_status_report", Duration::from_secs(10))
        .expect("provider status report");
    let report = status.get("report").expect("report");
    assert_eq!(
        report.get("llm_active").and_then(|v| v.as_str()),
        Some("stub"),
        "{report:?}"
    );

    let connection = catalog_connection();
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "sync_catalog",
            "id": "ai-sync",
            "connection": connection.clone(),
            "objects": [
                {"schema": "dbo", "name": "users", "kind": "table", "columns": [], "source": {"kind": "database"}},
            ],
        }),
    );
    assert!(
        wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(10)).is_some(),
        "sync must report"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "review_sql",
            "id": "rv-1",
            "connection": connection.clone(),
            "args": {"sql": "DELETE FROM dbo.users"},
        }),
    );
    let review =
        wait_for_value(&rx, "sql_review_report", Duration::from_secs(15)).expect("review report");
    let report = review.get("report").expect("report");
    assert!(
        report
            .get("validation")
            .and_then(|v| v.get("issues"))
            .and_then(|v| v.as_array())
            .is_some_and(|issues| !issues.is_empty()),
        "an unfiltered DELETE must carry validator findings: {report:?}"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "summarize_query",
            "id": "qs-1",
            "connection": connection,
            "args": {"sql": "SELECT * FROM dbo.users", "result_columns": ["id"], "row_count": 3},
        }),
    );
    let summary = wait_for_value(&rx, "query_summary_report", Duration::from_secs(15))
        .expect("summary report");
    assert!(
        summary
            .get("report")
            .and_then(|r| r.get("summary"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "{summary:?}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    std::fs::remove_dir_all(&dir).ok();
}

/// The engine writes a request/response correlation log: every call in is
/// recorded with its id and type, and every frame out is recorded against the
/// id of the request that triggered it. Driving a real ping over the protocol
/// must leave a REQ line and a correlated RES (pong) line sharing the id, plus
/// the startup `ready` frame. This is the durable debugging trail; an input that
/// crashes the engine would leave its REQ line with no terminal RES.
#[test]
fn engine_writes_correlated_request_response_log() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!("sqlly-engine-it-{nanos}.log"));

    let mut child = spawn_engine_with_log(&log_path);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "log-ping"}),
    );
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "ping answered"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "log-shutdown"}),
    );
    let _ = child.wait();

    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    // Inbound call recorded with id + type.
    assert!(
        contents.contains("REQ id=log-ping type=ping"),
        "request must be logged: {contents}"
    );
    // Outbound response correlated to the same request id.
    assert!(
        contents.contains("RES id=log-ping type=pong"),
        "response must be correlated to the request id: {contents}"
    );
    // The shutdown call is logged too — every call into the engine is captured.
    assert!(
        contents.contains("REQ id=log-shutdown type=shutdown"),
        "shutdown request must be logged: {contents}"
    );
}

/// On a graceful shutdown the engine trims its correlation log down to the last
/// 50 lines, so the file never grows without bound across sessions. We drive
/// enough pings to push the log well past the cap, then send `shutdown` (the
/// clean exit path) and assert the file is trimmed to exactly 50 lines with the
/// most recent activity retained and the oldest discarded.
#[test]
fn graceful_shutdown_trims_log_to_last_50_lines() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!("sqlly-engine-trim-{nanos}.log"));

    let mut child = spawn_engine_with_log(&log_path);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    // Each ping writes 2 log lines (REQ + RES pong). 60 pings => ~120 lines,
    // well past the 50-line cap. Wait for each pong so all lines are on disk.
    for i in 0..60 {
        send(
            &mut stdin,
            &serde_json::json!({"type": "ping", "id": format!("p{i}")}),
        );
        assert!(
            wait_for(&rx, "pong", Duration::from_secs(5)),
            "ping {i} answered"
        );
    }

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "bye"}),
    );
    let _ = child.wait();

    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    let line_count = contents.lines().count();
    assert_eq!(
        line_count, 50,
        "graceful shutdown must trim the log to exactly 50 lines, got {line_count}: {contents}"
    );
    // The most recent activity (the shutdown call and the final pings) is what
    // survives; the earliest pings are discarded.
    assert!(
        contents.contains("REQ id=bye type=shutdown"),
        "the final shutdown call must be among the retained lines: {contents}"
    );
    assert!(
        contents.contains("id=p59"),
        "the most recent ping must be retained: {contents}"
    );
    assert!(
        !contents.contains("id=p0 "),
        "the oldest ping must be trimmed away: {contents}"
    );
    // No staging temp file is left behind next to the log.
    let tmp = log_path.with_extension("log.trim");
    let leftover = tmp.exists();
    let _ = std::fs::remove_file(&tmp);
    assert!(!leftover, "the trim staging temp file must be cleaned up");
}

/// A request the engine cannot turn into a known verb must STILL be recorded
/// (so a malformed/garbage message is attributable to the call that sent it),
/// and it must produce a correlated terminal `request_failed` rather than
/// vanishing.
#[test]
fn engine_logs_unparseable_request_and_fails_it() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let log_path = std::env::temp_dir().join(format!("sqlly-engine-bad-{nanos}.log"));

    let mut child = spawn_engine_with_log(&log_path);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    // Valid JSON, but not a recognized request shape (unknown "type").
    send(
        &mut stdin,
        &serde_json::json!({"type": "bogus", "id": "weird-1"}),
    );
    assert!(
        wait_for(&rx, "request_failed", Duration::from_secs(5)),
        "unparseable request must yield a terminal request_failed"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();

    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);

    // The raw request is recorded (id taken from the body even though it did not
    // deserialize into a known request).
    assert!(
        contents.contains("REQ id=weird-1"),
        "even an unparseable request must be logged: {contents}"
    );
    // It produced a correlated failure carrying the reason — the data needed to
    // debug why the message failed.
    assert!(
        contents.contains("type=request_failed") && contents.contains("error="),
        "failure reason must be logged: {contents}"
    );
}

// ---------------------------------------------------------------------------
// Catalog endpoint integration tests
// ---------------------------------------------------------------------------

/// Spawn an engine with `SQLLY_CATALOG_DIR` pointed at a unique temp directory
/// so the test gets an isolated catalog and never touches real data.
fn spawn_engine_with_catalog(catalog_dir: &std::path::Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("engine")
        .env("SQLLY_CATALOG_DIR", catalog_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine")
}

fn spawn_engine_with_catalog_and_log(
    catalog_dir: &std::path::Path,
    log_path: &std::path::Path,
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_sqlly-cli"))
        .arg("engine")
        .env("SQLLY_CATALOG_DIR", catalog_dir)
        .env("SQLLY_ENGINE_LOG", log_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn sqlly-cli engine")
}

fn unique_catalog_dir(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("sqlly-cat-it-{label}-{nanos}"))
}

fn catalog_connection() -> serde_json::Value {
    serde_json::json!({
        "server": "test.local",
        "port": 1433,
        "database": "appdb",
        "auth": "sql",
        "username": "sa",
        "password": "p",
        "encrypt": true,
        "trust_certificate": true
    })
}

fn catalog_objects() -> serde_json::Value {
    serde_json::json!([
        {
            "schema": "dbo", "name": "Orders", "type": "table",
            "columns": [
                {"name": "OrderId", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "CustomerId", "type_name": "int", "ordinal": 1, "nullable": false}
            ],
            "primary_key": ["OrderId"],
            "foreign_keys": [{"columns": ["CustomerId"], "target_schema": "dbo", "target_table": "Customers", "target_columns": ["CustomerId"]}],
            "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        },
        {
            "schema": "dbo", "name": "Customers", "type": "table",
            "columns": [{"name": "CustomerId", "type_name": "int", "ordinal": 0, "nullable": false}],
            "primary_key": ["CustomerId"],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        }
    ])
}

fn catalog_objects_with_customer_view() -> serde_json::Value {
    serde_json::json!([
        {
            "schema": "dbo", "name": "Orders", "type": "table",
            "columns": [
                {"name": "OrderId", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "CustomerId", "type_name": "int", "ordinal": 1, "nullable": false}
            ],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        },
        {
            "schema": "dbo", "name": "CustomerRollup", "type": "view",
            "columns": [{"name": "CustomerId", "type_name": "int", "ordinal": 0, "nullable": false}],
            "foreign_keys": [], "parameters": [],
            "definition": "CREATE VIEW dbo.CustomerRollup AS SELECT CustomerId FROM dbo.Orders",
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        }
    ])
}

/// `sync_catalog` must yield a terminal `catalog_sync_report` frame over the
/// real subprocess protocol.
#[test]
fn sync_catalog_yields_terminal_frame() {
    let dir = unique_catalog_dir("sync");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(
        wait_for(&rx, "catalog_sync_report", Duration::from_secs(5)),
        "sync_catalog must yield a terminal catalog_sync_report frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `build_ai_cache` must yield a terminal `ai_cache_build_report` frame. The
/// catalog is seeded with a `sync_catalog` first.
#[test]
fn build_ai_cache_yields_terminal_frame() {
    let dir = unique_catalog_dir("build");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(wait_for(&rx, "ready", Duration::from_secs(5)));

    // Seed the catalog.
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(
        wait_for(&rx, "catalog_sync_report", Duration::from_secs(5)),
        "seed sync must succeed before building"
    );

    // Build the AI cache.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "build_ai_cache",
            "id": "bc1",
            "connection": catalog_connection()
        }),
    );
    assert!(
        wait_for(&rx, "ai_cache_build_report", Duration::from_secs(5)),
        "build_ai_cache must yield a terminal ai_cache_build_report frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `ask_schema` must yield a terminal `schema_answer` frame with the stub
/// provider. The catalog is seeded with a `sync_catalog` first.
#[test]
fn ask_schema_yields_terminal_frame() {
    let dir = unique_catalog_dir("ask");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(wait_for(&rx, "ready", Duration::from_secs(5)));

    // Seed the catalog.
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(
        wait_for(&rx, "catalog_sync_report", Duration::from_secs(5)),
        "seed sync must succeed before asking"
    );

    // Ask a question.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "ask_schema",
            "id": "as1",
            "connection": catalog_connection(),
            "question": "How are Orders related to Customers?"
        }),
    );
    assert!(
        wait_for(&rx, "schema_answer", Duration::from_secs(5)),
        "ask_schema must yield a terminal schema_answer frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `ask_schema` must surface `grounded_in` metadata naming the catalog objects
/// the answer was built from — the Phase 6 contract that proves the model only
/// ever sees a bounded, cited slice of the schema.
#[test]
fn ask_schema_response_includes_grounding() {
    let dir = unique_catalog_dir("ask-grounding");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "ask_schema",
            "id": "as1",
            "connection": catalog_connection(),
            "question": "How are Orders related to Customers?"
        }),
    );
    let frame = wait_for_value(&rx, "schema_answer", Duration::from_secs(5))
        .expect("ask_schema must yield a schema_answer frame");

    let grounded = frame["response"]["meta"]["grounded_in"]
        .as_array()
        .expect("schema_answer must carry response.meta.grounded_in as an array");
    assert!(
        !grounded.is_empty(),
        "grounded_in should name the objects the answer was built from"
    );
    let keys: Vec<&str> = grounded
        .iter()
        .filter_map(|g| g["object_key"].as_str())
        .collect();
    assert!(
        keys.contains(&"dbo.orders"),
        "grounded_in should cite dbo.orders, got {keys:?}"
    );
    assert!(
        keys.contains(&"dbo.customers"),
        "grounded_in should cite dbo.customers (FK graph expansion), got {keys:?}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn search_schema_yields_terminal_frame_after_sync() {
    let dir = unique_catalog_dir("search");
    let log_path = std::env::temp_dir().join(format!(
        "sqlly-engine-search-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = spawn_engine_with_catalog_and_log(&dir, &log_path);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "search_schema",
            "id": "ss1",
            "connection": catalog_connection(),
            "query": "CustomerId",
            "limit": 5
        }),
    );
    let frame = wait_for_value(&rx, "schema_search_results", Duration::from_secs(5))
        .expect("search_schema must yield schema_search_results");
    assert!(
        frame["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["name"] == "Orders"),
        "column-only query should find Orders: {frame}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();

    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        contents.contains("REQ id=ss1 type=search_schema"),
        "search request logged: {contents}"
    );
    assert!(
        contents.contains("RES id=ss1 type=schema_search_results"),
        "terminal search result logged: {contents}"
    );
}

#[test]
fn search_schema_respects_object_type_filter_over_protocol() {
    let dir = unique_catalog_dir("search-filter");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary(
            "sc1",
            &catalog_connection(),
            &catalog_objects_with_customer_view(),
        ),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "search_schema",
            "id": "ss-filter",
            "connection": catalog_connection(),
            "query": "CustomerId",
            "limit": 5,
            "object_types": ["view"]
        }),
    );
    let frame = wait_for_value(&rx, "schema_search_results", Duration::from_secs(5))
        .expect("search_schema must yield schema_search_results");
    let results = frame["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "filter should only return the view: {frame}"
    );
    assert_eq!(results[0]["name"], "CustomerRollup");
    assert_eq!(results[0]["object_type"], "view");

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `find_join_path` must yield a terminal `join_path_report` frame after
/// seeding the catalog with FK-linked objects.
#[test]
fn find_join_path_yields_terminal_frame_after_sync() {
    let dir = unique_catalog_dir("join-path");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "find_join_path",
            "id": "fjp1",
            "connection": catalog_connection(),
            "source": "dbo.orders",
            "target": "dbo.customers",
            "max_depth": 4
        }),
    );
    let frame = wait_for_value(&rx, "join_path_report", Duration::from_secs(5))
        .expect("find_join_path must yield join_path_report");
    let paths = frame["paths"].as_array().expect("paths should be an array");
    assert!(!paths.is_empty(), "should find at least one join path");
    assert_eq!(
        paths[0]["steps"].as_array().unwrap().len(),
        1,
        "direct FK should be one step"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `find_join_path` returns an empty paths array when no FK relationship
/// connects the two objects.
#[test]
fn find_join_path_returns_empty_paths_for_disconnected_objects() {
    let dir = unique_catalog_dir("join-nopath");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    let objects = serde_json::json!([
        {
            "schema": "dbo", "name": "Alpha", "type": "table",
            "columns": [{"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false}],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        },
        {
            "schema": "dbo", "name": "Beta", "type": "table",
            "columns": [{"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false}],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        }
    ]);
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &objects),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "find_join_path",
            "id": "fjp-nopath",
            "connection": catalog_connection(),
            "source": "dbo.alpha",
            "target": "dbo.beta",
            "max_depth": 4
        }),
    );
    let frame = wait_for_value(&rx, "join_path_report", Duration::from_secs(5))
        .expect("find_join_path must yield join_path_report even for no-path");
    let paths = frame["paths"].as_array().expect("paths should be an array");
    assert!(
        paths.is_empty(),
        "disconnected objects should return empty paths, got {:?}",
        paths
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `find_join_path` resolves a path through a *name-inferred* relationship when
/// the fixture declares no foreign keys at all. `Orders.CustomerId` infers a
/// reference to `Customers` purely from naming conventions.
#[test]
fn find_join_path_resolves_via_inferred_relationship() {
    let dir = unique_catalog_dir("join-inferred");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    let objects = serde_json::json!([
        {
            "schema": "dbo", "name": "Orders", "type": "table",
            "columns": [
                {"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "CustomerId", "type_name": "int", "ordinal": 1, "nullable": false}
            ],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        },
        {
            "schema": "dbo", "name": "Customers", "type": "table",
            "columns": [
                {"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "Name", "type_name": "nvarchar", "ordinal": 1, "nullable": true}
            ],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        }
    ]);
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &objects),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "find_join_path",
            "id": "fjp-inf",
            "connection": catalog_connection(),
            "source": "dbo.orders",
            "target": "dbo.customers",
            "max_depth": 4
        }),
    );
    let frame = wait_for_value(&rx, "join_path_report", Duration::from_secs(5))
        .expect("find_join_path must yield join_path_report");
    let paths = frame["paths"].as_array().expect("paths should be an array");
    assert!(
        !paths.is_empty(),
        "inferred relationship should yield a join path"
    );
    let steps = paths[0]["steps"].as_array().expect("steps array");
    assert_eq!(steps.len(), 1, "inferred reference is one hop");
    assert_eq!(
        steps[0]["is_inferred"], true,
        "the resolving step should be flagged inferred, got {:?}",
        steps[0]
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `find_join_path` with `include_inferred: false` traverses declared FKs only,
/// so an FK-less catalog yields no path even though an inferred edge exists.
#[test]
fn find_join_path_strict_mode_excludes_inferred_relationship() {
    let dir = unique_catalog_dir("join-strict");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    let objects = serde_json::json!([
        {
            "schema": "dbo", "name": "Orders", "type": "table",
            "columns": [
                {"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "CustomerId", "type_name": "int", "ordinal": 1, "nullable": false}
            ],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        },
        {
            "schema": "dbo", "name": "Customers", "type": "table",
            "columns": [
                {"name": "Id", "type_name": "int", "ordinal": 0, "nullable": false},
                {"name": "Name", "type_name": "nvarchar", "ordinal": 1, "nullable": true}
            ],
            "foreign_keys": [], "parameters": [],
            "source": {"type": "database", "create_date": 1700000000000i64, "modify_date": 1700000000000i64}
        }
    ]);
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &objects),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "find_join_path",
            "id": "fjp-strict",
            "connection": catalog_connection(),
            "source": "dbo.orders",
            "target": "dbo.customers",
            "max_depth": 4,
            "include_inferred": false
        }),
    );
    let frame = wait_for_value(&rx, "join_path_report", Duration::from_secs(5))
        .expect("strict find_join_path must still yield a terminal join_path_report");
    let paths = frame["paths"].as_array().expect("paths should be an array");
    assert!(
        paths.is_empty(),
        "declared-only traversal must not resolve the inferred edge, got {paths:?}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `find_join_path` finds a reverse-direction path (target → source via inbound FK).
#[test]
fn find_join_path_finds_reverse_direction() {
    let dir = unique_catalog_dir("join-reverse");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "find_join_path",
            "id": "fjp-rev",
            "connection": catalog_connection(),
            "source": "dbo.customers",
            "target": "dbo.orders",
            "max_depth": 4
        }),
    );
    let frame = wait_for_value(&rx, "join_path_report", Duration::from_secs(5))
        .expect("find_join_path reverse must yield join_path_report");
    let paths = frame["paths"].as_array().expect("paths should be an array");
    assert!(!paths.is_empty(), "reverse path should exist");
    assert_eq!(
        paths[0]["steps"].as_array().unwrap().len(),
        1,
        "reverse path should be one step"
    );
    assert_eq!(paths[0]["steps"][0]["direction"], "reverse");

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `build_embeddings` must yield a terminal `embeddings_build_report` frame
/// after `sync_catalog` has been called.
#[test]
fn build_embeddings_yields_terminal_frame_after_sync_catalog() {
    let dir = unique_catalog_dir("build-embed");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "build_embeddings",
            "id": "be1",
            "connection": catalog_connection()
        }),
    );
    let frame = wait_for_value(&rx, "embeddings_build_report", Duration::from_secs(10))
        .expect("build_embeddings must yield embeddings_build_report");
    let report = &frame["report"];
    assert!(
        report["embedded"].as_u64().unwrap_or(0) > 0,
        "should have embedded at least one object"
    );
    assert_eq!(
        report["failed"].as_u64().unwrap_or(0),
        0,
        "no failures expected"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// After `build_embeddings`, `search_schema` should still return
/// `schema_search_results` (now using hybrid internally).
#[test]
fn search_schema_after_build_embeddings_returns_schema_search_results() {
    let dir = unique_catalog_dir("search-after-embed");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "build_embeddings",
            "id": "be1",
            "connection": catalog_connection()
        }),
    );
    assert!(wait_for_value(&rx, "embeddings_build_report", Duration::from_secs(10)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "search_schema",
            "id": "ss1",
            "connection": catalog_connection(),
            "query": "orders"
        }),
    );
    let frame = wait_for_value(&rx, "schema_search_results", Duration::from_secs(5))
        .expect("search_schema must yield schema_search_results after build_embeddings");
    let results = frame["results"]
        .as_array()
        .expect("results should be an array");
    assert!(!results.is_empty(), "should find results for 'orders'");

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// After `build_embeddings`, exact identifier search still works.
#[test]
fn search_schema_exact_identifier_still_works_after_build_embeddings() {
    let dir = unique_catalog_dir("exact-after-embed");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "build_embeddings",
            "id": "be1",
            "connection": catalog_connection()
        }),
    );
    assert!(wait_for_value(&rx, "embeddings_build_report", Duration::from_secs(10)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "search_schema",
            "id": "ss1",
            "connection": catalog_connection(),
            "query": "customers"
        }),
    );
    let frame = wait_for_value(&rx, "schema_search_results", Duration::from_secs(5))
        .expect("search_schema must yield schema_search_results");
    let results = frame["results"]
        .as_array()
        .expect("results should be an array");
    assert!(!results.is_empty(), "should find results for 'customers'");
    let first_name = results[0]["name"].as_str().unwrap_or("");
    assert!(
        first_name.to_ascii_lowercase().contains("customers"),
        "exact identifier 'customers' should rank the Customers table first, got: {first_name}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `validate_sql` for a well-formed SELECT against synced objects must yield a
/// terminal `sql_validation_report` frame reporting the SQL as valid.
#[test]
fn validate_sql_known_select_yields_valid_report() {
    let dir = unique_catalog_dir("validate-ok");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "validate_sql",
            "id": "vs1",
            "connection": catalog_connection(),
            "sql": "select o.OrderId from dbo.Orders o where o.OrderId = 1"
        }),
    );
    let frame = wait_for_value(&rx, "sql_validation_report", Duration::from_secs(5))
        .expect("validate_sql must yield a terminal sql_validation_report frame");
    let report = &frame["report"];
    assert_eq!(
        report["is_valid"].as_bool(),
        Some(true),
        "known SELECT should be valid: {report}"
    );
    assert_eq!(report["detected_intent"].as_str(), Some("select"));
    let objects = report["referenced_objects"]
        .as_array()
        .expect("referenced_objects array");
    assert!(
        objects
            .iter()
            .any(|object| object["object_key"].as_str() == Some("dbo.orders")),
        "referenced_objects should include dbo.orders: {report}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `validate_sql` for a SELECT referencing an unknown table must report an
/// `unresolved_table` error in its terminal `sql_validation_report` frame.
#[test]
fn validate_sql_unknown_table_reports_error() {
    let dir = unique_catalog_dir("validate-unknown");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "validate_sql",
            "id": "vs1",
            "connection": catalog_connection(),
            "sql": "select * from dbo.Nope"
        }),
    );
    let frame = wait_for_value(&rx, "sql_validation_report", Duration::from_secs(5))
        .expect("validate_sql must yield a terminal sql_validation_report frame");
    let report = &frame["report"];
    assert_eq!(report["is_valid"].as_bool(), Some(false));
    let issues = report["issues"].as_array().expect("issues array");
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"].as_str() == Some("unresolved_table")),
        "unknown table should produce an unresolved_table issue: {report}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Unconstrained-mutation warnings are off by default: a `DELETE` with no
/// `WHERE` clause validates clean unless the connection opts in via
/// `confirm_data_changes`, in which case it reports `delete_without_where`.
#[test]
fn validate_sql_delete_without_where_errors() {
    let dir = unique_catalog_dir("validate-delete");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "validate_sql",
            "id": "vs1",
            "connection": catalog_connection(),
            "sql": "delete from dbo.Orders"
        }),
    );
    let frame = wait_for_value(&rx, "sql_validation_report", Duration::from_secs(5))
        .expect("validate_sql must yield a terminal sql_validation_report frame");
    let report = &frame["report"];
    assert_eq!(
        report["is_valid"].as_bool(),
        Some(true),
        "unconstrained mutations are not flagged by default: {report}"
    );
    assert_eq!(report["detected_intent"].as_str(), Some("delete"));

    let mut confirming = catalog_connection();
    confirming["confirm_data_changes"] = serde_json::json!(true);
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "validate_sql",
            "id": "vs2",
            "connection": confirming,
            "sql": "delete from dbo.Orders"
        }),
    );
    let frame = wait_for_value(&rx, "sql_validation_report", Duration::from_secs(5))
        .expect("opted-in validate_sql must yield a terminal sql_validation_report frame");
    let report = &frame["report"];
    assert_eq!(report["is_valid"].as_bool(), Some(false));
    assert_eq!(report["detected_intent"].as_str(), Some("delete"));
    let issues = report["issues"].as_array().expect("issues array");
    assert!(
        issues
            .iter()
            .any(|issue| issue["code"].as_str() == Some("delete_without_where")),
        "DELETE without WHERE should produce a delete_without_where issue \
         when the connection confirms data changes: {report}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `generate_sql` against a synced catalog must yield a terminal
/// `sql_generation_report` frame carrying a non-empty SQL string and a nested
/// validation report. The correlation log must also record the bounded
/// `attempts=N` detail for the terminal frame.
#[test]
fn generate_sql_after_sync_yields_report() {
    let dir = unique_catalog_dir("generate");
    let log_path = std::env::temp_dir().join(format!(
        "sqlly-engine-generate-{}.log",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut child = spawn_engine_with_catalog_and_log(&dir, &log_path);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "generate_sql",
            "id": "gs1",
            "connection": catalog_connection(),
            "question": "show all orders"
        }),
    );
    let frame = wait_for_value(&rx, "sql_generation_report", Duration::from_secs(10))
        .expect("generate_sql must yield a terminal sql_generation_report frame");
    let report = &frame["report"];
    assert!(
        !report["sql"].as_str().unwrap_or("").trim().is_empty(),
        "generated SQL should be non-empty: {report}"
    );
    assert!(
        report["validation"].is_object(),
        "generation report should carry a nested validation report: {report}"
    );
    assert!(
        report["attempts"]
            .as_array()
            .map(|a| !a.is_empty())
            .unwrap_or(false),
        "generation report should carry at least one attempt: {report}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();

    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    let _ = std::fs::remove_file(&log_path);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        contents.contains("RES id=gs1 type=sql_generation_report"),
        "terminal generation frame logged: {contents}"
    );
    assert!(
        contents.contains("attempts="),
        "terminal generation frame log must record the bounded attempt count: {contents}"
    );
}

/// `generate_sql` with an explicit `max_attempts` on the wire must still yield a
/// terminal `sql_generation_report` whose recorded attempts never exceed the
/// requested bound.
#[test]
fn generate_sql_with_max_attempts_yields_bounded_report() {
    let dir = unique_catalog_dir("generate_attempts");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "generate_sql",
            "id": "gs-attempts",
            "connection": catalog_connection(),
            "question": "show all orders",
            "max_attempts": 1
        }),
    );
    let frame = wait_for_value(&rx, "sql_generation_report", Duration::from_secs(10))
        .expect("generate_sql with max_attempts must yield a terminal frame");
    let attempts = frame["report"]["attempts"]
        .as_array()
        .expect("attempts array present");
    assert!(
        !attempts.is_empty() && attempts.len() <= 1,
        "attempts must be bounded by the requested max_attempts=1: {}",
        frame["report"]
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// `invalidate_ai_cache` nukes all AI-derived data (card cache + embeddings)
/// and yields a terminal `invalidate_ai_cache_report` frame with the counts.
#[test]
fn invalidate_ai_cache_yields_report() {
    let dir = unique_catalog_dir("invalidate-ai-cache");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some());

    // Sync some objects so there is catalog data to nuke.
    send_raw(
        &mut stdin,
        &encode_sync_catalog_binary("sc1", &catalog_connection(), &catalog_objects()),
    );
    assert!(wait_for_value(&rx, "catalog_sync_report", Duration::from_secs(5)).is_some());

    // Build AI cache so there is valid cache to invalidate.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "build_ai_cache",
            "id": "bac1",
            "connection": catalog_connection()
        }),
    );
    assert!(wait_for_value(&rx, "ai_cache_build_report", Duration::from_secs(10)).is_some());

    // Nuke AI data and verify the report carries real counts.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "invalidate_ai_cache",
            "id": "iac1",
            "connection": catalog_connection()
        }),
    );
    let frame = wait_for_value(&rx, "invalidate_ai_cache_report", Duration::from_secs(5))
        .expect("invalidate_ai_cache must yield a terminal frame");
    let report = &frame["report"];
    let invalidated = report["invalidated"].as_u64().unwrap_or(0);
    assert!(
        invalidated > 0,
        "invalidate should flip at least one valid AI cache entry: {frame}"
    );
    // Embeddings may or may not exist depending on provider; just assert the
    // field is present.
    assert!(
        report["embeddings_deleted"].is_u64(),
        "report should carry embeddings_deleted: {frame}"
    );

    // Second nuke should report zero invalidated (nothing left to flip).
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "invalidate_ai_cache",
            "id": "iac2",
            "connection": catalog_connection()
        }),
    );
    let frame2 = wait_for_value(&rx, "invalidate_ai_cache_report", Duration::from_secs(5))
        .expect("second invalidate must yield a terminal frame");
    assert_eq!(
        frame2["report"]["invalidated"].as_u64().unwrap_or(0),
        0,
        "second nuke should find nothing left to invalidate: {frame2}"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `frame_type_reader` must be tag-aware: JSON frames (0x00) yield their
/// `type` field, while binary frames (0x01 validation, 0x02 rows_batch) yield a
/// synthetic type string. This test verifies the reader handles all three tags
/// without breaking, by sending a `ping` (JSON `pong` response) and confirming
/// the type is reported correctly.
#[test]
fn frame_reader_is_tag_aware() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "engine should emit a ready frame on startup"
    );

    send(&mut stdin, &serde_json::json!({"type": "ping", "id": "p1"}));
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "ping should yield a pong frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Subprocess test that sends a `validate` request and decodes the binary
/// validation_report body (tag 0x01) from the real engine, verifying the
/// flat binary format: `[u32 id_len][id][u32 diag_count]` then per diagnostic
/// `[u8 severity][u8 class][u32 span_start][u32 span_len][u32 line][u32 col][u32 msg_len][msg]`,
/// followed by `[u32 resolved_count]` then per span `[u32 start][u32 len]`.
#[test]
fn validate_request_yields_decodable_binary_body() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_raw_reader(&mut child);

    assert!(
        wait_for_raw_json(&rx, "ready", Duration::from_secs(5)).is_some(),
        "engine should emit a ready frame on startup"
    );

    send_raw(
        &mut stdin,
        &encode_validate_binary(
            "bin-v1",
            "SELECT * FROM Shared.MovementLine",
            Some("sqlserver"),
            Some("dbo"),
            &[("dbo", "MovementLine", "table", &["MovementLineId"])],
        ),
    );

    let body = wait_for_raw_binary(&rx, FRAME_TAG_VALIDATION, Duration::from_secs(5))
        .expect("validate must yield a binary validation_report frame");

    let mut cur = 0usize;
    let id_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    let id = std::str::from_utf8(&body[cur..cur + id_len]).unwrap();
    assert_eq!(id, "bin-v1");
    cur += id_len;

    let diag_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;

    assert!(
        diag_count > 0,
        "should have at least one diagnostic for unresolved object"
    );

    for _ in 0..diag_count {
        let _severity = body[cur];
        cur += 1;
        let _class = body[cur];
        cur += 1;
        let _span_start = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let _span_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let _line = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let _col = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let msg_len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
        cur += 4;
        let _msg = std::str::from_utf8(&body[cur..cur + msg_len]).unwrap();
        cur += msg_len;
    }

    let resolved_count = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap()) as usize;
    cur += 4;
    for _ in 0..resolved_count {
        let _start = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let _len = u32::from_le_bytes(body[cur..cur + 4].try_into().unwrap());
        cur += 4;
    }

    assert_eq!(
        cur,
        body.len(),
        "must consume exactly all body bytes with no trailing data"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Protocol v2 binary request frame tests
// ---------------------------------------------------------------------------

/// The ready frame must report `protocol_version: 2` after the v2 cutover.
#[test]
fn ready_frame_reports_protocol_version_2() {
    let mut child = spawn_engine();
    let rx = frame_value_reader(&mut child);

    let ready = wait_for_value(&rx, "ready", Duration::from_secs(5))
        .expect("engine should emit a ready frame");
    assert_eq!(
        ready.get("protocol_version").and_then(|v| v.as_u64()),
        Some(2),
        "protocol version must be 2"
    );

    drop(child.kill());
    let _ = child.wait();
}

/// Send a binary validate request (tag 0x10) and verify the engine yields a
/// binary validation_report frame (tag 0x01) with the correct request id.
#[test]
fn binary_validate_request_yields_validation_report() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_raw_reader(&mut child);

    assert!(
        wait_for_raw_json(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    let body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bin-validate-1")
        .string("SELECT * FROM dbo.MyTable")
        .opt_string(Some("sqlserver"))
        .opt_string(Some("dbo"))
        .u32_le(1) // object_count
        .string("dbo")
        .string("MyTable")
        .u8(0) // kind = table
        .u32_le(1) // column_count
        .string("Id")
        .u32_le(0) // parameter_count
        .u32_le(0) // foreign_key_count
        .done();

    let mut payload = vec![REQUEST_TAG_VALIDATE];
    payload.extend_from_slice(&body);
    send_raw(&mut stdin, &payload);

    let body = wait_for_raw_binary(&rx, FRAME_TAG_VALIDATION, Duration::from_secs(5))
        .expect("binary validate must yield a validation_report frame");

    let id_len = u32::from_le_bytes(body[0..4].try_into().unwrap()) as usize;
    let id = std::str::from_utf8(&body[4..4 + id_len]).unwrap();
    assert_eq!(id, "bin-validate-1");

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Send a binary sync_catalog request (tag 0x11) and verify the engine yields
/// a `catalog_sync_report` terminal frame.
#[test]
fn binary_sync_catalog_request_yields_terminal_frame() {
    let dir = unique_catalog_dir("bin-sync");
    let mut child = spawn_engine_with_catalog(&dir);
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(
        wait_for(&rx, "ready", Duration::from_secs(5)),
        "ready frame"
    );

    let connection_json = serde_json::to_string(&catalog_connection()).unwrap();

    let body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("bin-sync-1")
        .string(&connection_json)
        .u32_le(1) // object_count
        .opt_string(Some("dbo"))
        .string("Orders")
        .u8(0) // kind = table
        .u32_le(2) // column_count
        .string("OrderId")
        .opt_string(Some("int"))
        .i64_le(0)
        .bool(false)
        .string("CustomerId")
        .opt_string(Some("int"))
        .i64_le(1)
        .bool(false)
        .u32_le(1) // primary_key_count
        .string("OrderId")
        .u32_le(1) // foreign_key_count
        .u32_le(1) // local_column_count
        .string("CustomerId")
        .opt_string(Some("dbo"))
        .string("Customers")
        .u32_le(1) // target_column_count
        .string("CustomerId")
        .u32_le(0) // parameter_count
        .opt_string(None) // definition
        .u8(0) // source_kind = database
        .opt_string(None) // path
        .opt_i64(Some(1700000000000))
        .opt_i64(Some(1700000000000))
        .done();

    let mut payload = vec![REQUEST_TAG_SYNC_CATALOG];
    payload.extend_from_slice(&body);
    send_raw(&mut stdin, &payload);

    assert!(
        wait_for(&rx, "catalog_sync_report", Duration::from_secs(5)),
        "binary sync_catalog must yield a terminal catalog_sync_report frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A malformed binary validate request must yield `request_failed` and the
/// engine must stay alive — a subsequent `ping` must still get a `pong`.
#[test]
fn malformed_binary_validate_yields_request_failed_and_engine_survives() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(wait_for(&rx, "ready", Duration::from_secs(5)));

    let payload = [REQUEST_TAG_VALIDATE, 0xFF];
    send_raw(&mut stdin, &payload);

    assert!(
        wait_for(&rx, "request_failed", Duration::from_secs(5)),
        "malformed binary validate must yield request_failed"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "alive"}),
    );
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "engine must survive malformed binary and respond to ping"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// An unknown binary request tag must yield `request_failed`.
#[test]
fn unknown_binary_tag_yields_request_failed() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(wait_for(&rx, "ready", Duration::from_secs(5)));

    let payload = [0xFE, 0x00];
    send_raw(&mut stdin, &payload);

    assert!(
        wait_for(&rx, "request_failed", Duration::from_secs(5)),
        "unknown binary tag must yield request_failed"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// JSON control requests (ping, shutdown) must continue to work under
/// protocol v2 — the untagged JSON path is preserved for non-schema requests.
#[test]
fn json_control_requests_still_work_under_protocol_v2() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_type_reader(&mut child);

    assert!(wait_for(&rx, "ready", Duration::from_secs(5)));

    send(&mut stdin, &serde_json::json!({"type": "ping", "id": "p1"}));
    assert!(
        wait_for(&rx, "pong", Duration::from_secs(5)),
        "JSON ping must still work"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    assert!(
        wait_for(&rx, "shutdown_ack", Duration::from_secs(5)),
        "JSON shutdown must still work"
    );
    let _ = child.wait();
}

/// Bare JSON requests (starting with `{` = 0x7B) are rejected as unknown
/// tags under the fully binary protocol, and the engine survives.
#[test]
fn bare_json_requests_are_rejected_as_unknown_tag() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Bare JSON validate must be rejected as unknown tag 0x7B.
    let json_validate = br#"{"type":"validate","id":"jv-reject","sql":"SELECT 1","dialect":"sqlserver","default_schema":"dbo","objects":[],"foreign_keys":[]}"#;
    send_raw(&mut stdin, json_validate);
    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("bare JSON validate must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("unknown"));
    assert!(
        frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown request tag"),
        "error should mention unknown tag: {frame:?}"
    );

    // Bare JSON sync_catalog must be rejected.
    let json_sync = serde_json::to_vec(&serde_json::json!({
        "type": "sync_catalog",
        "id": "js-reject",
        "connection": catalog_connection(),
        "objects": []
    }))
    .unwrap();
    send_raw(&mut stdin, &json_sync);
    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("bare JSON sync_catalog must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("unknown"));
    assert!(
        frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("unknown request tag"),
        "error should mention unknown tag: {frame:?}"
    );

    // Engine must still be alive — send a ping.
    send_raw(&mut stdin, &ping_frame("after-reject"));
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive bare JSON rejection"
    );

    send_raw(&mut stdin, &shutdown_frame("s1"));
    let _ = child.wait();
}

// ───────────────────────────────────────────────────────────────────
// Protocol survival tests
//
// These verify that malformed binary frames do not crash or wedge the
// engine, and that the engine stays alive for subsequent valid requests.
// ───────────────────────────────────────────────────────────────────

/// Maximum frame size — must match `MAX_FRAME_BYTES` in engine.rs.
const MAX_FRAME_BYTES: u32 = 16 * 1024 * 1024;

/// Empty payload frame yields `request_failed`, then a subsequent `ping`
/// succeeds — the engine must not crash or wedge on an empty frame.
#[test]
fn empty_payload_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send an empty payload (0-length frame).
    send_raw(&mut stdin, &[]);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("empty payload must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("unknown"));

    // Engine must still be alive — ping must succeed.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-empty"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive empty payload"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Unknown request tag yields `request_failed`, then a subsequent `ping`
/// succeeds — the engine must not crash on an unrecognized tag.
#[test]
fn unknown_request_tag_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a frame with an unknown tag byte.
    send_raw(&mut stdin, &[0x99, 0x01, 0x02]);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("unknown tag must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("unknown"));

    // Engine must still be alive — ping must succeed.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-unknown"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive unknown tag"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Malformed binary validate request yields `request_failed` with the
/// recoverable id, then a subsequent ping succeeds.
#[test]
fn malformed_validate_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a malformed validate binary frame with a recoverable id.
    // Build the body up to the id, then corrupt the dialect presence byte.
    let mut bad_body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("mal-1")
        .string("SELECT 1")
        .done();
    bad_body.push(0x02); // invalid dialect presence byte
    let mut bad_frame = vec![REQUEST_TAG_VALIDATE];
    bad_frame.extend_from_slice(&bad_body);
    send_raw(&mut stdin, &bad_frame);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("malformed validate must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("mal-1"));
    assert!(
        frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("invalid presence"),
        "error should mention invalid presence: {frame:?}"
    );

    // Engine must still be alive — ping must succeed.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-mal"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive malformed validate"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Malformed binary sync_catalog request yields `request_failed` with the
/// recoverable id, then a subsequent ping succeeds.
#[test]
fn malformed_sync_catalog_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a malformed sync_catalog binary frame with a recoverable id.
    // Build the body up to the id, then use invalid JSON for connection_json.
    let bad_body = BinaryFrameWriter::new()
        .u16_le(BINARY_REQUEST_LAYOUT_VERSION)
        .string("sync-mal-1")
        .string("{bad json}")
        .u32_le(0)
        .done();
    let mut bad_frame = vec![REQUEST_TAG_SYNC_CATALOG];
    bad_frame.extend_from_slice(&bad_body);
    send_raw(&mut stdin, &bad_frame);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("malformed sync_catalog must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("sync-mal-1"));
    assert!(
        frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("connection_json"),
        "error should mention connection_json: {frame:?}"
    );

    // Engine must still be alive — ping must succeed.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-sync-mal"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive malformed sync_catalog"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Unsupported validate layout version yields `request_failed` with the
/// recoverable id, then a subsequent ping succeeds.
#[test]
fn unsupported_validate_layout_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a validate binary frame with an unsupported layout version.
    let bad_body = BinaryFrameWriter::new()
        .u16_le(999)
        .string("ver-bad")
        .string("SELECT 1")
        .opt_string(None)
        .opt_string(None)
        .u32_le(0)
        .u32_le(0)
        .done();
    let mut bad_frame = vec![REQUEST_TAG_VALIDATE];
    bad_frame.extend_from_slice(&bad_body);
    send_raw(&mut stdin, &bad_frame);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("unsupported layout must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("ver-bad"));
    assert!(
        frame["error"]
            .as_str()
            .unwrap_or("")
            .contains("unsupported"),
        "error should mention unsupported: {frame:?}"
    );

    // Engine must still be alive — ping must succeed.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-ver"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive unsupported layout"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Oversized inbound length prefix (MAX_FRAME_BYTES + 1) causes the engine
/// to exit cleanly without OOM or hang. The engine's stdin reader rejects
/// the oversized length and returns, causing the request loop to end.
#[test]
fn oversized_length_prefix_exits_cleanly() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "engine should emit ready before oversized length test"
    );

    // Send a 4-byte length prefix that claims MAX_FRAME_BYTES + 1.
    let oversize_len = MAX_FRAME_BYTES + 1;
    stdin
        .write_all(&oversize_len.to_le_bytes())
        .expect("write oversized length prefix");
    stdin.flush().expect("flush");

    // Close stdin so the engine's reader gets EOF after rejecting the length.
    drop(stdin);

    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(10)),
        "engine must exit after oversized length prefix"
    );
}

/// EOF mid-payload causes the engine to exit cleanly without hang.
/// The engine starts reading a frame (length prefix says 100 bytes) but
/// only 50 bytes arrive before stdin closes.
#[test]
fn eof_mid_payload_exits_cleanly() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "engine should emit ready before EOF mid-payload test"
    );

    // Send a length prefix claiming 100 bytes, then only send 50.
    stdin
        .write_all(&100u32.to_le_bytes())
        .expect("write length prefix");
    stdin.write_all(&[0xAB; 50]).expect("write partial payload");
    stdin.flush().expect("flush");

    // Close stdin mid-payload.
    drop(stdin);

    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(10)),
        "engine must exit after EOF mid-payload"
    );
}

/// EOF mid-length-prefix (only 2 of 4 bytes) causes the engine to exit
/// cleanly without hang.
#[test]
fn eof_mid_length_prefix_exits_cleanly() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "engine should emit ready before EOF mid-length-prefix test"
    );

    // Send only 2 bytes of a 4-byte length prefix, then close stdin.
    stdin
        .write_all(&[0x01, 0x00])
        .expect("write partial length prefix");
    stdin.flush().expect("flush");
    drop(stdin);

    assert!(
        wait_for_child_exit(&mut child, Duration::from_secs(10)),
        "engine must exit after EOF mid-length-prefix"
    );
}

/// Binary control requests (ping, shutdown) must work under the fully
/// binary protocol.
#[test]
fn binary_control_requests_work() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a binary ping.
    send_raw(&mut stdin, &ping_frame("bp1"));

    let frame =
        wait_for_value(&rx, "pong", Duration::from_secs(5)).expect("binary ping must yield pong");
    assert_eq!(frame["id"].as_str(), Some("bp1"));

    // Send a binary shutdown.
    send_raw(&mut stdin, &shutdown_frame("bs1"));

    assert!(
        wait_for_value(&rx, "shutdown_ack", Duration::from_secs(5)).is_some(),
        "binary shutdown must yield shutdown_ack"
    );
    let _ = child.wait();
}

/// Unknown tag with a malformed body yields `request_failed` with id
/// "unknown" (body doesn't parse so id can't be recovered), then the
/// engine survives.
#[test]
fn unknown_tag_malformed_yields_request_failed_then_ping_succeeds() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send an unknown tag (0x99) with garbage body.
    let bad = [0x99u8, 0x01, 0x02, 0x03];
    send_raw(&mut stdin, &bad);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("unknown tag must yield request_failed");
    assert_eq!(frame["id"].as_str(), Some("unknown"));

    // Engine survives.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-ut"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive unknown tag"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Binary validate request truncated before the id (body too short to
/// read layout_version + id) yields `request_failed` with id "unknown".
#[test]
fn binary_validate_truncated_before_id_yields_unknown_id() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send 0x10 + only 1 byte — not enough for u16 layout_version.
    send_raw(&mut stdin, &[REQUEST_TAG_VALIDATE, 0x01]);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("truncated validate must yield request_failed");
    assert_eq!(
        frame["id"].as_str(),
        Some("unknown"),
        "id should be 'unknown' when body is too short to recover id"
    );

    // Engine survives.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-vtrunc"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive truncated validate"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Binary sync_catalog request truncated before the id yields
/// `request_failed` with id "unknown".
#[test]
fn binary_sync_catalog_truncated_before_id_yields_unknown_id() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send 0x11 + only 1 byte — not enough for u16 layout_version.
    send_raw(&mut stdin, &[REQUEST_TAG_SYNC_CATALOG, 0x01]);

    let frame = wait_for_value(&rx, "request_failed", Duration::from_secs(5))
        .expect("truncated sync_catalog must yield request_failed");
    assert_eq!(
        frame["id"].as_str(),
        Some("unknown"),
        "id should be 'unknown' when body is too short to recover id"
    );

    // Engine survives.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-strunc"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive truncated sync_catalog"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Cancel while a long-running SQLite query is in flight stops the query and
/// returns `request_cancelled` for the cancel request.
#[test]
fn cancel_in_flight_sqlite_query_yields_request_cancelled() {
    let dir = std::env::temp_dir().join(format!("sqlly-engine-cancel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db_path = dir.join("cancel-io.db");
    let _ = std::fs::remove_file(&db_path);

    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "execute_query",
            "id": "q-long",
            "connection": {
                "server": db_path.to_string_lossy(),
                "port": 0,
                "username": "",
                "password": "",
                "database_type": "Sqlite"
            },
            "sql": "WITH RECURSIVE c(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM c) SELECT count(*) FROM c"
        }),
    );

    assert!(
        wait_for_value(&rx, "request_started", Duration::from_secs(5)).is_some(),
        "long query must start before cancel"
    );

    send(
        &mut stdin,
        &serde_json::json!({
            "type": "cancel",
            "id": "cancel-1",
            "target_id": "q-long"
        }),
    );

    let frame = wait_for_value(&rx, "request_cancelled", Duration::from_secs(5))
        .expect("in-flight cancel must yield request_cancelled");
    assert_eq!(frame["id"].as_str(), Some("cancel-1"));
    assert_eq!(frame["target_id"].as_str(), Some("q-long"));

    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-cancel"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive in-flight cancel"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
    let _ = std::fs::remove_file(&db_path);
}

/// Cancel for an unknown target_id is idempotent — the engine always
/// returns `request_cancelled` even if the target was never active.
#[test]
fn cancel_unknown_target_yields_request_cancelled() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Cancel a target that was never sent — engine should still respond.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "cancel",
            "id": "cancel-1",
            "target_id": "nonexistent-target"
        }),
    );

    let frame = wait_for_value(&rx, "request_cancelled", Duration::from_secs(5))
        .expect("cancel must yield request_cancelled");
    assert_eq!(frame["id"].as_str(), Some("cancel-1"));
    assert_eq!(
        frame["target_id"].as_str(),
        Some("nonexistent-target"),
        "target_id should be echoed back"
    );

    // Engine survives — ping works.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-cancel"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive cancel of unknown target"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}

/// Cancel after the target already completed normally — the engine
/// should still return `request_cancelled` (idempotent), and the
/// next request should work fine.
#[test]
fn cancel_after_completion_yields_request_cancelled_and_engine_survives() {
    let mut child = spawn_engine();
    let mut stdin = child.stdin.take().expect("child stdin");
    let rx = frame_value_reader(&mut child);

    assert!(
        wait_for_value(&rx, "ready", Duration::from_secs(5)).is_some(),
        "ready frame"
    );

    // Send a ping (inline, completes immediately).
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "ping-1"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "ping must complete"
    );

    // Now cancel the already-completed ping — should still get request_cancelled.
    send(
        &mut stdin,
        &serde_json::json!({
            "type": "cancel",
            "id": "cancel-1",
            "target_id": "ping-1"
        }),
    );
    let frame = wait_for_value(&rx, "request_cancelled", Duration::from_secs(5))
        .expect("cancel after completion must still yield request_cancelled");
    assert_eq!(frame["id"].as_str(), Some("cancel-1"));
    assert_eq!(frame["target_id"].as_str(), Some("ping-1"));

    // Engine survives.
    send(
        &mut stdin,
        &serde_json::json!({"type": "ping", "id": "after-cancel-2"}),
    );
    assert!(
        wait_for_value(&rx, "pong", Duration::from_secs(5)).is_some(),
        "engine must survive cancel after completion"
    );

    send(
        &mut stdin,
        &serde_json::json!({"type": "shutdown", "id": "s1"}),
    );
    let _ = child.wait();
}
