//! Consumer-group readers for the key view's stream tabs:
//!
//! - `SQLLY.XGROUPS key` — `XINFO GROUPS` + `XLEN`: one row per group with
//!   `group, consumers, pending, last_delivered_id, entries_read, lag,
//!   stream_length`. The groups are shaped by the shared `XINFO GROUPS`
//!   shape and projected onto fixed columns, so the view reads them by name
//!   whatever the server version reports (`entries_read` and `lag` are
//!   Redis 7 fields and `NULL` before it; `lag` is also `NULL` when the
//!   server cannot tell).
//! - `SQLLY.XPENDING key group [from] [count]` — the extended `XPENDING`
//!   form plus the summary count: `id, consumer, idle_ms, deliveries`
//!   followed by the key view's `next` / `total` trailer (`next` is the
//!   exclusive `(id` of the last entry when the page was full, `total` the
//!   group's whole pending count).
//!
//! Group and consumer names are bulk strings kept as their exact bytes;
//! entry ids are ASCII. Both only ever run read-only commands.

use redis::Value;
use sqlly_core::{CellValue, StudioError, StudioResult};

use super::RedisClient;
use super::page::with_trailer;
use super::shape::{Shaped, cell, shape_reply};

/// The fixed columns of `SQLLY.XGROUPS`, with the `XINFO GROUPS` field
/// each one is read from.
const GROUP_COLUMNS: &[(&str, &str, &str)] = &[
    ("group", "string", "name"),
    ("consumers", "integer", "consumers"),
    ("pending", "integer", "pending"),
    ("last_delivered_id", "string", "last-delivered-id"),
    ("entries_read", "integer", "entries-read"),
    ("lag", "integer", "lag"),
];

/// `XINFO GROUPS` reply + `XLEN` → the fixed group rows. A stream with no
/// groups answers no rows.
pub fn shape_groups(key: &[u8], groups: Value, len: &Value) -> Shaped {
    let shaped = shape_reply(
        &[b"XINFO".to_vec(), b"GROUPS".to_vec(), key.to_vec()],
        groups,
    );
    let index_of = |field: &str| shaped.columns.iter().position(|c| c.name == field);
    let sources: Vec<Option<usize>> = GROUP_COLUMNS
        .iter()
        .map(|(_, _, field)| index_of(field))
        .collect();
    let length = match len {
        Value::Int(n) => CellValue::I64(*n),
        _ => CellValue::Null,
    };
    let rows = if sources[0].is_none() {
        // Not a group listing (an empty array shapes as nothing useful).
        Vec::new()
    } else {
        shaped
            .rows
            .into_iter()
            .map(|row| {
                let mut out: Vec<CellValue> = sources
                    .iter()
                    .map(|source| {
                        source
                            .and_then(|i| row.get(i).cloned())
                            .unwrap_or(CellValue::Null)
                    })
                    .collect();
                out.push(length.clone());
                out
            })
            .collect()
    };
    let mut columns: Vec<(&str, &str)> = GROUP_COLUMNS.iter().map(|(n, t, _)| (*n, *t)).collect();
    columns.push(("stream_length", "integer"));
    Shaped::new(&columns, rows)
}

/// The summary `XPENDING key group` reply's count.
fn pending_count(summary: &Value) -> i64 {
    match summary {
        Value::Array(items) => match items.first() {
            Some(Value::Int(n)) => *n,
            _ => 0,
        },
        _ => 0,
    }
}

fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        _ => None,
    }
}

