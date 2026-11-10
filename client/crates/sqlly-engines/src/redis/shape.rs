//! Redis replies into result sets.
//!
//! Every command answers with one result set. The shape follows the reply
//! first and the command second: scalars (status, bulk string, integer,
//! double, boolean, nil) become a one-row `result` column, arrays one row
//! per element with an `index` column (nested arrays rendered as compact
//! JSON), RESP3 maps and sets their natural rows. A few commands whose
//! flat-array replies carry structure are shaped by name so the grid shows
//! it: `HGETALL`/`CONFIG GET` as field/value pairs, `ZRANGE … WITHSCORES`
//! as member/score, the `SCAN` family as cursor + element, `INFO` as
//! section/key/value, `CLIENT LIST` as one row per client, `SLOWLOG GET` as
//! one row per entry, and `XRANGE`/`XREVRANGE` as id + fields JSON.

use redis::Value;
use sqlly_core::{CellValue, ResultColumn};

use super::info::{parse_client_list, parse_info};

/// A shaped reply: columns plus rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Shaped {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<CellValue>>,
}

impl Shaped {
    pub fn new(columns: &[(&str, &str)], rows: Vec<Vec<CellValue>>) -> Self {
        Shaped {
            columns: columns.iter().map(|(n, t)| column(n, t)).collect(),
            rows,
        }
    }

    fn scalar(cell: CellValue, data_type: &str) -> Self {
        Shaped::new(&[("result", data_type)], vec![vec![cell]])
    }
}

pub(crate) fn column(name: &str, data_type: &str) -> ResultColumn {
    ResultColumn {
        name: name.to_string(),
        data_type: data_type.to_string(),
        nullable: true,
    }
}

/// Bulk strings are shown as text when they are valid UTF-8, bytes otherwise.
pub(crate) fn bulk_cell(bytes: Vec<u8>) -> CellValue {
    match String::from_utf8(bytes) {
        Ok(text) => CellValue::String(text),
        Err(error) => CellValue::Bytes(error.into_bytes()),
    }
}

/// One reply value as a grid cell: scalars directly, containers as JSON.
pub(crate) fn cell(value: Value) -> CellValue {
    match value {
        Value::Nil => CellValue::Null,
        Value::Int(n) => CellValue::I64(n),
        Value::BulkString(bytes) => bulk_cell(bytes),
        Value::SimpleString(s) => CellValue::String(s),
        Value::Okay => CellValue::String("OK".to_string()),
        Value::Double(d) => CellValue::F64(d),
        Value::Boolean(b) => CellValue::Bool(b),
        Value::VerbatimString { text, .. } => CellValue::String(text),
        Value::BigNumber(digits) => {
            CellValue::String(String::from_utf8_lossy(&digits).into_owned())
        }
        Value::Attribute { data, .. } => cell(*data),
        Value::ServerError(error) => CellValue::String(format!("{error:?}")),
        container @ (Value::Array(_) | Value::Map(_) | Value::Set(_) | Value::Push { .. }) => {
            CellValue::String(json(&container).to_string())
        }
        other => CellValue::String(format!("{other:?}")),
    }
}

/// Reply as JSON for nested containers.
pub(crate) fn json(value: &Value) -> serde_json::Value {
    match value {
        Value::Nil => serde_json::Value::Null,
        Value::Int(n) => serde_json::Value::from(*n),
        Value::BulkString(bytes) => {
            serde_json::Value::String(String::from_utf8_lossy(bytes).into_owned())
        }
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            serde_json::Value::String(s.clone())
        }
        Value::Okay => serde_json::Value::String("OK".to_string()),
        Value::Double(d) => serde_json::Number::from_f64(*d)
            .map(serde_json::Value::Number)
            .unwrap_or_else(|| serde_json::Value::String(d.to_string())),
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::BigNumber(digits) => {
            serde_json::Value::String(String::from_utf8_lossy(digits).into_owned())
        }
        Value::Array(items) | Value::Set(items) => {
            serde_json::Value::Array(items.iter().map(json).collect())
        }
        Value::Map(pairs) => {
            serde_json::Value::Object(pairs.iter().map(|(k, v)| (json_key(k), json(v))).collect())
        }
        Value::Attribute { data, .. } => json(data),
        Value::Push { data, .. } => serde_json::Value::Array(data.iter().map(json).collect()),
        Value::ServerError(error) => serde_json::Value::String(format!("{error:?}")),
        other => serde_json::Value::String(format!("{other:?}")),
    }
}

pub(crate) fn json_key(value: &Value) -> String {
    match json(value) {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    }
}

fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => Some(s.clone()),
        Value::Okay => Some("OK".to_string()),
        Value::Int(n) => Some(n.to_string()),
        Value::Double(d) => Some(d.to_string()),
        _ => None,
    }
}

fn arg_upper(args: &[Vec<u8>], index: usize) -> String {
    args.get(index)
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

fn has_flag(args: &[Vec<u8>], flag: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(flag.as_bytes()))
}

/// Shape the reply to a command (`args[0]` is the command name).
pub fn shape_reply(args: &[Vec<u8>], value: Value) -> Shaped {
    // A RESP3 reply to a core command is folded to its RESP2 form first, so
    // the grid is the same on either protocol (see `super::hello`).
    let value = super::hello::resp2_form(args, value);
    let name = arg_upper(args, 0);
    let sub = arg_upper(args, 1);
    match (name.as_str(), value) {
        ("INFO", value) => match text_of(&value) {
            Some(text) => info_rows(&text),
            None => generic(value),
        },
        ("CLIENT", value) if sub == "LIST" => match text_of(&value) {
            Some(text) => client_list_rows(&text),
            None => generic(value),
        },
        ("SLOWLOG", Value::Array(entries)) if sub == "GET" => slowlog_rows(entries),
        ("HGETALL", Value::Array(items))
        | ("CONFIG", Value::Array(items))
        | ("HRANDFIELD", Value::Array(items))
            if name != "HRANDFIELD" || has_flag(args, "WITHVALUES") =>
        {
            if name == "CONFIG" && sub != "GET" {
                return generic(Value::Array(items));
            }
            pairs(items, "field", "value")
        }
        (
            "ZRANGE" | "ZRANGEBYSCORE" | "ZREVRANGE" | "ZREVRANGEBYSCORE" | "ZRANDMEMBER"
            | "ZPOPMIN" | "ZPOPMAX" | "ZUNION" | "ZINTER" | "ZDIFF",
            Value::Array(items),
        ) if has_flag(args, "WITHSCORES") => pairs(items, "member", "score"),
        ("SCAN", Value::Array(items)) => scan_rows(items, "key"),
        ("SSCAN", Value::Array(items)) => scan_rows(items, "member"),
        ("HSCAN", Value::Array(items)) => scan_pairs(items, "field", "value"),
        ("ZSCAN", Value::Array(items)) => scan_pairs(items, "member", "score"),
        ("XRANGE" | "XREVRANGE", Value::Array(entries)) => stream_rows(entries),
        ("MEMORY", Value::Array(items)) if sub == "STATS" => pairs(items, "field", "value"),
        ("HELLO", Value::Array(items)) => pairs(items, "field", "value"),
        // Redis Stack modules and the structured core replies: see the
        // "Stack shapes" section below.
        ("JSON.GET", value) => json_document(value),
        ("JSON.MGET", Value::Array(items)) => json_documents(args, items),
        ("FT.SEARCH", value) => search_rows(args, value),
        ("FT.AGGREGATE", value) => aggregate_rows(value),
        ("FT.CURSOR", value) if sub == "READ" => aggregate_rows(value),
        (
            "FT.INFO" | "TS.INFO" | "BF.INFO" | "CF.INFO" | "CMS.INFO" | "TOPK.INFO"
            | "TDIGEST.INFO",
            value,
        ) => field_value_rows(value),
        ("FT._LIST", Value::Array(items) | Value::Set(items)) => name_rows(items, "index"),
        ("TS.QUERYINDEX", Value::Array(items) | Value::Set(items)) => name_rows(items, "key"),
        ("TS.RANGE" | "TS.REVRANGE", Value::Array(samples)) => sample_rows(samples),
        ("TS.GET", Value::Array(sample)) => {
            if sample.is_empty() {
                sample_rows(Vec::new())
            } else {
                sample_rows(vec![Value::Array(sample)])
            }
        }
        ("TS.MRANGE" | "TS.MREVRANGE" | "TS.MGET", value) => multi_series_rows(value),
        ("BF.MEXISTS" | "CF.MEXISTS" | "TOPK.QUERY", Value::Array(items)) => {
            item_rows(args, items, "exists", true)
        }
        ("BF.MADD", Value::Array(items)) => item_rows(args, items, "added", true),
        ("CMS.QUERY", Value::Array(items)) => item_rows(args, items, "count", false),
        ("TOPK.LIST", Value::Array(items)) => {
            if has_flag(args, "WITHCOUNT") {
                pairs(items, "item", "count")
            } else {
                name_rows(items, "item")
            }
        }
        ("GEOPOS", Value::Array(items)) => geopos_rows(args, items),
        (
            "GEOSEARCH"
            | "GEORADIUS"
            | "GEORADIUS_RO"
            | "GEORADIUSBYMEMBER"
            | "GEORADIUSBYMEMBER_RO",
            Value::Array(items),
        ) => geo_rows(args, items),
        ("XINFO", value) if sub == "STREAM" => field_value_rows(value),
        ("XINFO", Value::Array(items)) if sub == "GROUPS" || sub == "CONSUMERS" => {
            record_rows(items)
        }
        ("LATENCY", Value::Array(items)) if sub == "LATEST" => latency_latest_rows(items),
        ("LATENCY", Value::Array(items)) if sub == "HISTORY" => latency_history_rows(items),
        (_, value) => generic(value),
    }
}

