//! Paged readers for the key view — one bounded page of a container key
//! plus where the next page starts and how big the whole key is, so a view
//! can say "showing 1,000 of 48,213" and offer Load more instead of silently
//! capping at a fixed range:
//!
//! - `SQLLY.HPAGE key cursor count` — `HSCAN` + `HLEN`: field, value
//! - `SQLLY.LPAGE key start count` — `LRANGE` + `LLEN`: index, value
//! - `SQLLY.SPAGE key cursor count` — `SSCAN` + `SCARD`: member
//! - `SQLLY.ZPAGE key start count` — `ZRANGE … WITHSCORES` + `ZCARD`: index, member, score
//! - `SQLLY.XPAGE key from count` — `XRANGE … COUNT` + `XLEN`: id, fields
//! - `SQLLY.STRPAGE key offset count` — `STRLEN`, then `GET` for values up
//!   to [`STRING_WINDOW_BYTES`] or `GETRANGE` windows above it: value, offset
//!
//! Every answer carries two trailing columns: `next` (the cursor, index,
//! stream id or byte offset to continue from, `NULL` when the page was the
//! last) and `total` (the key's length when the page was read). An empty
//! page still answers one row — data columns `NULL` — so the totals are
//! never lost. Values, members, fields and scores are bulk strings kept as
//! their exact bytes (text when valid UTF-8, bytes otherwise); nothing is
//! parsed into a number.

use redis::Value;
use sqlly_core::{CellValue, StudioError, StudioResult};

use super::RedisClient;
use super::shape::{Shaped, cell, json, json_key};

/// Elements per page when a script gives no count.
pub const DEFAULT_PAGE: usize = 1000;
/// The largest page a script may ask for.
pub const MAX_PAGE: usize = 10_000;
/// Strings up to this size are read whole with `GET`; longer ones are read
/// in `GETRANGE` windows of this size.
pub const STRING_WINDOW_BYTES: usize = 1024 * 1024;

/// Clamp a requested page size.
pub fn page_size(requested: Option<usize>) -> usize {
    requested.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE)
}

fn total_of(value: &Value) -> i64 {
    match value {
        Value::Int(n) => *n,
        _ => 0,
    }
}

fn next_cell(next: Option<String>) -> CellValue {
    next.map(CellValue::String).unwrap_or(CellValue::Null)
}

/// Split a `*SCAN` reply into its cursor (`None` once it came back to 0)
/// and elements.
fn scan_parts(reply: Value) -> (Option<String>, Vec<Value>) {
    let mut iter = match reply {
        Value::Array(items) => items.into_iter(),
        _ => return (None, Vec::new()),
    };
    let cursor = match iter.next() {
        Some(Value::BulkString(b)) => String::from_utf8_lossy(&b).into_owned(),
        Some(Value::SimpleString(s)) => s,
        Some(Value::Int(n)) => n.to_string(),
        _ => "0".to_string(),
    };
    let elements = match iter.next() {
        Some(Value::Array(items)) | Some(Value::Set(items)) => items,
        _ => Vec::new(),
    };
    let next = (cursor != "0").then_some(cursor);
    (next, elements)
}

/// Flat `[a, b, a, b]` (RESP2) or nested `[[a, b], [a, b]]` (RESP3 `ZRANGE
/// WITHSCORES`) into pairs.
fn pair_up(items: Vec<Value>) -> Vec<(Value, Value)> {
    let nested = items
        .iter()
        .all(|i| matches!(i, Value::Array(pair) if pair.len() == 2));
    if nested && !items.is_empty() {
        return items
            .into_iter()
            .map(|item| match item {
                Value::Array(mut pair) => {
                    let v = pair.pop().unwrap_or(Value::Nil);
                    let k = pair.pop().unwrap_or(Value::Nil);
                    (k, v)
                }
                other => (other, Value::Nil),
            })
            .collect();
    }
    let mut out = Vec::with_capacity(items.len() / 2);
    let mut iter = items.into_iter();
    while let Some(k) = iter.next() {
        out.push((k, iter.next().unwrap_or(Value::Nil)));
    }
    out
}