/// Extended `XPENDING key group from + count` reply + the summary →
/// id, consumer, idle_ms, deliveries, next, total.
pub fn shape_pending(range: Value, summary: &Value, count: usize) -> Shaped {
    let entries = match range {
        Value::Array(items) => items,
        _ => Vec::new(),
    };
    let read = entries.len();
    let mut last_id: Option<String> = None;
    let rows = entries
        .into_iter()
        .map(|entry| match entry {
            Value::Array(parts) => {
                let mut parts = parts.into_iter();
                let id = parts.next().unwrap_or(Value::Nil);
                if let Some(text) = id_text(&id) {
                    last_id = Some(text);
                }
                let consumer = parts.next().unwrap_or(Value::Nil);
                let idle = parts.next().unwrap_or(Value::Nil);
                let deliveries = parts.next().unwrap_or(Value::Nil);
                vec![cell(id), cell(consumer), cell(idle), cell(deliveries)]
            }
            other => vec![
                cell(other),
                CellValue::Null,
                CellValue::Null,
                CellValue::Null,
            ],
        })
        .collect();
    let next = if read > 0 && read >= count {
        last_id.map(|id| format!("({id}"))
    } else {
        None
    };
    with_trailer(
        &[
            ("id", "string"),
            ("consumer", "string"),
            ("idle_ms", "integer"),
            ("deliveries", "integer"),
        ],
        rows,
        next,
        pending_count(summary),
    )
}

fn two(replies: Vec<Value>) -> StudioResult<(Value, Value)> {
    let mut iter = replies.into_iter();
    match (iter.next(), iter.next()) {
        (Some(a), Some(b)) => Ok((a, b)),
        _ => Err(StudioError::QueryExecution(
            "the server answered with fewer replies than asked".into(),
        )),
    }
}

/// `SQLLY.XGROUPS`.
pub async fn groups(client: &RedisClient, key: &[u8]) -> StudioResult<Shaped> {
    let mut pipe = redis::pipe();
    pipe.cmd("XINFO").arg("GROUPS").arg(key);
    pipe.cmd("XLEN").arg(key);
    let (groups, len) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_groups(key, groups, &len))
}