/// Reply-driven shaping for any command.
pub fn generic(value: Value) -> Shaped {
    match value {
        Value::Nil => Shaped::scalar(CellValue::Null, "nil"),
        Value::Int(n) => Shaped::scalar(CellValue::I64(n), "integer"),
        Value::BulkString(bytes) => Shaped::scalar(bulk_cell(bytes), "string"),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => {
            Shaped::scalar(CellValue::String(s), "status")
        }
        Value::Okay => Shaped::scalar(CellValue::String("OK".to_string()), "status"),
        Value::Double(d) => Shaped::scalar(CellValue::F64(d), "double"),
        Value::Boolean(b) => Shaped::scalar(CellValue::Bool(b), "boolean"),
        Value::BigNumber(digits) => Shaped::scalar(
            CellValue::String(String::from_utf8_lossy(&digits).into_owned()),
            "bignumber",
        ),
        Value::Attribute { data, .. } => generic(*data),
        Value::ServerError(error) => {
            Shaped::scalar(CellValue::String(format!("{error:?}")), "error")
        }
        Value::Array(items) | Value::Set(items) | Value::Push { data: items, .. } => {
            let rows = items
                .into_iter()
                .enumerate()
                .map(|(i, item)| vec![CellValue::I64(i as i64), cell(item)])
                .collect();
            Shaped::new(&[("index", "integer"), ("value", "string")], rows)
        }
        Value::Map(entries) => Shaped::new(
            &[("field", "string"), ("value", "string")],
            entries
                .into_iter()
                .map(|(k, v)| vec![cell(k), cell(v)])
                .collect(),
        ),
        other => Shaped::scalar(cell(other), "string"),
    }
}

fn pairs(items: Vec<Value>, left: &str, right: &str) -> Shaped {
    let mut rows = Vec::with_capacity(items.len() / 2);
    let mut iter = items.into_iter();
    while let Some(k) = iter.next() {
        let v = iter.next().unwrap_or(Value::Nil);
        rows.push(vec![cell(k), cell(v)]);
    }
    Shaped::new(&[(left, "string"), (right, "string")], rows)
}

fn scan_rows(items: Vec<Value>, element: &str) -> Shaped {
    let mut iter = items.into_iter();
    let cursor = iter.next().map(cell).unwrap_or(CellValue::Null);
    let elements = match iter.next() {
        Some(Value::Array(elements)) | Some(Value::Set(elements)) => elements,
        Some(other) => vec![other],
        None => Vec::new(),
    };
    let rows: Vec<Vec<CellValue>> = if elements.is_empty() {
        vec![vec![cursor, CellValue::Null]]
    } else {
        elements
            .into_iter()
            .map(|e| vec![cursor.clone(), cell(e)])
            .collect()
    };
    Shaped::new(&[("cursor", "string"), (element, "string")], rows)
}

fn scan_pairs(items: Vec<Value>, left: &str, right: &str) -> Shaped {
    let mut iter = items.into_iter();
    let cursor = iter.next().map(cell).unwrap_or(CellValue::Null);
    let elements = match iter.next() {
        Some(Value::Array(elements)) => elements,
        _ => Vec::new(),
    };
    let mut rows = Vec::new();
    let mut pair = elements.into_iter();
    while let Some(k) = pair.next() {
        let v = pair.next().unwrap_or(Value::Nil);
        rows.push(vec![cursor.clone(), cell(k), cell(v)]);
    }
    if rows.is_empty() {
        rows.push(vec![cursor, CellValue::Null, CellValue::Null]);
    }
    Shaped::new(
        &[("cursor", "string"), (left, "string"), (right, "string")],
        rows,
    )
}

fn stream_rows(entries: Vec<Value>) -> Shaped {
    let rows = entries
        .into_iter()
        .map(|entry| match entry {
            Value::Array(mut parts) if parts.len() == 2 => {
                let fields = parts.pop().unwrap_or(Value::Nil);
                let id = parts.pop().unwrap_or(Value::Nil);
                let fields = match fields {
                    Value::Array(flat) => {
                        let mut map = serde_json::Map::new();
                        let mut iter = flat.into_iter();
                        while let Some(k) = iter.next() {
                            let v = iter.next().unwrap_or(Value::Nil);
                            map.insert(json_key(&k), json(&v));
                        }
                        CellValue::String(serde_json::Value::Object(map).to_string())
                    }
                    other => cell(other),
                };
                vec![cell(id), fields]
            }
            other => vec![CellValue::Null, cell(other)],
        })
        .collect();
    Shaped::new(&[("id", "string"), ("fields", "json")], rows)
}

fn info_rows(text: &str) -> Shaped {
    let rows = parse_info(text)
        .into_iter()
        .map(|entry| {
            vec![
                CellValue::String(entry.section),
                CellValue::String(entry.key),
                CellValue::String(entry.value),
            ]
        })
        .collect();
    Shaped::new(
        &[
            ("section", "string"),
            ("key", "string"),
            ("value", "string"),
        ],
        rows,
    )
}

/// `CLIENT LIST` as one row per client. The first columns are the ones the
/// Activity Monitor reads by name (`session_id`, `status`, `query_text`);
/// every field the server sent follows under its own name.
pub(crate) fn client_list_rows(text: &str) -> Shaped {
    let clients = parse_client_list(text);
    let mut field_names: Vec<String> = Vec::new();
    for client in &clients {
        for (key, _) in &client.fields {
            if !field_names.iter().any(|f| f == key) {
                field_names.push(key.clone());
            }
        }
    }
    let mut columns = vec![
        ("session_id", "integer"),
        ("status", "string"),
        ("login_name", "string"),
        ("host_name", "string"),
        ("database_name", "string"),
        ("seconds_running", "integer"),
        ("idle_seconds", "integer"),
        ("query_text", "string"),
    ]
    .into_iter()
    .map(|(n, t)| column(n, t))
    .collect::<Vec<_>>();
    columns.extend(field_names.iter().map(|f| column(f, "string")));
    let rows = clients
        .iter()
        .map(|client| {
            let field = |name: &str| client.field(name).map(str::to_string);
            let idle = client
                .field("idle")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let age = client
                .field("age")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let flags = client.field("flags").unwrap_or("N");
            let status = if flags.contains('b') {
                "blocked"
            } else if flags.contains('P') {
                "pubsub"
            } else if flags.contains('x') {
                "in MULTI"
            } else if idle > 0 {
                "idle"
            } else {
                "running"
            };
            let mut row = vec![
                client
                    .field("id")
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(CellValue::I64)
                    .unwrap_or(CellValue::Null),
                CellValue::String(status.to_string()),
                field("user")
                    .map(CellValue::String)
                    .unwrap_or_else(|| CellValue::String("default".to_string())),
                field("addr")
                    .map(CellValue::String)
                    .unwrap_or(CellValue::Null),
                field("db")
                    .map(CellValue::String)
                    .unwrap_or(CellValue::Null),
                CellValue::I64(age),
                CellValue::I64(idle),
                field("cmd")
                    .map(|c| CellValue::String(c.replace('|', " ")))
                    .unwrap_or_else(|| CellValue::String("idle".to_string())),
            ];
            for name in &field_names {
                row.push(
                    client
                        .field(name)
                        .map(|v| CellValue::String(v.to_string()))
                        .unwrap_or(CellValue::Null),
                );
            }
            row
        })
        .collect();
    Shaped { columns, rows }
}

/// `SLOWLOG GET` entries: id, unix timestamp, microseconds, the command
/// line, and (Redis 4+) the client address and name.
pub(crate) fn slowlog_rows(entries: Vec<Value>) -> Shaped {
    let rows = entries
        .into_iter()
        .map(|entry| {
            let parts = match entry {
                Value::Array(parts) => parts,
                other => vec![other],
            };
            let mut iter = parts.into_iter();
            let id = iter.next().map(cell).unwrap_or(CellValue::Null);
            let timestamp = iter
                .next()
                .and_then(|v| match v {
                    Value::Int(n) => Some(n),
                    _ => None,
                })
                .map(|secs| {
                    chrono::DateTime::from_timestamp(secs, 0)
                        .map(|dt| CellValue::String(dt.format("%Y-%m-%d %H:%M:%S").to_string()))
                        .unwrap_or(CellValue::I64(secs))
                })
                .unwrap_or(CellValue::Null);
            let micros = iter.next().and_then(|v| match v {
                Value::Int(n) => Some(n),
                _ => None,
            });
            let command = iter
                .next()
                .map(|v| match v {
                    Value::Array(args) => {
                        let args: Vec<Vec<u8>> = args
                            .into_iter()
                            .map(|a| match a {
                                Value::BulkString(b) => b,
                                other => text_of(&other).unwrap_or_default().into_bytes(),
                            })
                            .collect();
                        CellValue::String(
                            sqlly_intel::query::safety::redis::RedisCommand { line: 0, args }
                                .to_cli_line(),
                        )
                    }
                    other => cell(other),
                })
                .unwrap_or(CellValue::Null);
            let client_addr = iter.next().map(cell).unwrap_or(CellValue::Null);
            let client_name = iter.next().map(cell).unwrap_or(CellValue::Null);
            vec![
                id,
                timestamp,
                micros.map(CellValue::I64).unwrap_or(CellValue::Null),
                micros
                    .map(|m| CellValue::F64(m as f64 / 1000.0))
                    .unwrap_or(CellValue::Null),
                command,
                client_addr,
                client_name,
            ]
        })
        .collect();
    Shaped::new(
        &[
            ("query_id", "integer"),
            ("executed_at", "datetime"),
            ("duration_us", "integer"),
            ("avg_duration_ms", "double"),
            ("query_text", "string"),
            ("client_addr", "string"),
            ("client_name", "string"),
        ],
        rows,
    )
}