pub(crate) fn with_trailer(
    columns: &[(&str, &str)],
    mut rows: Vec<Vec<CellValue>>,
    next: Option<String>,
    total: i64,
) -> Shaped {
    let mut all: Vec<(&str, &str)> = columns.to_vec();
    all.push(("next", "string"));
    all.push(("total", "integer"));
    if rows.is_empty() {
        rows.push(vec![CellValue::Null; columns.len()]);
    }
    for row in &mut rows {
        row.push(next_cell(next.clone()));
        row.push(CellValue::I64(total));
    }
    Shaped::new(&all, rows)
}

/// `HSCAN` reply + `HLEN` → field, value, next, total.
pub fn shape_hash_page(scan: Value, len: &Value) -> Shaped {
    let (next, elements) = scan_parts(scan);
    let rows = pair_up(elements)
        .into_iter()
        .map(|(f, v)| vec![cell(f), cell(v)])
        .collect();
    with_trailer(
        &[("field", "string"), ("value", "string")],
        rows,
        next,
        total_of(len),
    )
}

/// `SSCAN` reply + `SCARD` → member, next, total.
pub fn shape_set_page(scan: Value, card: &Value) -> Shaped {
    let (next, elements) = scan_parts(scan);
    let rows = elements.into_iter().map(|m| vec![cell(m)]).collect();
    with_trailer(&[("member", "string")], rows, next, total_of(card))
}