/// `SQLLY.XPENDING`.
pub async fn pending(
    client: &RedisClient,
    key: &[u8],
    group: &[u8],
    from: &str,
    count: usize,
) -> StudioResult<Shaped> {
    let from = if from.is_empty() { "-" } else { from };
    let mut pipe = redis::pipe();
    pipe.cmd("XPENDING")
        .arg(key)
        .arg(group)
        .arg(from)
        .arg("+")
        .arg(count);
    pipe.cmd("XPENDING").arg(key).arg(group);
    let (range, summary) = two(client.run_pipeline(&pipe).await?)?;
    Ok(shape_pending(range, &summary, count))
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

    fn group_resp2(name: Value, lag: Value) -> Value {
        Value::Array(vec![
            bulk("name"),
            name,
            bulk("consumers"),
            Value::Int(2),
            bulk("pending"),
            Value::Int(1),
            bulk("last-delivered-id"),
            bulk("5-0"),
            bulk("entries-read"),
            Value::Int(5),
            bulk("lag"),
            lag,
        ])
    }

    #[test]
    fn streams_groups_project_resp2_and_resp3_onto_fixed_columns() {
        let resp2 = Value::Array(vec![
            group_resp2(bulk("wsg:g1"), Value::Int(3)),
            group_resp2(Value::BulkString(vec![0xff, b'g']), Value::Nil),
        ]);
        let shaped = shape_groups(b"wsg:x", resp2, &Value::Int(8));
        assert_eq!(
            names(&shaped),
            vec![
                "group",
                "consumers",
                "pending",
                "last_delivered_id",
                "entries_read",
                "lag",
                "stream_length"
            ]
        );
        assert_eq!(shaped.rows.len(), 2);
        assert_eq!(shaped.rows[0][0], CellValue::String("wsg:g1".into()));
        assert_eq!(shaped.rows[0][2], CellValue::I64(1));
        assert_eq!(shaped.rows[0][3], CellValue::String("5-0".into()));
        assert_eq!(shaped.rows[0][5], CellValue::I64(3));
        assert_eq!(shaped.rows[0][6], CellValue::I64(8));
        // A binary group name keeps its bytes; an unknown lag stays NULL.
        assert_eq!(shaped.rows[1][0], CellValue::Bytes(vec![0xff, b'g']));
        assert_eq!(shaped.rows[1][5], CellValue::Null);

        let resp3 = Value::Array(vec![Value::Map(vec![
            (bulk("name"), bulk("wsg:g1")),
            (bulk("consumers"), Value::Int(2)),
            (bulk("pending"), Value::Int(1)),
            (bulk("last-delivered-id"), bulk("5-0")),
            (bulk("entries-read"), Value::Int(5)),
            (bulk("lag"), Value::Int(3)),
        ])]);
        let from_map = shape_groups(b"wsg:x", resp3, &Value::Int(8));
        assert_eq!(from_map.rows[0], shaped.rows[0]);

        // Redis 6 has no entries-read / lag: those columns are NULL.
        let old = Value::Array(vec![Value::Array(vec![
            bulk("name"),
            bulk("g"),
            bulk("consumers"),
            Value::Int(0),
            bulk("pending"),
            Value::Int(0),
            bulk("last-delivered-id"),
            bulk("0-0"),
        ])]);
        let old = shape_groups(b"k", old, &Value::Int(0));
        assert_eq!(old.rows[0][4], CellValue::Null);
        assert_eq!(old.rows[0][5], CellValue::Null);

        let none = shape_groups(b"k", Value::Array(vec![]), &Value::Int(4));
        assert!(none.rows.is_empty());
        assert_eq!(none.columns.len(), 7);
    }

    #[test]
    fn streams_pending_rows_carry_idle_deliveries_and_the_trailer() {
        let entry = |id: &str, consumer: Value, idle: i64, n: i64| {
            Value::Array(vec![bulk(id), consumer, Value::Int(idle), Value::Int(n)])
        };
        let range = Value::Array(vec![
            entry("1-0", bulk("dead"), 93_000, 1),
            entry("2-0", Value::BulkString(vec![0xc3, 0x28]), 12, 4),
        ]);
        let summary = Value::Array(vec![
            Value::Int(7),
            bulk("1-0"),
            bulk("9-0"),
            Value::Array(vec![]),
        ]);
        let shaped = shape_pending(range.clone(), &summary, 2);
        assert_eq!(
            names(&shaped),
            vec!["id", "consumer", "idle_ms", "deliveries", "next", "total"]
        );
        assert_eq!(shaped.rows[0][0], CellValue::String("1-0".into()));
        assert_eq!(shaped.rows[0][1], CellValue::String("dead".into()));
        assert_eq!(shaped.rows[0][2], CellValue::I64(93_000));
        assert_eq!(shaped.rows[1][1], CellValue::Bytes(vec![0xc3, 0x28]));
        assert_eq!(shaped.rows[1][3], CellValue::I64(4));
        // A full page continues after its last id; the total is the summary.
        assert_eq!(shaped.rows[0][4], CellValue::String("(2-0".into()));
        assert_eq!(shaped.rows[0][5], CellValue::I64(7));
        let short = shape_pending(range, &summary, 10);
        assert_eq!(short.rows[0][4], CellValue::Null);
        // Nothing pending: one NULL row keeps the zero total.
        let empty = shape_pending(
            Value::Array(vec![]),
            &Value::Array(vec![Value::Int(0), Value::Nil, Value::Nil, Value::Nil]),
            10,
        );
        assert_eq!(empty.rows.len(), 1);
        assert_eq!(empty.rows[0][0], CellValue::Null);
        assert_eq!(empty.rows[0][5], CellValue::I64(0));
    }

    /// Live: a stream with two groups and one dead consumer. `SQLLY.XGROUPS`
    /// lists both groups with their pending counts and lag, `SQLLY.XPENDING`
    /// shows the dead consumer's entry with its idle milliseconds, and an
    /// `XAUTOCLAIM` run through the script path moves the entry to another
    /// consumer. Database 15, keys prefixed `wsg:`. Opt-in through
    /// `SQLLY_REDIS_URL`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn streams_live_groups_pending_and_autoclaim() {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let mut profile = super::super::profile_from_url("redis-streams", &url).expect("profile");
        profile.database = Some("15".to_string());
        let client = super::super::open_client(&profile).await.expect("client");
        let key = format!("wsg:stream:{}", std::process::id());
        let quoted = sqlly_intel::query::safety::redis::quote_argument(key.as_bytes());
        let mut seed = redis::pipe();
        seed.cmd("DEL").arg(&key);
        for i in 0..6 {
            seed.cmd("XADD")
                .arg(&key)
                .arg(format!("{}-0", i + 1))
                .arg("n")
                .arg(format!("{i:05}"));
        }
        seed.cmd("XGROUP")
            .arg("CREATE")
            .arg(&key)
            .arg("wsg:workers")
            .arg("0");
        seed.cmd("XGROUP")
            .arg("CREATE")
            .arg(&key)
            .arg("wsg:audit")
            .arg("$");
        // The dead consumer reads two entries and never acknowledges them.
        seed.cmd("XREADGROUP")
            .arg("GROUP")
            .arg("wsg:workers")
            .arg("dead")
            .arg("COUNT")
            .arg(2)
            .arg("STREAMS")
            .arg(&key)
            .arg(">");
        client.run_pipeline(&seed).await.expect("seed");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;

        let groups =
            super::super::query_rows_on_client(&client, &format!("SQLLY.XGROUPS {quoted}"))
                .await
                .expect("groups");
        let col = |set: &sqlly_core::MaterializedResultSet, name: &str| {
            set.columns.iter().position(|c| c.name == name).unwrap()
        };
        assert_eq!(groups.rows.len(), 2, "{:?}", groups.rows);
        let workers = groups
            .rows
            .iter()
            .find(|r| r[0] == CellValue::String("wsg:workers".into()))
            .expect("workers group");
        assert_eq!(workers[col(&groups, "pending")], CellValue::I64(2));
        assert_eq!(workers[col(&groups, "consumers")], CellValue::I64(1));
        assert_eq!(workers[col(&groups, "stream_length")], CellValue::I64(6));
        assert_eq!(workers[col(&groups, "lag")], CellValue::I64(4));

        let pending = super::super::query_rows_on_client(
            &client,
            &format!("SQLLY.XPENDING {quoted} wsg:workers"),
        )
        .await
        .expect("pending");
        assert_eq!(pending.rows.len(), 2);
        assert_eq!(pending.rows[0][0], CellValue::String("1-0".into()));
        assert_eq!(pending.rows[0][1], CellValue::String("dead".into()));
        match pending.rows[0][2] {
            CellValue::I64(idle) => assert!(idle >= 50, "idle {idle} ms"),
            ref other => panic!("{other:?}"),
        }
        assert_eq!(pending.rows[0][col(&pending, "total")], CellValue::I64(2));
        // Paging: one at a time continues after the first id.
        let first = pending_page(&client, &key, "-", 1).await;
        assert_eq!(first.rows[0][4], CellValue::String("(1-0".into()));
        let second = pending_page(&client, &key, "(1-0", 1).await;
        assert_eq!(second.rows[0][0], CellValue::String("2-0".into()));

        // XAUTOCLAIM through the script path moves both entries.
        let claimed = super::super::query_rows_on_client(
            &client,
            &format!("XAUTOCLAIM {quoted} wsg:workers alive 50 0-0 COUNT 10"),
        )
        .await
        .expect("autoclaim");
        assert!(!claimed.rows.is_empty());
        let after = super::super::query_rows_on_client(
            &client,
            &format!("SQLLY.XPENDING {quoted} wsg:workers"),
        )
        .await
        .expect("pending after");
        assert_eq!(after.rows.len(), 2);
        assert!(
            after
                .rows
                .iter()
                .all(|r| r[1] == CellValue::String("alive".into())),
            "{:?}",
            after.rows
        );
        // The audit group has nothing pending: one NULL row, total 0.
        let audit = super::super::query_rows_on_client(
            &client,
            &format!("SQLLY.XPENDING {quoted} wsg:audit"),
        )
        .await
        .expect("audit pending");
        assert_eq!(audit.rows.len(), 1);
        assert_eq!(audit.rows[0][0], CellValue::Null);

        let mut cleanup = redis::cmd("DEL");
        cleanup.arg(&key);
        client.run(&cleanup).await.expect("cleanup");
    }

    async fn pending_page(client: &RedisClient, key: &str, from: &str, count: usize) -> Shaped {
        pending(client, key.as_bytes(), b"wsg:workers", from, count)
            .await
            .expect("pending page")
    }
}