// ---------------------------------------------------------------------------
// Stack shapes: JSON, Search, TimeSeries, probabilistic types, geo, XINFO,
// LATENCY. Each accepts the RESP2 flat form and the RESP3 map form of the
// same reply and produces the same columns, so a grid never changes shape
// with the protocol the server negotiated.
// ---------------------------------------------------------------------------

/// A millisecond Unix timestamp as a `datetime` cell (`None` outside the
/// representable range).
fn datetime_ms_cell(ms: i64) -> CellValue {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|dt| CellValue::String(dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()))
        .unwrap_or(CellValue::I64(ms))
}

/// A second Unix timestamp as a `datetime` cell.
fn datetime_secs_cell(secs: i64) -> CellValue {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| CellValue::String(dt.format("%Y-%m-%d %H:%M:%S").to_string()))
        .unwrap_or(CellValue::I64(secs))
}

/// A numeric reply the module defines as a double (time-series samples, geo
/// coordinates and distances): RESP3 sends `Double`, RESP2 the decimal text.
/// Text that does not parse stays text, so nothing is invented.
fn number_cell(value: Value) -> CellValue {
    match value {
        Value::Double(d) => CellValue::F64(d),
        Value::Int(n) => CellValue::I64(n),
        Value::BulkString(bytes) => match std::str::from_utf8(&bytes)
            .ok()
            .and_then(|s| s.trim().parse::<f64>().ok())
        {
            Some(d) => CellValue::F64(d),
            None => bulk_cell(bytes),
        },
        Value::SimpleString(s) => match s.trim().parse::<f64>() {
            Ok(d) => CellValue::F64(d),
            Err(_) => CellValue::String(s),
        },
        other => cell(other),
    }
}

/// A yes/no reply the module defines as a flag (`BF.MEXISTS`, `TOPK.QUERY`):
/// RESP3 sends booleans, RESP2 integers 0/1.
fn flag_cell(value: Value) -> CellValue {
    match value {
        Value::Int(n) => CellValue::Bool(n != 0),
        other => cell(other),
    }
}

/// The grid type for a column from the cells it holds (first non-null wins).
fn infer_type(cells: impl Iterator<Item = CellValue>) -> &'static str {
    for cell in cells {
        return match cell {
            CellValue::Null => continue,
            CellValue::Bool(_) => "boolean",
            CellValue::I64(_) => "integer",
            CellValue::F64(_) => "double",
            CellValue::String(_) | CellValue::Bytes(_) => "string",
        };
    }
    "string"
}

/// A flat `[k, v, k, v]` array or a RESP3 map as `(name, value)` pairs.
fn pair_list(value: Value) -> Vec<(String, Value)> {
    match value {
        Value::Map(entries) => entries
            .into_iter()
            .map(|(k, v)| (text_of(&k).unwrap_or_default(), v))
            .collect(),
        Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len() / 2);
            let mut iter = items.into_iter();
            while let Some(k) = iter.next() {
                let v = iter.next().unwrap_or(Value::Nil);
                out.push((text_of(&k).unwrap_or_default(), v));
            }
            out
        }
        Value::Attribute { data, .. } => pair_list(*data),
        _ => Vec::new(),
    }
}

/// Look a field up by name in a flat pair array or a map.
pub(crate) fn find_field(value: &Value, name: &str) -> Option<Value> {
    match value {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| text_of(k).as_deref() == Some(name))
            .map(|(_, v)| v.clone()),
        Value::Array(items) => items
            .chunks(2)
            .find(|chunk| text_of(&chunk[0]).as_deref() == Some(name))
            .and_then(|chunk| chunk.get(1).cloned()),
        Value::Attribute { data, .. } => find_field(data, name),
        _ => None,
    }
}

/// Records (one per row) with a union of their field names as columns, in
/// first-seen order, followed by `trailing` typed columns repeated on every
/// row. Column types are inferred from the cells.
fn records_table(
    records: Vec<Vec<(String, CellValue)>>,
    trailing: &[(&str, &str, CellValue)],
) -> Shaped {
    let mut names: Vec<String> = Vec::new();
    for record in &records {
        for (name, _) in record {
            if !names.iter().any(|n| n == name) {
                names.push(name.clone());
            }
        }
    }
    let mut rows: Vec<Vec<CellValue>> = records
        .into_iter()
        .map(|record| {
            let mut row: Vec<CellValue> = names
                .iter()
                .map(|name| {
                    record
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, v)| v.clone())
                        .unwrap_or(CellValue::Null)
                })
                .collect();
            row.extend(trailing.iter().map(|(_, _, v)| v.clone()));
            row
        })
        .collect();
    if rows.is_empty() && !trailing.is_empty() {
        rows.push(trailing.iter().map(|(_, _, v)| v.clone()).collect());
    }
    let mut columns: Vec<ResultColumn> = names
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let data_type = infer_type(rows.iter().map(|r| r[i].clone()));
            column(name, data_type)
        })
        .collect();
    columns.extend(trailing.iter().map(|(n, t, _)| column(n, t)));
    Shaped { columns, rows }
}

/// `JSON.GET`: the document as one `json` cell so the viewer opens the tree.
fn json_document(value: Value) -> Shaped {
    Shaped::new(&[("json", "json")], vec![vec![cell(value)]])
}

/// `JSON.MGET key… path`: key + document (the last argument is the path).
fn json_documents(args: &[Vec<u8>], items: Vec<Value>) -> Shaped {
    let keys: &[Vec<u8>] = if args.len() >= 3 {
        &args[1..args.len() - 1]
    } else {
        &args[1.min(args.len())..]
    };
    let rows = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let key = keys
                .get(i)
                .map(|k| bulk_cell(k.clone()))
                .unwrap_or(CellValue::Null);
            vec![key, cell(item)]
        })
        .collect();
    Shaped::new(&[("key", "string"), ("json", "json")], rows)
}

/// One hit of `FT.SEARCH`, whichever protocol delivered it.
struct SearchDoc {
    key: CellValue,
    score: Option<CellValue>,
    payload: Option<CellValue>,
    sortkey: Option<CellValue>,
    fields: Vec<(String, CellValue)>,
}

/// `FT.SEARCH`: one row per document, `key` first, then score / payload /
/// sort key when the query asked for them, then a column per returned
/// field, then the server's `total_results`.
fn search_rows(args: &[Vec<u8>], value: Value) -> Shaped {
    let (total, docs) = match value {
        Value::Map(entries) => search_docs_resp3(entries),
        Value::Array(items) => search_docs_resp2(args, items),
        other => return generic(other),
    };
    documents_table(total, docs, None)
}

fn search_docs_resp2(args: &[Vec<u8>], items: Vec<Value>) -> (Option<i64>, Vec<SearchDoc>) {
    let nocontent = has_flag(args, "NOCONTENT");
    let withscores = has_flag(args, "WITHSCORES");
    let withpayloads = has_flag(args, "WITHPAYLOADS");
    let withsortkeys = has_flag(args, "WITHSORTKEYS");
    let mut iter = items.into_iter();
    let total = match iter.next() {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    };
    let mut docs = Vec::new();
    while let Some(key) = iter.next() {
        let score = withscores.then(|| iter.next().map(number_cell).unwrap_or(CellValue::Null));
        let payload = withpayloads.then(|| iter.next().map(cell).unwrap_or(CellValue::Null));
        let sortkey = withsortkeys.then(|| iter.next().map(cell).unwrap_or(CellValue::Null));
        let fields = if nocontent {
            Vec::new()
        } else {
            iter.next()
                .map(|v| {
                    pair_list(v)
                        .into_iter()
                        .map(|(k, v)| (k, cell(v)))
                        .collect()
                })
                .unwrap_or_default()
        };
        docs.push(SearchDoc {
            key: cell(key),
            score,
            payload,
            sortkey,
            fields,
        });
    }
    (total, docs)
}