/// `LRANGE start …` reply + `LLEN` → index, value, next, total.
pub fn shape_list_page(range: Value, start: i64, len: &Value) -> Shaped {
    let elements = match range {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    let total = total_of(len);
    let read = elements.len() as i64;
    let rows = elements
        .into_iter()
        .enumerate()
        .map(|(i, v)| vec![CellValue::I64(start + i as i64), cell(v)])
        .collect();
    let next = (read > 0 && start + read < total).then(|| (start + read).to_string());
    with_trailer(
        &[("index", "integer"), ("value", "string")],
        rows,
        next,
        total,
    )
}

/// `ZRANGE start … WITHSCORES` reply + `ZCARD` → index, member, score,
/// next, total. Scores stay the server's own text.
pub fn shape_zset_page(range: Value, start: i64, card: &Value) -> Shaped {
    let elements = match range {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    let total = total_of(card);
    let pairs = pair_up(elements);
    let read = pairs.len() as i64;
    let rows = pairs
        .into_iter()
        .enumerate()
        .map(|(i, (m, s))| vec![CellValue::I64(start + i as i64), cell(m), score_cell(s)])
        .collect();
    let next = (read > 0 && start + read < total).then(|| (start + read).to_string());
    with_trailer(
        &[
            ("index", "integer"),
            ("member", "string"),
            ("score", "string"),
        ],
        rows,
        next,
        total,
    )
}

/// A score as text, never a float: RESP2 answers a bulk string already;
/// RESP3 answers a double, rendered the way Redis prints it.
fn score_cell(value: Value) -> CellValue {
    match value {
        Value::Double(d) => CellValue::String(format_score(d)),
        other => cell(other),
    }
}

fn format_score(d: f64) -> String {
    if d.is_infinite() {
        if d > 0.0 { "inf" } else { "-inf" }.to_string()
    } else if d.fract() == 0.0 && d.abs() < 1e17 {
        format!("{}", d as i64)
    } else {
        format!("{d}")
    }
}

/// `XRANGE from + COUNT n` reply + `XLEN` → id, fields, next, total. `next`
/// is the exclusive form `(id` of the last entry read, which `XRANGE`
/// accepts as a start on Redis 6.2 and later.
pub fn shape_stream_page(range: Value, count: usize, len: &Value) -> Shaped {
    let entries = match range {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    let total = total_of(len);
    let read = entries.len();
    let mut last_id: Option<String> = None;
    let rows = entries
        .into_iter()
        .map(|entry| match entry {
            Value::Array(mut parts) if parts.len() == 2 => {
                let fields = parts.pop().unwrap_or(Value::Nil);
                let id = parts.pop().unwrap_or(Value::Nil);
                if let Value::BulkString(b) = &id {
                    last_id = Some(String::from_utf8_lossy(b).into_owned());
                } else if let Value::SimpleString(s) = &id {
                    last_id = Some(s.clone());
                }
                let fields = match fields {
                    Value::Array(flat) => {
                        let mut map = serde_json::Map::new();
                        for (k, v) in pair_up(flat) {
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
    let next = if read > 0 && read as i64 >= count as i64 {
        last_id.map(|id| format!("({id}"))
    } else {
        None
    };
    with_trailer(&[("id", "string"), ("fields", "json")], rows, next, total)
}

/// A string's window → value, offset, next, total.
pub fn shape_string_page(value: Value, offset: i64, total: i64) -> Shaped {
    let bytes_read = match &value {
        Value::BulkString(b) => b.len() as i64,
        _ => 0,
    };
    let end = offset + bytes_read;
    let next = (bytes_read > 0 && end < total).then(|| end.to_string());
    let rows = match value {
        Value::Nil => Vec::new(),
        other => vec![vec![cell(other), CellValue::I64(offset)]],
    };
    with_trailer(
        &[("value", "string"), ("offset", "integer")],
        rows,
        next,
        total,
    )
}

fn two(replies: Vec<Value>) -> StudioResult<(Value, Value)> {
    let mut iter = replies.into_iter();
    match (iter.next(), iter.next()) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err(StudioError::QueryExecution(
            "the server answered a page with fewer replies than asked".into(),
        )),
    }
}

/// `SQLLY.HPAGE`.
pub async fn hash_page(
    client: &RedisClient,
    key: &[u8],
    cursor: &str,
    count: usize,
) -> StudioResult<Shaped> {
    let mut pipe = redis::pipe();
    pipe.cmd("HSCAN")
        .arg(key)
        .arg(cursor)
        .arg("COUNT")
        .arg(count);
    pipe.cmd("HLEN").arg(key);
    let (scan, len) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_hash_page(scan, &len))
}

/// `SQLLY.SPAGE`.
pub async fn set_page(
    client: &RedisClient,
    key: &[u8],
    cursor: &str,
    count: usize,
) -> StudioResult<Shaped> {
    let mut pipe = redis::pipe();
    pipe.cmd("SSCAN")
        .arg(key)
        .arg(cursor)
        .arg("COUNT")
        .arg(count);
    pipe.cmd("SCARD").arg(key);
    let (scan, card) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_set_page(scan, &card))
}

/// `SQLLY.LPAGE`.
pub async fn list_page(
    client: &RedisClient,
    key: &[u8],
    start: i64,
    count: usize,
) -> StudioResult<Shaped> {
    let start = start.max(0);
    let mut pipe = redis::pipe();
    pipe.cmd("LRANGE")
        .arg(key)
        .arg(start)
        .arg(start + count as i64 - 1);
    pipe.cmd("LLEN").arg(key);
    let (range, len) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_list_page(range, start, &len))
}

/// `SQLLY.ZPAGE`.
pub async fn zset_page(
    client: &RedisClient,
    key: &[u8],
    start: i64,
    count: usize,
) -> StudioResult<Shaped> {
    let start = start.max(0);
    let mut pipe = redis::pipe();
    pipe.cmd("ZRANGE")
        .arg(key)
        .arg(start)
        .arg(start + count as i64 - 1)
        .arg("WITHSCORES");
    pipe.cmd("ZCARD").arg(key);
    let (range, card) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_zset_page(range, start, &card))
}

/// `SQLLY.XPAGE`.
pub async fn stream_page(
    client: &RedisClient,
    key: &[u8],
    from: &str,
    count: usize,
) -> StudioResult<Shaped> {
    let from = if from.is_empty() { "-" } else { from };
    let mut pipe = redis::pipe();
    pipe.cmd("XRANGE")
        .arg(key)
        .arg(from)
        .arg("+")
        .arg("COUNT")
        .arg(count);
    pipe.cmd("XLEN").arg(key);
    let (range, len) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_stream_page(range, count, &len))
}

/// `SQLLY.STRPAGE`: the whole value when it fits one window, else the
/// window at `offset`.
pub async fn string_page(
    client: &RedisClient,
    key: &[u8],
    offset: i64,
    window: usize,
) -> StudioResult<Shaped> {
    let offset = offset.max(0);
    let window = window.clamp(1, STRING_WINDOW_BYTES);
    let mut strlen = redis::cmd("STRLEN");
    strlen.arg(key);
    let total = total_of(&client.run(&strlen).await?);
    let value = if offset == 0 && total <= window as i64 {
        let mut get = redis::cmd("GET");
        get.arg(key);
        client.run(&get).await?
    } else {
        let mut range = redis::cmd("GETRANGE");
        range.arg(key).arg(offset).arg(offset + window as i64 - 1);
        client.run(&range).await?
    };
    Ok(shape_string_page(value, offset, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn names(shaped: &Shaped) -> Vec<&str> {
        shaped.columns.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn page_size_is_clamped() {
        assert_eq!(page_size(None), DEFAULT_PAGE);
        assert_eq!(page_size(Some(0)), 1);
        assert_eq!(page_size(Some(50)), 50);
        assert_eq!(page_size(Some(1_000_000)), MAX_PAGE);
    }

    #[test]
    fn page_hash_keeps_cursor_total_and_exact_bytes() {
        let scan = Value::Array(vec![
            bulk("1234"),
            Value::Array(vec![
                bulk("count"),
                bulk("00014"),
                bulk("raw"),
                Value::BulkString(vec![0xff, 0x00]),
            ]),
        ]);
        let shaped = shape_hash_page(scan, &Value::Int(5000));
        assert_eq!(names(&shaped), vec!["field", "value", "next", "total"]);
        assert_eq!(shaped.rows.len(), 2);
        assert_eq!(shaped.rows[0][1], CellValue::String("00014".into()));
        assert_eq!(shaped.rows[1][1], CellValue::Bytes(vec![0xff, 0x00]));
        assert_eq!(shaped.rows[0][2], CellValue::String("1234".into()));
        assert_eq!(shaped.rows[0][3], CellValue::I64(5000));
        // The last page: cursor 0 → next NULL.
        let last = shape_hash_page(
            Value::Array(vec![bulk("0"), Value::Array(vec![bulk("f"), bulk("v")])]),
            &Value::Int(1),
        );
        assert_eq!(last.rows[0][2], CellValue::Null);
        // An empty page still carries the trailer.
        let empty = shape_hash_page(
            Value::Array(vec![bulk("0"), Value::Array(vec![])]),
            &Value::Int(0),
        );
        assert_eq!(empty.rows.len(), 1);
        assert_eq!(empty.rows[0][0], CellValue::Null);
        assert_eq!(empty.rows[0][3], CellValue::I64(0));
    }

    #[test]
    fn page_list_and_zset_report_the_next_index() {
        let range = Value::Array(vec![bulk("a"), bulk("b"), bulk("c")]);
        let shaped = shape_list_page(range, 0, &Value::Int(10));
        assert_eq!(names(&shaped), vec!["index", "value", "next", "total"]);
        assert_eq!(shaped.rows[2][0], CellValue::I64(2));
        assert_eq!(shaped.rows[2][2], CellValue::String("3".into()));
        let tail = shape_list_page(Value::Array(vec![bulk("j")]), 9, &Value::Int(10));
        assert_eq!(tail.rows[0][0], CellValue::I64(9));
        assert_eq!(tail.rows[0][2], CellValue::Null);
        // RESP2 flat pairs and RESP3 nested pairs shape alike; scores stay text.
        let flat = Value::Array(vec![bulk("a"), bulk("1.5"), bulk("b"), bulk("2")]);
        let nested = Value::Array(vec![
            Value::Array(vec![bulk("a"), Value::Double(1.5)]),
            Value::Array(vec![bulk("b"), Value::Double(2.0)]),
        ]);
        let from_flat = shape_zset_page(flat, 0, &Value::Int(2));
        let from_nested = shape_zset_page(nested, 0, &Value::Int(2));
        assert_eq!(
            names(&from_flat),
            vec!["index", "member", "score", "next", "total"]
        );
        assert_eq!(from_flat.rows, from_nested.rows);
        assert_eq!(from_flat.rows[0][2], CellValue::String("1.5".into()));
        assert_eq!(from_flat.rows[1][2], CellValue::String("2".into()));
        assert_eq!(from_flat.rows[1][3], CellValue::Null);
        assert_eq!(format_score(f64::INFINITY), "inf");
        assert_eq!(format_score(-0.25), "-0.25");
    }

    #[test]
    fn page_set_stream_and_string_carry_their_trailers() {
        let set = shape_set_page(
            Value::Array(vec![bulk("7"), Value::Array(vec![bulk("m1"), bulk("m2")])]),
            &Value::Int(40),
        );
        assert_eq!(names(&set), vec!["member", "next", "total"]);
        assert_eq!(set.rows[1][1], CellValue::String("7".into()));
        assert_eq!(set.rows[1][2], CellValue::I64(40));

        let entry = |id: &str| {
            Value::Array(vec![
                bulk(id),
                Value::Array(vec![bulk("f"), bulk("v"), bulk("n"), bulk("007")]),
            ])
        };
        let stream = shape_stream_page(
            Value::Array(vec![entry("1-0"), entry("2-0")]),
            2,
            &Value::Int(9),
        );
        assert_eq!(names(&stream), vec!["id", "fields", "next", "total"]);
        assert_eq!(
            stream.rows[1][1],
            CellValue::String("{\"f\":\"v\",\"n\":\"007\"}".into())
        );
        assert_eq!(stream.rows[1][2], CellValue::String("(2-0".into()));
        let short = shape_stream_page(Value::Array(vec![entry("1-0")]), 2, &Value::Int(1));
        assert_eq!(short.rows[0][2], CellValue::Null);

        let whole = shape_string_page(bulk("00014"), 0, 5);
        assert_eq!(names(&whole), vec!["value", "offset", "next", "total"]);
        assert_eq!(whole.rows[0][0], CellValue::String("00014".into()));
        assert_eq!(whole.rows[0][2], CellValue::Null);
        assert_eq!(whole.rows[0][3], CellValue::I64(5));
        let window = shape_string_page(Value::BulkString(vec![b'x'; 1024]), 0, 3000);
        assert_eq!(window.rows[0][2], CellValue::String("1024".into()));
        let last = shape_string_page(Value::BulkString(vec![b'x'; 952]), 2048, 3000);
        assert_eq!(last.rows[0][1], CellValue::I64(2048));
        assert_eq!(last.rows[0][2], CellValue::Null);
        let missing = shape_string_page(Value::Nil, 0, 0);
        assert_eq!(missing.rows[0][0], CellValue::Null);
        assert_eq!(missing.rows[0][3], CellValue::I64(0));
    }

    /// Live: a 5,000-member hash pages in 1,000s with a stable cursor and
    /// no duplicates, a 3 MB string reads its first window and reports the
    /// full STRLEN, and the list / zset / set / stream pages continue
    /// where they said. Database 11, keys prefixed `wsa:`. Opt-in through
    /// `SQLLY_REDIS_URL`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn page_live_walks_big_keys_with_stable_cursors() {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let mut profile = super::super::profile_from_url("redis-page", &url).expect("profile");
        profile.database = Some("11".to_string());
        let client = super::super::open_client(&profile).await.expect("client");
        let prefix = format!("wsa:page:{}", std::process::id());
        let hash = format!("{prefix}:h");
        let big = format!("{prefix}:big");
        let list = format!("{prefix}:l");
        let zset = format!("{prefix}:z");
        let set = format!("{prefix}:s");
        let stream = format!("{prefix}:x");
        let mut seed = redis::pipe();
        for i in 0..5000 {
            seed.cmd("HSET")
                .arg(&hash)
                .arg(format!("f{i}"))
                .arg(format!("{i:05}"));
            seed.cmd("RPUSH").arg(&list).arg(format!("{i:05}"));
            seed.cmd("ZADD").arg(&zset).arg(i).arg(format!("m{i}"));
            seed.cmd("SADD").arg(&set).arg(format!("s{i}"));
        }
        for i in 0..25 {
            seed.cmd("XADD").arg(&stream).arg("*").arg("i").arg(i);
        }
        seed.cmd("SET").arg(&big).arg(vec![b'x'; 3 * 1024 * 1024]);
        client.run_pipeline(&seed).await.expect("seed");

        // Hash: follow the cursor until NULL, collecting every field once.
        let mut seen = std::collections::HashSet::new();
        let mut cursor = "0".to_string();
        let mut pages = 0;
        loop {
            let page = hash_page(&client, hash.as_bytes(), &cursor, 1000)
                .await
                .expect("hash page");
            pages += 1;
            assert_eq!(page.rows[0][3], CellValue::I64(5000));
            for row in &page.rows {
                if let CellValue::String(field) = &row[0] {
                    assert!(seen.insert(field.clone()), "duplicate {field}");
                }
            }
            match &page.rows[0][2] {
                CellValue::String(next) => cursor = next.clone(),
                _ => break,
            }
            assert!(pages < 100, "cursor never returned to 0");
        }
        assert_eq!(seen.len(), 5000);
        assert!(
            pages >= 2,
            "a 5,000-field hash pages in more than one HSCAN"
        );
        // Leading zeros survive as text.
        let first = hash_page(&client, hash.as_bytes(), "0", 10).await.unwrap();
        assert!(
            first
                .rows
                .iter()
                .all(|r| matches!(&r[1], CellValue::String(v) if v.len() == 5))
        );

        // String: the first window and the whole length.
        let window = string_page(&client, big.as_bytes(), 0, STRING_WINDOW_BYTES)
            .await
            .expect("string page");
        assert_eq!(window.rows[0][3], CellValue::I64(3 * 1024 * 1024));
        match &window.rows[0][0] {
            CellValue::String(v) => assert_eq!(v.len(), STRING_WINDOW_BYTES),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            window.rows[0][2],
            CellValue::String(STRING_WINDOW_BYTES.to_string())
        );
        let last = string_page(
            &client,
            big.as_bytes(),
            2 * 1024 * 1024,
            STRING_WINDOW_BYTES,
        )
        .await
        .unwrap();
        assert_eq!(last.rows[0][2], CellValue::Null);

        // List and zset continue at the index they announce.
        let l1 = list_page(&client, list.as_bytes(), 0, 1000).await.unwrap();
        assert_eq!(l1.rows.len(), 1000);
        assert_eq!(l1.rows[0][2], CellValue::String("1000".into()));
        assert_eq!(l1.rows[0][1], CellValue::String("00000".into()));
        let l2 = list_page(&client, list.as_bytes(), 1000, 1000)
            .await
            .unwrap();
        assert_eq!(l2.rows[0][0], CellValue::I64(1000));
        assert_eq!(l2.rows[0][1], CellValue::String("01000".into()));
        let z = zset_page(&client, zset.as_bytes(), 4990, 1000)
            .await
            .unwrap();
        assert_eq!(z.rows.len(), 10);
        assert_eq!(z.rows[9][1], CellValue::String("m4999".into()));
        assert_eq!(z.rows[9][2], CellValue::String("4999".into()));
        assert_eq!(z.rows[9][3], CellValue::Null);
        assert_eq!(z.rows[9][4], CellValue::I64(5000));
        let s = set_page(&client, set.as_bytes(), "0", 1000).await.unwrap();
        assert_eq!(s.rows[0][2], CellValue::I64(5000));
        // Stream: two pages of 10, then the rest.
        let x1 = stream_page(&client, stream.as_bytes(), "-", 10)
            .await
            .unwrap();
        assert_eq!(x1.rows.len(), 10);
        assert_eq!(x1.rows[0][3], CellValue::I64(25));
        let next = match &x1.rows[0][2] {
            CellValue::String(n) => n.clone(),
            other => panic!("{other:?}"),
        };
        assert!(next.starts_with('('));
        let x2 = stream_page(&client, stream.as_bytes(), &next, 10)
            .await
            .unwrap();
        assert_eq!(x2.rows.len(), 10);
        assert_ne!(
            x1.rows[9][0], x2.rows[0][0],
            "exclusive start skips the last id"
        );
        let next = match &x2.rows[0][2] {
            CellValue::String(n) => n.clone(),
            other => panic!("{other:?}"),
        };
        let x3 = stream_page(&client, stream.as_bytes(), &next, 10)
            .await
            .unwrap();
        assert_eq!(x3.rows.len(), 5);
        assert_eq!(x3.rows[0][2], CellValue::Null);

        // Through the script path, as the view runs it.
        let text = format!(
            "SQLLY.LPAGE {} 4998 10",
            sqlly_intel::query::safety::redis::quote_argument(list.as_bytes())
        );
        let scripted = super::super::query_rows_on_client(&client, &text)
            .await
            .expect("script");
        assert_eq!(scripted.rows.len(), 2);
        assert_eq!(scripted.rows[1][1], CellValue::String("04999".into()));

        let mut cleanup = redis::cmd("DEL");
        cleanup
            .arg(&hash)
            .arg(&big)
            .arg(&list)
            .arg(&zset)
            .arg(&set)
            .arg(&stream);
        client.run(&cleanup).await.expect("cleanup");
    }
}