fn search_docs_resp3(entries: Vec<(Value, Value)>) -> (Option<i64>, Vec<SearchDoc>) {
    let reply = Value::Map(entries);
    let total = match find_field(&reply, "total_results") {
        Some(Value::Int(n)) => Some(n),
        _ => None,
    };
    let results = match find_field(&reply, "results") {
        Some(Value::Array(results)) => results,
        _ => Vec::new(),
    };
    let docs = results
        .into_iter()
        .map(|doc| SearchDoc {
            key: find_field(&doc, "id").map(cell).unwrap_or(CellValue::Null),
            score: find_field(&doc, "score").map(number_cell),
            payload: find_field(&doc, "payload").map(cell),
            sortkey: find_field(&doc, "sortkey").map(cell),
            fields: find_field(&doc, "extra_attributes")
                .map(|v| {
                    pair_list(v)
                        .into_iter()
                        .map(|(k, v)| (k, cell(v)))
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    (total, docs)
}

fn documents_table(total: Option<i64>, docs: Vec<SearchDoc>, cursor: Option<i64>) -> Shaped {
    let with_score = docs.iter().any(|d| d.score.is_some());
    let with_payload = docs.iter().any(|d| d.payload.is_some());
    let with_sortkey = docs.iter().any(|d| d.sortkey.is_some());
    let records = docs
        .into_iter()
        .map(|doc| {
            let mut record = vec![("key".to_string(), doc.key)];
            if with_score {
                record.push(("score".to_string(), doc.score.unwrap_or(CellValue::Null)));
            }
            if with_payload {
                record.push((
                    "payload".to_string(),
                    doc.payload.unwrap_or(CellValue::Null),
                ));
            }
            if with_sortkey {
                record.push((
                    "sortkey".to_string(),
                    doc.sortkey.unwrap_or(CellValue::Null),
                ));
            }
            record.extend(doc.fields);
            record
        })
        .collect::<Vec<_>>();
    let mut trailing = vec![(
        "total_results",
        "integer",
        total.map(CellValue::I64).unwrap_or(CellValue::Null),
    )];
    if let Some(cursor) = cursor {
        trailing.push(("cursor", "integer", CellValue::I64(cursor)));
    }
    let mut shaped = records_table(records, &trailing);
    // An empty hit list still names the key column before the total.
    if shaped.columns.first().map(|c| c.name.as_str()) != Some("key") {
        shaped.columns.insert(0, column("key", "string"));
        for row in &mut shaped.rows {
            row.insert(0, CellValue::Null);
        }
    }
    shaped
}

/// `FT.AGGREGATE` / `FT.CURSOR READ`: one row per result record, a column
/// per field, then `total_results` and (with `WITHCURSOR`) the cursor id.
fn aggregate_rows(value: Value) -> Shaped {
    let (inner, cursor) = match value {
        Value::Array(mut parts)
            if parts.len() == 2
                && matches!(parts[0], Value::Array(_) | Value::Map(_))
                && matches!(parts[1], Value::Int(_)) =>
        {
            let cursor = match parts.pop() {
                Some(Value::Int(n)) => Some(n),
                _ => None,
            };
            (parts.pop().unwrap_or(Value::Nil), cursor)
        }
        other => (other, None),
    };
    let (total, records): (Option<i64>, Vec<Vec<(String, CellValue)>>) = match inner {
        Value::Map(entries) => {
            let (total, docs) = search_docs_resp3(entries);
            (total, docs.into_iter().map(|d| d.fields).collect())
        }
        Value::Array(items) => {
            let mut iter = items.into_iter();
            let total = match iter.next() {
                Some(Value::Int(n)) => Some(n),
                _ => None,
            };
            let records = iter
                .map(|record| {
                    pair_list(record)
                        .into_iter()
                        .map(|(k, v)| (k, cell(v)))
                        .collect()
                })
                .collect();
            (total, records)
        }
        other => return generic(other),
    };
    let mut trailing = vec![(
        "total_results",
        "integer",
        total.map(CellValue::I64).unwrap_or(CellValue::Null),
    )];
    if let Some(cursor) = cursor {
        trailing.push(("cursor", "integer", CellValue::I64(cursor)));
    }
    records_table(records, &trailing)
}

/// `FT.INFO`, `TS.INFO`, `BF.INFO`, `XINFO STREAM`, …: field / value, nested
/// structures as JSON.
fn field_value_rows(value: Value) -> Shaped {
    match value {
        Value::Array(items) => pairs(items, "field", "value"),
        other => generic(other),
    }
}

/// A plain list of names (`FT._LIST`, `TS.QUERYINDEX`, `TOPK.LIST`).
fn name_rows(items: Vec<Value>, name: &str) -> Shaped {
    let rows = items.into_iter().map(|item| vec![cell(item)]).collect();
    Shaped::new(&[(name, "string")], rows)
}

/// One `[timestamp_ms, value]` sample as a row; the raw millisecond
/// timestamp rides along so nothing is lost to formatting.
fn sample_row(sample: Value) -> Vec<CellValue> {
    match sample {
        Value::Array(mut parts) if parts.len() == 2 => {
            let value = parts.pop().unwrap_or(Value::Nil);
            let ts = parts.pop().unwrap_or(Value::Nil);
            match ts {
                Value::Int(ms) => {
                    vec![datetime_ms_cell(ms), number_cell(value), CellValue::I64(ms)]
                }
                other => vec![cell(other), number_cell(value), CellValue::Null],
            }
        }
        other => vec![CellValue::Null, cell(other), CellValue::Null],
    }
}

const SAMPLE_COLUMNS: &[(&str, &str)] = &[
    ("timestamp", "datetime"),
    ("value", "double"),
    ("timestamp_ms", "integer"),
];

/// `TS.RANGE` / `TS.REVRANGE` / `TS.GET`: timestamp + value per sample.
fn sample_rows(samples: Vec<Value>) -> Shaped {
    Shaped::new(
        SAMPLE_COLUMNS,
        samples.into_iter().map(sample_row).collect(),
    )
}

/// `TS.MRANGE` / `TS.MREVRANGE` / `TS.MGET`: series + timestamp + value, with
/// the series' labels as JSON. RESP2 lists `[key, labels, samples]` triples;
/// RESP3 maps each key to `[labels, (aggregators,) samples]`.
fn multi_series_rows(value: Value) -> Shaped {
    let series: Vec<(Value, Vec<Value>)> = match value {
        Value::Array(items) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::Array(mut parts) if !parts.is_empty() => {
                    let key = parts.remove(0);
                    Some((key, parts))
                }
                _ => None,
            })
            .collect(),
        Value::Map(entries) => entries
            .into_iter()
            .map(|(key, body)| match body {
                Value::Array(parts) => (key, parts),
                other => (key, vec![other]),
            })
            .collect(),
        other => return generic(other),
    };
    let mut rows = Vec::new();
    for (key, parts) in series {
        let name = cell(key);
        let labels = parts.first().map(labels_json).unwrap_or(CellValue::Null);
        // The samples are the last part: a list of samples, or one sample
        // (`TS.MGET`) whose first element is the timestamp.
        let samples = match parts.into_iter().last() {
            Some(Value::Array(items)) if matches!(items.first(), Some(Value::Int(_))) => {
                vec![Value::Array(items)]
            }
            Some(Value::Array(items)) => items,
            _ => Vec::new(),
        };
        if samples.is_empty() {
            rows.push(vec![
                name,
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
                labels,
            ]);
            continue;
        }
        for sample in samples {
            let mut row = vec![name.clone()];
            row.extend(sample_row(sample));
            row.push(labels.clone());
            rows.push(row);
        }
    }
    Shaped::new(
        &[
            ("series", "string"),
            ("timestamp", "datetime"),
            ("value", "double"),
            ("timestamp_ms", "integer"),
            ("labels", "json"),
        ],
        rows,
    )
}

/// `SQLLY.COMMANDS`: `COMMAND DOCS` (a map of name → body under RESP3, a
/// flat `[name, body, …]` array under RESP2) joined with `COMMAND INFO`
/// (an array of rows whose first item is the name) into one row per
/// command: `command` (upper-case), `docs` and `info` as JSON text in the
/// connection's own spelling. The IntelliSense vocabulary parses either.
pub(crate) fn command_docs_rows(docs: Value, info: Value) -> Shaped {
    use std::collections::BTreeMap;
    let mut docs_by_name: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    match json(&docs) {
        serde_json::Value::Object(map) => {
            for (name, body) in map {
                docs_by_name.insert(name.to_ascii_uppercase(), body);
            }
        }
        serde_json::Value::Array(items) => {
            let mut iter = items.into_iter();
            while let (Some(name), Some(body)) = (iter.next(), iter.next()) {
                if let serde_json::Value::String(name) = name {
                    docs_by_name.insert(name.to_ascii_uppercase(), body);
                }
            }
        }
        _ => {}
    }
    let mut info_by_name: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    if let serde_json::Value::Array(rows) = json(&info) {
        for row in rows {
            if let Some(serde_json::Value::String(name)) =
                row.as_array().and_then(|items| items.first())
            {
                info_by_name.insert(name.to_ascii_uppercase(), row);
            }
        }
    }
    let names: std::collections::BTreeSet<&String> =
        docs_by_name.keys().chain(info_by_name.keys()).collect();
    let rows = names
        .into_iter()
        .map(|name| {
            vec![
                CellValue::String(name.clone()),
                CellValue::String(
                    docs_by_name
                        .get(name)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "{}".to_string()),
                ),
                CellValue::String(
                    info_by_name
                        .get(name)
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "[]".to_string()),
                ),
            ]
        })
        .collect();
    Shaped::new(
        &[
            ("command", "string"),
            ("docs", "string"),
            ("info", "string"),
        ],
        rows,
    )
}

/// Series labels as a JSON object: RESP3 sends a map, RESP2 a list of
/// `[name, value]` pairs. Keys are written sorted on purpose: a
/// `serde_json::Map` keeps insertion order only when some crate in the build
/// enables serde_json's `preserve_order`, so its order would differ between
/// builds (and between RESP2 and RESP3 replies).
fn labels_json(value: &Value) -> CellValue {
    let object = match value {
        Value::Map(_) => json(value),
        Value::Array(items) => serde_json::Value::Object(
            items
                .iter()
                .filter_map(|pair| match pair {
                    Value::Array(kv) if kv.len() == 2 => Some((json_key(&kv[0]), json(&kv[1]))),
                    _ => None,
                })
                .collect(),
        ),
        other => json(other),
    };
    let text = match object {
        serde_json::Value::Object(map) => {
            let sorted: std::collections::BTreeMap<String, serde_json::Value> =
                map.into_iter().collect();
            serde_json::to_string(&sorted).unwrap_or_default()
        }
        other => other.to_string(),
    };
    CellValue::String(text)
}

/// Per-item replies (`BF.MEXISTS key item…`, `CMS.QUERY key item…`): the
/// item names come from the arguments after the key.
fn item_rows(args: &[Vec<u8>], items: Vec<Value>, value_name: &str, flags: bool) -> Shaped {
    let rows = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let name = args
                .get(i + 2)
                .map(|a| bulk_cell(a.clone()))
                .unwrap_or(CellValue::Null);
            let value = if flags { flag_cell(item) } else { cell(item) };
            vec![name, value]
        })
        .collect();
    let data_type = if flags { "boolean" } else { "integer" };
    Shaped::new(&[("item", "string"), (value_name, data_type)], rows)
}

/// `GEOPOS key member…`: member / lon / lat (nulls for unknown members).
fn geopos_rows(args: &[Vec<u8>], items: Vec<Value>) -> Shaped {
    let rows = items
        .into_iter()
        .enumerate()
        .map(|(i, item)| {
            let member = args
                .get(i + 2)
                .map(|a| bulk_cell(a.clone()))
                .unwrap_or(CellValue::Null);
            let (lon, lat) = lon_lat(item);
            vec![member, lon, lat]
        })
        .collect();
    Shaped::new(
        &[("member", "string"), ("lon", "double"), ("lat", "double")],
        rows,
    )
}

fn lon_lat(value: Value) -> (CellValue, CellValue) {
    match value {
        Value::Array(mut parts) if parts.len() == 2 => {
            let lat = number_cell(parts.pop().unwrap_or(Value::Nil));
            let lon = number_cell(parts.pop().unwrap_or(Value::Nil));
            (lon, lat)
        }
        _ => (CellValue::Null, CellValue::Null),
    }
}

/// `GEOSEARCH` / `GEORADIUS*`: member, then the distance, hash and
/// coordinates the query asked for (in the order the server sends them).
fn geo_rows(args: &[Vec<u8>], items: Vec<Value>) -> Shaped {
    let with_dist = has_flag(args, "WITHDIST");
    let with_hash = has_flag(args, "WITHHASH");
    let with_coord = has_flag(args, "WITHCOORD");
    if !(with_dist || with_hash || with_coord) {
        return name_rows(items, "member");
    }
    let mut columns = vec![("member", "string")];
    if with_dist {
        columns.push(("dist", "double"));
    }
    if with_hash {
        columns.push(("hash", "integer"));
    }
    if with_coord {
        columns.push(("lon", "double"));
        columns.push(("lat", "double"));
    }
    let rows = items
        .into_iter()
        .map(|item| {
            let mut parts = match item {
                Value::Array(parts) => parts.into_iter(),
                other => vec![other].into_iter(),
            };
            let mut row = vec![parts.next().map(cell).unwrap_or(CellValue::Null)];
            if with_dist {
                row.push(parts.next().map(number_cell).unwrap_or(CellValue::Null));
            }
            if with_hash {
                row.push(parts.next().map(cell).unwrap_or(CellValue::Null));
            }
            if with_coord {
                let (lon, lat) = parts
                    .next()
                    .map(lon_lat)
                    .unwrap_or((CellValue::Null, CellValue::Null));
                row.push(lon);
                row.push(lat);
            }
            row
        })
        .collect();
    Shaped::new(&columns, rows)
}

/// `XINFO GROUPS` / `XINFO CONSUMERS`: one row per group or consumer with a
/// column per reported field.
fn record_rows(items: Vec<Value>) -> Shaped {
    let records = items
        .into_iter()
        .map(|item| {
            pair_list(item)
                .into_iter()
                .map(|(k, v)| (k, cell(v)))
                .collect()
        })
        .collect();
    records_table(records, &[])
}

/// `LATENCY LATEST`: event, when it was last measured, the latest and the
/// all-time maximum latency in milliseconds.
fn latency_latest_rows(items: Vec<Value>) -> Shaped {
    let rows = items
        .into_iter()
        .map(|item| {
            let mut parts = match item {
                Value::Array(parts) => parts.into_iter(),
                other => vec![other].into_iter(),
            };
            let event = parts.next().map(cell).unwrap_or(CellValue::Null);
            let measured_at = match parts.next() {
                Some(Value::Int(secs)) => datetime_secs_cell(secs),
                Some(other) => cell(other),
                None => CellValue::Null,
            };
            let latest = parts.next().map(cell).unwrap_or(CellValue::Null);
            let max = parts.next().map(cell).unwrap_or(CellValue::Null);
            vec![event, measured_at, latest, max]
        })
        .collect();
    Shaped::new(
        &[
            ("event", "string"),
            ("measured_at", "datetime"),
            ("latest_ms", "integer"),
            ("max_ms", "integer"),
        ],
        rows,
    )
}

/// `LATENCY HISTORY event`: when each spike was measured and its latency.
fn latency_history_rows(items: Vec<Value>) -> Shaped {
    let rows = items
        .into_iter()
        .map(|item| {
            let mut parts = match item {
                Value::Array(parts) => parts.into_iter(),
                other => vec![other].into_iter(),
            };
            let measured_at = match parts.next() {
                Some(Value::Int(secs)) => datetime_secs_cell(secs),
                Some(other) => cell(other),
                None => CellValue::Null,
            };
            let latency = parts.next().map(cell).unwrap_or(CellValue::Null);
            vec![measured_at, latency]
        })
        .collect();
    Shaped::new(
        &[("measured_at", "datetime"), ("latency_ms", "integer")],
        rows,
    )
}

/// Stack shapes over replies captured from `redis/redis-stack-server` in
/// both protocols (`tests/fixtures/redis_stack/<name>.resp2|resp3`, raw
/// wire bytes), plus one live run against `SQLLY_REDIS_STACK_URL`.
#[cfg(test)]
mod stack {
    use super::*;

    fn args(text: &str) -> Vec<Vec<u8>> {
        text.split(' ').map(|s| s.as_bytes().to_vec()).collect()
    }

    fn names(shaped: &Shaped) -> Vec<&str> {
        shaped.columns.iter().map(|c| c.name.as_str()).collect()
    }

    fn types(shaped: &Shaped) -> Vec<&str> {
        shaped
            .columns
            .iter()
            .map(|c| c.data_type.as_str())
            .collect()
    }

    /// The captured reply for `name` under RESP `proto`, parsed by the
    /// driver's own parser — exactly what the client would hand to
    /// `shape_reply`.
    fn fixture(name: &str, proto: u8) -> Value {
        let path = format!(
            "{}/tests/fixtures/redis_stack/{name}.resp{proto}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        redis::parse_redis_value(&bytes).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    /// Shape `command` from the captured `name` reply under both protocols
    /// and assert they agree cell for cell, returning the RESP2 shape.
    fn both(name: &str, command: &str) -> Shaped {
        let (two, three) = both_columns(name, command);
        assert_eq!(two.rows, three.rows, "{name}: rows differ by protocol");
        two
    }

    /// Like [`both`] for replies whose cells legitimately differ by
    /// protocol (RESP2 sends doubles as text and nests structures as
    /// arrays where RESP3 sends doubles and maps) or by capture (cursor
    /// ids, idle times): columns, types and row count must still agree.
    fn both_columns(name: &str, command: &str) -> (Shaped, Shaped) {
        let two = shape_reply(&args(command), fixture(name, 2));
        let three = shape_reply(&args(command), fixture(name, 3));
        assert_eq!(
            names(&two),
            names(&three),
            "{name}: columns differ by protocol"
        );
        assert_eq!(
            types(&two),
            types(&three),
            "{name}: types differ by protocol"
        );
        assert_eq!(
            two.rows.len(),
            three.rows.len(),
            "{name}: row count differs by protocol"
        );
        (two, three)
    }

    #[test]
    fn json_get_and_mget_are_json_cells() {
        let doc = both("json_get", "JSON.GET wse:doc:1");
        assert_eq!(names(&doc), vec!["json"]);
        assert_eq!(types(&doc), vec!["json"]);
        assert!(
            matches!(&doc.rows[0][0], CellValue::String(s) if s.starts_with("{\"name\":\"ann\""))
        );
        let paths = both("json_get_paths", "JSON.GET wse:doc:1 $.name $.age");
        assert!(matches!(&paths.rows[0][0], CellValue::String(s) if s.contains("\"$.name\"")));
        let missing = shape_reply(&args("JSON.GET nope"), Value::Nil);
        assert_eq!(missing.rows, vec![vec![CellValue::Null]]);
        assert_eq!(names(&missing), vec!["json"]);

        let many = both("json_mget", "JSON.MGET wse:doc:1 wse:doc:2 wse:missing $");
        assert_eq!(names(&many), vec!["key", "json"]);
        assert_eq!(types(&many)[1], "json");
        assert_eq!(many.rows.len(), 3);
        assert_eq!(many.rows[0][0], CellValue::String("wse:doc:1".into()));
        assert_eq!(many.rows[2][0], CellValue::String("wse:missing".into()));
        assert_eq!(many.rows[2][1], CellValue::Null);
    }

    #[test]
    fn ft_search_is_one_row_per_document_with_field_columns() {
        let hits = both("ft_search", "FT.SEARCH wse:idx * SORTBY age ASC");
        assert_eq!(names(&hits), vec!["key", "age", "$", "total_results"]);
        assert_eq!(hits.rows.len(), 3);
        assert_eq!(hits.rows[0][0], CellValue::String("wse:doc:3".into()));
        assert_eq!(hits.rows[0][1], CellValue::String("27".into()));
        assert!(matches!(&hits.rows[0][2], CellValue::String(s) if s.starts_with('{')));
        assert_eq!(hits.rows[0][3], CellValue::I64(3));
        assert_eq!(hits.rows[2][0], CellValue::String("wse:doc:2".into()));

        let scored = both(
            "ft_search_withscores",
            "FT.SEARCH wse:idx * WITHSCORES SORTBY age ASC",
        );
        assert_eq!(
            names(&scored),
            vec!["key", "score", "age", "$", "total_results"]
        );
        assert_eq!(scored.rows[0][1], CellValue::F64(1.0));
        assert_eq!(types(&scored)[1], "double");

        let keys_only = both(
            "ft_search_nocontent",
            "FT.SEARCH wse:idx * NOCONTENT SORTBY age ASC",
        );
        assert_eq!(names(&keys_only), vec!["key", "total_results"]);
        assert_eq!(keys_only.rows.len(), 3);
        assert_eq!(keys_only.rows[1][0], CellValue::String("wse:doc:1".into()));

        let returned = both(
            "ft_search_return",
            "FT.SEARCH wse:idx * RETURN 1 name SORTBY age ASC",
        );
        assert_eq!(names(&returned), vec!["key", "name", "total_results"]);
        assert_eq!(returned.rows[0][1], CellValue::String("cyd".into()));

        // No hits: the key column is still there, the total is 0 on one row.
        let none = both("ft_search_empty", "FT.SEARCH wse:idx @name:nomatch");
        assert_eq!(names(&none), vec!["key", "total_results"]);
        assert_eq!(none.rows, vec![vec![CellValue::Null, CellValue::I64(0)]]);
    }

    #[test]
    fn ft_aggregate_is_one_row_per_group_and_carries_the_cursor() {
        let groups = both(
            "ft_aggregate",
            "FT.AGGREGATE wse:idx * GROUPBY 1 @city REDUCE COUNT 0 AS n SORTBY 2 @city ASC",
        );
        assert_eq!(names(&groups), vec!["city", "n", "total_results"]);
        assert_eq!(
            groups.rows,
            vec![
                vec![
                    CellValue::String("Bergen".into()),
                    CellValue::String("1".into()),
                    CellValue::I64(2)
                ],
                vec![
                    CellValue::String("Oslo".into()),
                    CellValue::String("2".into()),
                    CellValue::I64(2)
                ],
            ]
        );
        // The cursor id is whatever the server minted at capture time.
        let (paged, paged3) = both_columns(
            "ft_aggregate_cursor",
            "FT.AGGREGATE wse:idx * LOAD 1 @name WITHCURSOR COUNT 2",
        );
        assert_eq!(names(&paged), vec!["name", "total_results", "cursor"]);
        assert_eq!(paged.rows.len(), 2);
        assert_eq!(paged.rows[0][0], CellValue::String("cyd".into()));
        assert_eq!(paged3.rows[0][0], CellValue::String("cyd".into()));
        assert!(matches!(paged.rows[0][2], CellValue::I64(c) if c > 0));
        assert!(matches!(paged3.rows[0][2], CellValue::I64(c) if c > 0));
    }

    #[test]
    fn ft_info_and_list_are_tables() {
        // RESP2 reports the statistics as text and the nested definition
        // as a flat array; RESP3 as doubles and a map. Both land in the
        // same field/value table with the nested values as JSON.
        let (info, info3) = both_columns("ft_info", "FT.INFO wse:idx");
        assert_eq!(names(&info), vec!["field", "value"]);
        for table in [&info, &info3] {
            assert!(
                table
                    .rows
                    .iter()
                    .any(|r| r[0] == CellValue::String("num_docs".into())
                        && r[1] == CellValue::I64(3))
            );
            let definition = table
                .rows
                .iter()
                .find(|r| r[0] == CellValue::String("index_definition".into()))
                .expect("index_definition row");
            assert!(matches!(&definition[1], CellValue::String(s) if s.contains("wse:doc:")));
        }

        let list = both("ft_list", "FT._LIST");
        assert_eq!(names(&list), vec!["index"]);
        assert_eq!(list.rows, vec![vec![CellValue::String("wse:idx".into())]]);
    }

    #[test]
    fn ts_range_has_datetime_timestamps_and_double_values() {
        let range = both("ts_range", "TS.RANGE wse:ts - +");
        assert_eq!(names(&range), vec!["timestamp", "value", "timestamp_ms"]);
        assert_eq!(types(&range), vec!["datetime", "double", "integer"]);
        assert_eq!(range.rows.len(), 5);
        assert_eq!(
            range.rows[0],
            vec![
                CellValue::String("2023-11-14 22:13:21.000".into()),
                CellValue::F64(1.5),
                CellValue::I64(1_700_000_001_000),
            ]
        );
        let rev = both("ts_revrange", "TS.REVRANGE wse:ts - + COUNT 2");
        assert_eq!(rev.rows[0][1], CellValue::F64(5.5));
        let get = both("ts_get", "TS.GET wse:ts");
        assert_eq!(get.rows.len(), 1);
        assert_eq!(get.rows[0][2], CellValue::I64(1_700_000_005_000));
        let empty = shape_reply(&args("TS.GET empty"), Value::Array(vec![]));
        assert!(empty.rows.is_empty());
        assert_eq!(names(&empty), vec!["timestamp", "value", "timestamp_ms"]);
        let index = both("ts_queryindex", "TS.QUERYINDEX team=wse");
        assert_eq!(names(&index), vec!["key"]);
        assert_eq!(index.rows.len(), 2);
        let (info, info3) = both_columns("ts_info", "TS.INFO wse:ts");
        assert_eq!(names(&info), vec!["field", "value"]);
        for table in [&info, &info3] {
            assert!(
                table
                    .rows
                    .iter()
                    .any(|r| r[0] == CellValue::String("totalSamples".into())
                        && r[1] == CellValue::I64(5))
            );
        }
    }

    #[test]
    fn ts_mrange_and_mget_flatten_series() {
        let m = both("ts_mrange", "TS.MRANGE - + WITHLABELS FILTER team=wse");
        assert_eq!(
            names(&m),
            vec!["series", "timestamp", "value", "timestamp_ms", "labels"]
        );
        assert_eq!(m.rows.len(), 10);
        assert_eq!(m.rows[0][0], CellValue::String("wse:ts".into()));
        assert_eq!(m.rows[0][2], CellValue::F64(1.5));
        assert_eq!(
            m.rows[0][4],
            CellValue::String("{\"kind\":\"primary\",\"team\":\"wse\"}".into())
        );
        assert_eq!(m.rows[5][0], CellValue::String("wse:ts2".into()));
        assert_eq!(m.rows[9][2], CellValue::F64(50.0));

        let bare = both("ts_mrange_nolabels", "TS.MRANGE - + FILTER team=wse");
        assert_eq!(bare.rows.len(), 10);
        assert_eq!(bare.rows[0][4], CellValue::String("{}".into()));

        let latest = both("ts_mget", "TS.MGET WITHLABELS FILTER team=wse");
        assert_eq!(latest.rows.len(), 2);
        assert_eq!(latest.rows[0][3], CellValue::I64(1_700_000_005_000));
        assert_eq!(latest.rows[1][2], CellValue::F64(50.0));
    }

    #[test]
    fn probabilistic_replies_name_their_items() {
        let bf = both("bf_mexists", "BF.MEXISTS wse:bf a b zz");
        assert_eq!(names(&bf), vec!["item", "exists"]);
        assert_eq!(types(&bf), vec!["string", "boolean"]);
        assert_eq!(
            bf.rows,
            vec![
                vec![CellValue::String("a".into()), CellValue::Bool(true)],
                vec![CellValue::String("b".into()), CellValue::Bool(true)],
                vec![CellValue::String("zz".into()), CellValue::Bool(false)],
            ]
        );
        let cf = both("cf_mexists", "CF.MEXISTS wse:cf a zz");
        assert_eq!(cf.rows[1][1], CellValue::Bool(false));
        let info = both("bf_info", "BF.INFO wse:bf");
        assert_eq!(names(&info), vec!["field", "value"]);
        assert!(
            info.rows
                .iter()
                .any(|r| r[0] == CellValue::String("Capacity".into()))
        );

        let top = both("topk_list", "TOPK.LIST wse:topk");
        assert_eq!(names(&top), vec!["item"]);
        assert_eq!(top.rows.len(), 3);
        let counted = both("topk_list_withcount", "TOPK.LIST wse:topk WITHCOUNT");
        assert_eq!(names(&counted), vec!["item", "count"]);
        assert_eq!(
            counted.rows[0],
            vec![CellValue::String("a".into()), CellValue::I64(3)]
        );
        let query = both("topk_query", "TOPK.QUERY wse:topk a zz");
        assert_eq!(query.rows[0][1], CellValue::Bool(true));
        assert_eq!(query.rows[1][1], CellValue::Bool(false));
        let cms = both("cms_query", "CMS.QUERY wse:cms a b zz");
        assert_eq!(names(&cms), vec!["item", "count"]);
        assert_eq!(
            cms.rows[1],
            vec![CellValue::String("b".into()), CellValue::I64(7)]
        );
    }

    #[test]
    fn geo_replies_split_coordinates_and_distances() {
        let pos = both("geopos", "GEOPOS wse:geo Palermo Catania nowhere");
        assert_eq!(names(&pos), vec!["member", "lon", "lat"]);
        assert_eq!(pos.rows[0][0], CellValue::String("Palermo".into()));
        assert!(matches!(pos.rows[0][1], CellValue::F64(lon) if (lon - 13.3614).abs() < 1e-3));
        assert_eq!(
            pos.rows[2],
            vec![
                CellValue::String("nowhere".into()),
                CellValue::Null,
                CellValue::Null
            ]
        );

        let plain = both(
            "geosearch_plain",
            "GEOSEARCH wse:geo FROMLONLAT 15 37 BYRADIUS 200 km ASC",
        );
        assert_eq!(names(&plain), vec!["member"]);
        assert_eq!(plain.rows[0][0], CellValue::String("Catania".into()));

        let rich = both(
            "geosearch_dist_coord",
            "GEOSEARCH wse:geo FROMLONLAT 15 37 BYRADIUS 200 km ASC WITHDIST WITHCOORD",
        );
        assert_eq!(names(&rich), vec!["member", "dist", "lon", "lat"]);
        assert!(matches!(rich.rows[0][1], CellValue::F64(d) if (d - 56.4413).abs() < 1e-3));
        assert!(matches!(rich.rows[0][3], CellValue::F64(lat) if (lat - 37.5027).abs() < 1e-3));

        let all = both(
            "geosearch_all",
            "GEOSEARCH wse:geo FROMLONLAT 15 37 BYRADIUS 200 km ASC WITHDIST WITHHASH WITHCOORD",
        );
        assert_eq!(names(&all), vec!["member", "dist", "hash", "lon", "lat"]);
        assert!(matches!(all.rows[0][2], CellValue::I64(_)));

        let radius = both(
            "georadius_ro",
            "GEORADIUS_RO wse:geo 15 37 200 km WITHDIST ASC",
        );
        assert_eq!(names(&radius), vec!["member", "dist"]);
        assert_eq!(radius.rows.len(), 2);
    }

    #[test]
    fn bitfield_xinfo_and_latency_are_tables() {
        let bits = both("bitfield", "BITFIELD wse:bits GET u8 0 GET u4 8");
        assert_eq!(names(&bits), vec!["index", "value"]);
        assert_eq!(bits.rows[0], vec![CellValue::I64(0), CellValue::I64(255)]);

        let stream = both("xinfo_stream", "XINFO STREAM wse:stream");
        assert_eq!(names(&stream), vec!["field", "value"]);
        assert!(
            stream
                .rows
                .iter()
                .any(|r| r[0] == CellValue::String("length".into()) && r[1] == CellValue::I64(2))
        );
        let groups = both("xinfo_groups", "XINFO GROUPS wse:stream");
        assert_eq!(
            names(&groups),
            vec![
                "name",
                "consumers",
                "pending",
                "last-delivered-id",
                "entries-read",
                "lag"
            ]
        );
        assert_eq!(groups.rows[0][0], CellValue::String("wse:g".into()));
        assert_eq!(groups.rows[0][2], CellValue::I64(1));
        assert_eq!(types(&groups)[2], "integer");
        // Idle and inactive milliseconds tick between the two captures.
        let (consumers, _) = both_columns("xinfo_consumers", "XINFO CONSUMERS wse:stream wse:g");
        assert_eq!(
            names(&consumers),
            vec!["name", "pending", "idle", "inactive"]
        );
        assert_eq!(consumers.rows[0][0], CellValue::String("wse:c".into()));
        assert_eq!(consumers.rows[0][1], CellValue::I64(1));

        let latest = both("latency_latest", "LATENCY LATEST");
        assert_eq!(
            names(&latest),
            vec!["event", "measured_at", "latest_ms", "max_ms"]
        );
        assert_eq!(latest.rows[0][0], CellValue::String("command".into()));
        assert!(matches!(&latest.rows[0][1], CellValue::String(s) if s.len() == 19));
        assert!(matches!(latest.rows[0][2], CellValue::I64(ms) if ms > 0));
        let history = both("latency_history", "LATENCY HISTORY command");
        assert_eq!(names(&history), vec!["measured_at", "latency_ms"]);
        assert_eq!(history.rows.len(), 1);
    }

    #[test]
    fn unshaped_module_replies_still_fall_back_to_generic() {
        let explain = both("ft_explain", "FT.EXPLAIN wse:idx @age:[30 50]");
        assert_eq!(names(&explain), vec!["result"]);
        // FT.PROFILE differs by protocol (array vs map) and keeps the generic
        // shape for each: never an error, never a panic.
        let two = shape_reply(
            &args("FT.PROFILE wse:idx SEARCH QUERY *"),
            fixture("ft_profile", 2),
        );
        let three = shape_reply(
            &args("FT.PROFILE wse:idx SEARCH QUERY *"),
            fixture("ft_profile", 3),
        );
        assert!(!two.rows.is_empty() && !three.rows.is_empty());
    }

    /// Live: an index over three JSON documents searches into three rows
    /// with the right columns, `SQLLY.INDEXES` lists it, and a 100-sample
    /// series ranges into 100 timestamp/value rows. Opt-in through
    /// `SQLLY_REDIS_STACK_URL` (`redis://127.0.0.1:6381/0`); keys are
    /// prefixed `wse:live:` and removed afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_stack_search_and_timeseries() {
        let Ok(url) = std::env::var("SQLLY_REDIS_STACK_URL") else {
            eprintln!("skipping: SQLLY_REDIS_STACK_URL not set");
            return;
        };
        let profile = super::super::profile_from_url("redis-stack-live", &url).unwrap();
        let client = super::super::open_client(&profile).await.unwrap();
        let run = |text: &'static str| {
            let client = &client;
            async move {
                super::super::query_rows_on_client(client, text)
                    .await
                    .unwrap()
            }
        };
        let cleanup = "FT.DROPINDEX wse:live:idx DD\nDEL wse:live:ts";
        let _ = super::super::query_rows_on_client(&client, cleanup).await;

        run(r#"["JSON.SET", "wse:live:doc:1", "$", "{\"name\":\"ann\",\"age\":31}"]"#).await;
        run(r#"["JSON.SET", "wse:live:doc:2", "$", "{\"name\":\"bob\",\"age\":42}"]"#).await;
        run(r#"["JSON.SET", "wse:live:doc:3", "$", "{\"name\":\"cyd\",\"age\":27}"]"#).await;
        run("FT.CREATE wse:live:idx ON JSON PREFIX 1 wse:live:doc: SCHEMA $.name AS name TEXT $.age AS age NUMERIC").await;
        // Indexing is asynchronous; wait for the three documents.
        for _ in 0..50 {
            let info = run("FT.INFO wse:live:idx").await;
            let docs = info
                .rows
                .iter()
                .find(|r| r[0] == CellValue::String("num_docs".into()))
                .map(|r| r[1].clone());
            if docs == Some(CellValue::I64(3)) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        let hits = run("FT.SEARCH wse:live:idx * RETURN 2 name age SORTBY age ASC").await;
        let hit_names: Vec<&str> = hits.columns.iter().map(|c| c.name.as_str()).collect();
        // The server orders the returned fields itself (the sort field
        // first); the key leads and the total trails regardless.
        assert_eq!(hit_names.len(), 4, "{hit_names:?}");
        assert_eq!(hit_names[0], "key");
        assert_eq!(hit_names[3], "total_results");
        assert!(
            hit_names.contains(&"name") && hit_names.contains(&"age"),
            "{hit_names:?}"
        );
        let name_at = hit_names.iter().position(|n| *n == "name").unwrap();
        assert_eq!(hits.rows.len(), 3, "{:?}", hits.rows);
        assert_eq!(hits.rows[0][0], CellValue::String("wse:live:doc:3".into()));
        assert_eq!(hits.rows[0][name_at], CellValue::String("cyd".into()));
        assert_eq!(hits.rows[0][3], CellValue::I64(3));

        let indexes = run("SQLLY.INDEXES").await;
        let index_names: Vec<&str> = indexes.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(index_names, vec!["index", "on", "prefixes", "num_docs"]);
        let ours = indexes
            .rows
            .iter()
            .find(|r| r[0] == CellValue::String("wse:live:idx".into()))
            .expect("SQLLY.INDEXES lists the live index");
        assert_eq!(ours[1], CellValue::String("JSON".into()));
        assert_eq!(ours[2], CellValue::String("wse:live:doc:".into()));
        assert_eq!(ours[3], CellValue::I64(3));

        let mut script = String::from("TS.CREATE wse:live:ts\n");
        for i in 0..100i64 {
            script.push_str(&format!(
                "TS.ADD wse:live:ts {} {i}.25\n",
                1_700_000_000_000 + i * 1000
            ));
        }
        script.push_str("TS.RANGE wse:live:ts - +");
        let range = super::super::query_rows_on_client(&client, &script)
            .await
            .unwrap();
        let range_names: Vec<&str> = range.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(range_names, vec!["timestamp", "value", "timestamp_ms"]);
        assert_eq!(range.columns[0].data_type, "datetime");
        assert_eq!(range.rows.len(), 100);
        assert_eq!(range.rows[99][1], CellValue::F64(99.25));
        assert_eq!(
            range.rows[0][0],
            CellValue::String("2023-11-14 22:13:20.000".into())
        );

        let _ = super::super::query_rows_on_client(&client, cleanup).await;
        let _ = super::super::query_rows_on_client(
            &client,
            "DEL wse:live:doc:1 wse:live:doc:2 wse:live:doc:3",
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_docs_rows_join_resp2_docs_and_info_by_upper_case_name() {
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let docs = Value::Array(vec![
            bulk("get"),
            Value::Array(vec![
                bulk("summary"),
                bulk("Returns the string value of a key."),
                bulk("group"),
                bulk("string"),
                bulk("arguments"),
                Value::Array(vec![Value::Array(vec![
                    bulk("name"),
                    bulk("key"),
                    bulk("type"),
                    bulk("key"),
                ])]),
            ]),
            bulk("FT.SEARCH"),
            Value::Array(vec![bulk("group"), bulk("module")]),
        ]);
        let info = Value::Array(vec![
            Value::Array(vec![
                bulk("get"),
                Value::Int(2),
                Value::Array(vec![bulk("readonly"), bulk("fast")]),
                Value::Int(1),
                Value::Int(1),
                Value::Int(1),
                Value::Array(vec![bulk("@read")]),
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![]),
            ]),
            Value::Array(vec![bulk("ping"), Value::Int(-1)]),
        ]);
        let shaped = command_docs_rows(docs, info);
        let names: Vec<&str> = shaped.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["command", "docs", "info"]);
        let first: Vec<&CellValue> = shaped.rows.iter().map(|r| &r[0]).collect();
        assert_eq!(
            first,
            vec![
                &CellValue::String("FT.SEARCH".into()),
                &CellValue::String("GET".into()),
                &CellValue::String("PING".into())
            ]
        );
        let CellValue::String(get_docs) = &shaped.rows[1][1] else {
            panic!()
        };
        assert!(get_docs.starts_with("[\"summary\""), "{get_docs}");
        let CellValue::String(get_info) = &shaped.rows[1][2] else {
            panic!()
        };
        assert!(get_info.contains("\"readonly\""), "{get_info}");
        assert_eq!(shaped.rows[2][1], CellValue::String("{}".into()));
        assert_eq!(shaped.rows[0][2], CellValue::String("[]".into()));
        // The IntelliSense vocabulary reads the rows back.
        let vocabulary = sqlly_intel::redis_complete::CommandVocabulary::from_server_rows(
            shaped.rows.iter().map(|r| {
                let (CellValue::String(n), CellValue::String(d), CellValue::String(i)) =
                    (&r[0], &r[1], &r[2])
                else {
                    panic!()
                };
                (n.as_str(), d.as_str(), i.as_str())
            }),
        )
        .expect("parses");
        assert_eq!(vocabulary.get("get").unwrap().syntax(), "GET key");
        assert!(vocabulary.get("GET").unwrap().info.has_flag("readonly"));
        assert!(vocabulary.get("FT.SEARCH").is_some());
        assert!(vocabulary.get("SQLLY.KEYS").is_some());
    }

    fn args(text: &str) -> Vec<Vec<u8>> {
        text.split(' ').map(|s| s.as_bytes().to_vec()).collect()
    }

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn names(shaped: &Shaped) -> Vec<&str> {
        shaped.columns.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn scalars_become_one_result_row() {
        let ok = shape_reply(&args("SET k v"), Value::Okay);
        assert_eq!(names(&ok), vec!["result"]);
        assert_eq!(ok.rows, vec![vec![CellValue::String("OK".into())]]);
        let n = shape_reply(&args("INCR n"), Value::Int(7));
        assert_eq!(n.rows, vec![vec![CellValue::I64(7)]]);
        let nil = shape_reply(&args("GET missing"), Value::Nil);
        assert_eq!(nil.rows, vec![vec![CellValue::Null]]);
        let pong = shape_reply(&args("PING"), Value::SimpleString("PONG".into()));
        assert_eq!(pong.rows, vec![vec![CellValue::String("PONG".into())]]);
        let bytes = shape_reply(&args("GET bin"), Value::BulkString(vec![0xff, 0xfe]));
        assert_eq!(bytes.rows, vec![vec![CellValue::Bytes(vec![0xff, 0xfe])]]);
    }

    #[test]
    fn arrays_get_index_rows_and_nested_arrays_become_json() {
        let shaped = shape_reply(
            &args("LRANGE l 0 -1"),
            Value::Array(vec![
                bulk("a"),
                Value::Array(vec![Value::Int(1), bulk("b")]),
                Value::Nil,
            ]),
        );
        assert_eq!(names(&shaped), vec!["index", "value"]);
        assert_eq!(
            shaped.rows,
            vec![
                vec![CellValue::I64(0), CellValue::String("a".into())],
                vec![CellValue::I64(1), CellValue::String("[1,\"b\"]".into())],
                vec![CellValue::I64(2), CellValue::Null],
            ]
        );
        let empty = shape_reply(&args("SMEMBERS s"), Value::Array(vec![]));
        assert!(empty.rows.is_empty());
        assert_eq!(names(&empty), vec!["index", "value"]);
    }

    #[test]
    fn hashes_scores_and_maps_become_pairs() {
        let h = shape_reply(
            &args("HGETALL h"),
            Value::Array(vec![bulk("name"), bulk("ann"), bulk("age"), bulk("3")]),
        );
        assert_eq!(names(&h), vec!["field", "value"]);
        assert_eq!(h.rows.len(), 2);
        let z = shape_reply(
            &args("ZRANGE z 0 -1 WITHSCORES"),
            Value::Array(vec![bulk("a"), bulk("1.5")]),
        );
        assert_eq!(names(&z), vec!["member", "score"]);
        let plain = shape_reply(&args("ZRANGE z 0 -1"), Value::Array(vec![bulk("a")]));
        assert_eq!(names(&plain), vec!["index", "value"]);
        let cfg = shape_reply(
            &args("CONFIG GET databases"),
            Value::Array(vec![bulk("databases"), bulk("16")]),
        );
        assert_eq!(names(&cfg), vec!["field", "value"]);
        let map = shape_reply(
            &args("HGETALL h"),
            Value::Map(vec![(bulk("k"), Value::Int(1))]),
        );
        assert_eq!(names(&map), vec!["field", "value"]);
        assert_eq!(
            map.rows,
            vec![vec![CellValue::String("k".into()), CellValue::I64(1)]]
        );
    }

    #[test]
    fn scan_carries_the_cursor_on_every_row() {
        let scan = shape_reply(
            &args("SCAN 0"),
            Value::Array(vec![bulk("17"), Value::Array(vec![bulk("a"), bulk("b")])]),
        );
        assert_eq!(names(&scan), vec!["cursor", "key"]);
        assert_eq!(scan.rows.len(), 2);
        assert_eq!(scan.rows[1][0], CellValue::String("17".into()));
        let empty = shape_reply(
            &args("SCAN 0"),
            Value::Array(vec![bulk("0"), Value::Array(vec![])]),
        );
        assert_eq!(
            empty.rows,
            vec![vec![CellValue::String("0".into()), CellValue::Null]]
        );
        let hscan = shape_reply(
            &args("HSCAN h 0"),
            Value::Array(vec![bulk("0"), Value::Array(vec![bulk("f"), bulk("v")])]),
        );
        assert_eq!(names(&hscan), vec!["cursor", "field", "value"]);
    }

    #[test]
    fn streams_become_id_plus_fields_json() {
        let x = shape_reply(
            &args("XRANGE s - +"),
            Value::Array(vec![Value::Array(vec![
                bulk("1-0"),
                Value::Array(vec![bulk("a"), bulk("1"), bulk("b"), bulk("2")]),
            ])]),
        );
        assert_eq!(names(&x), vec!["id", "fields"]);
        assert_eq!(
            x.rows,
            vec![vec![
                CellValue::String("1-0".into()),
                CellValue::String("{\"a\":\"1\",\"b\":\"2\"}".into())
            ]]
        );
    }

    #[test]
    fn info_client_list_and_slowlog_are_tabular() {
        let info = shape_reply(
            &args("INFO server"),
            bulk("# Server\r\nredis_version:7.2.4\r\nuptime_in_seconds:10\r\n"),
        );
        assert_eq!(names(&info), vec!["section", "key", "value"]);
        assert_eq!(info.rows[0][0], CellValue::String("Server".into()));
        assert_eq!(info.rows[0][1], CellValue::String("redis_version".into()));

        let clients = shape_reply(
            &args("CLIENT LIST"),
            bulk(
                "id=3 addr=127.0.0.1:5 name= age=10 idle=2 flags=N db=0 cmd=client|list user=default\nid=4 addr=127.0.0.1:6 age=1 idle=0 flags=b db=2 cmd=blpop\n",
            ),
        );
        assert_eq!(
            &names(&clients)[..8],
            &[
                "session_id",
                "status",
                "login_name",
                "host_name",
                "database_name",
                "seconds_running",
                "idle_seconds",
                "query_text"
            ]
        );
        assert_eq!(clients.rows[0][0], CellValue::I64(3));
        assert_eq!(clients.rows[0][1], CellValue::String("idle".into()));
        assert_eq!(clients.rows[0][7], CellValue::String("client list".into()));
        assert_eq!(clients.rows[1][1], CellValue::String("blocked".into()));
        assert_eq!(clients.rows[1][2], CellValue::String("default".into()));
        assert!(names(&clients).contains(&"addr"));

        let slow = shape_reply(
            &args("SLOWLOG GET 2"),
            Value::Array(vec![Value::Array(vec![
                Value::Int(12),
                Value::Int(1_700_000_000),
                Value::Int(2500),
                Value::Array(vec![bulk("KEYS"), bulk("*")]),
                bulk("127.0.0.1:1"),
                bulk("cli"),
            ])]),
        );
        assert_eq!(
            names(&slow),
            vec![
                "query_id",
                "executed_at",
                "duration_us",
                "avg_duration_ms",
                "query_text",
                "client_addr",
                "client_name"
            ]
        );
        assert_eq!(slow.rows[0][0], CellValue::I64(12));
        assert_eq!(slow.rows[0][2], CellValue::I64(2500));
        assert_eq!(slow.rows[0][3], CellValue::F64(2.5));
        assert_eq!(slow.rows[0][4], CellValue::String("KEYS *".into()));
        assert_eq!(
            slow.rows[0][1],
            CellValue::String("2023-11-14 22:13:20".into())
        );
    }
}
