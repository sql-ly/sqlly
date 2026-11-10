//! Live round trips for the key browser's Rename…, Copy to database…, Copy
//! to connection… and Export folder as command file: the commands the
//! dialogs send, run through the driver exactly as the dialogs run them.
//!
//! Opt-in: set `SQLLY_REDIS_URL` (for example `redis://127.0.0.1:6380/0`);
//! every test self-skips without it. The suite works in logical databases
//! 11 and 12 under the `wsk:` prefix and removes exactly the keys it
//! creates (it never flushes a database).

// The timed async helpers below nest deeply enough to overflow rustc's
// default query depth when computing their layout.
#![recursion_limit = "256"]
#![cfg(not(target_arch = "wasm32"))]

use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_engines::redis::{RedisClient, live_profile_from_env, open_client, query_rows_on_client};
use sqlly_intel::query::safety::redis::quote_argument;

fn profile(db: &str, tag: &str) -> Option<ConnectionProfile> {
    let mut profile = live_profile_from_env(tag)?;
    profile.database = Some(db.into());
    Some(profile)
}

/// Run one command (arguments quoted the way the dialogs quote them) and
/// return its result rows.
async fn q(client: &RedisClient, args: &[&[u8]]) -> Vec<Vec<CellValue>> {
    let line = args
        .iter()
        .map(|a| quote_argument(a))
        .collect::<Vec<_>>()
        .join(" ");
    query_rows_on_client(client, &line)
        .await
        .unwrap_or_else(|e| panic!("{line}: {e}"))
        .rows
}

async fn run(client: &RedisClient, args: &[&[u8]]) {
    q(client, args).await;
}

async fn unlink_matching(client: &RedisClient, pattern: &str) {
    let rows = query_rows_on_client(client, &format!("SQLLY.KEYS {pattern} 100000"))
        .await
        .expect("scan");
    let mut args = vec![b"UNLINK".to_vec()];
    for row in rows.rows {
        args.push(bytes(&row[0]));
    }
    if args.len() > 1 {
        client.run_args(&args).await.expect("unlink");
    }
}

fn bytes(cell: &CellValue) -> Vec<u8> {
    match cell {
        CellValue::String(s) => s.clone().into_bytes(),
        CellValue::Bytes(b) => b.clone(),
        CellValue::I64(n) => n.to_string().into_bytes(),
        CellValue::F64(f) => f.to_string().into_bytes(),
        CellValue::Null => Vec::new(),
        other => format!("{other:?}").into_bytes(),
    }
}

/// Every cell of every row, as bytes (row-major).
fn flat(rows: &[Vec<CellValue>]) -> Vec<Vec<u8>> {
    rows.iter().flat_map(|r| r.iter().map(bytes)).collect()
}

async fn int(client: &RedisClient, args: &[&[u8]]) -> i64 {
    match q(client, args).await[0][0] {
        CellValue::I64(n) => n,
        ref other => panic!("not an int: {other:?}"),
    }
}

/// Everything the round trip compares for a key: type, value, has-TTL.
async fn snapshot(client: &RedisClient, key: &[u8]) -> (String, Vec<Vec<u8>>, bool) {
    let kind = String::from_utf8(bytes(&q(client, &[b"TYPE", key]).await[0][0])).unwrap();
    let mut rows = match kind.as_str() {
        "string" => q(client, &[b"GET", key]).await,
        "hash" => q(client, &[b"HGETALL", key]).await,
        "list" => q(client, &[b"LRANGE", key, b"0", b"-1"]).await,
        "set" => q(client, &[b"SMEMBERS", key]).await,
        "zset" => q(client, &[b"ZRANGE", key, b"0", b"-1", b"WITHSCORES"]).await,
        "stream" => q(client, &[b"XRANGE", key, b"-", b"+"]).await,
        other => panic!("unexpected type {other}"),
    };
    if matches!(kind.as_str(), "hash" | "set") {
        rows.sort_by_key(|r| r.iter().map(bytes).collect::<Vec<_>>());
    }
    let ttl = int(client, &[b"PTTL", key]).await > 0;
    (kind, flat(&rows), ttl)
}

#[tokio::test]
async fn live_export_folder_reimports_byte_exact() {
    let Some(profile) = profile("11", "wsk-export") else {
        eprintln!("SQLLY_REDIS_URL not set; skipping");
        return;
    };
    let client = open_client(&profile).await.expect("open");
    unlink_matching(&client, "wsk:exp:*").await;

    let binary_key: &[u8] = b"wsk:exp:bin\x00\xff";
    let big_list: Vec<Vec<u8>> = (0..1203).map(|i| format!("{i:05}").into_bytes()).collect();
    run(&client, &[b"SET", binary_key, b"00014\x00\xfe\xff\"\\\r\n"]).await;
    run(&client, &[b"SET", b"wsk:exp:empty", b""]).await;
    run(&client, &[b"PEXPIRE", b"wsk:exp:empty", b"600000"]).await;
    run(
        &client,
        &[b"HSET", b"wsk:exp:h", b"f\x01", b"\x80v", b"n", b"007"],
    )
    .await;
    let mut rpush: Vec<&[u8]> = vec![b"RPUSH", b"wsk:exp:l"];
    rpush.extend(big_list.iter().map(Vec::as_slice));
    run(&client, &rpush).await;
    run(&client, &[b"SADD", b"wsk:exp:s", b"a", b"\xff", b""]).await;
    run(
        &client,
        &[
            b"ZADD",
            b"wsk:exp:z",
            b"1.5",
            b"m1",
            b"-inf",
            b"m2",
            b"0.1",
            b"m3",
        ],
    )
    .await;
    run(&client, &[b"XADD", b"wsk:exp:x", b"5-1", b"k", b"\x00"]).await;
    run(
        &client,
        &[b"XADD", b"wsk:exp:x", b"6-0", b"a", b"1", b"b", b"2"],
    )
    .await;

    let keys: Vec<&[u8]> = vec![
        binary_key,
        b"wsk:exp:empty",
        b"wsk:exp:h",
        b"wsk:exp:l",
        b"wsk:exp:s",
        b"wsk:exp:z",
        b"wsk:exp:x",
    ];
    let mut before = Vec::new();
    for key in &keys {
        before.push(snapshot(&client, key).await);
    }

    let script = query_rows_on_client(&client, "SQLLY.DUMPSCRIPT wsk:exp:* 1000")
        .await
        .expect("dumpscript");
    let lines: Vec<String> = script
        .rows
        .iter()
        .map(|r| match &r[0] {
            CellValue::String(s) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(
        lines.last().unwrap().starts_with("# complete: 7 keys"),
        "{lines:?}"
    );
    // Binary bytes travel as escapes: every command line is plain ASCII.
    assert!(
        lines
            .iter()
            .filter(|l| !l.starts_with('#'))
            .all(|l| l.is_ascii())
    );
    let text = lines.join("\n");

    let dbsize_before = int(&client, &[b"DBSIZE"]).await;
    unlink_matching(&client, "wsk:exp:*").await;
    assert_eq!(int(&client, &[b"DBSIZE"]).await, dbsize_before - 7);
    // The import runs the file's lines on the database, as the dialog does.
    query_rows_on_client(&client, &text).await.expect("import");
    assert_eq!(int(&client, &[b"DBSIZE"]).await, dbsize_before);
    for (key, expected) in keys.iter().zip(&before) {
        assert_eq!(&snapshot(&client, key).await, expected, "{key:?}");
    }
    unlink_matching(&client, "wsk:exp:*").await;
}

#[tokio::test]
async fn live_rename_copy_and_dump_restore_across_connections() {
    let (Some(source), Some(target)) = (profile("11", "wsk-src"), profile("12", "wsk-dst")) else {
        eprintln!("SQLLY_REDIS_URL not set; skipping");
        return;
    };
    let a = open_client(&source).await.expect("open 11");
    let b = open_client(&target).await.expect("open 12");
    let names: [&[u8]; 3] = [b"wsk:mv:a", b"wsk:mv:taken", b"wsk:mv:bin\xff"];
    for key in names {
        run(&a, &[b"UNLINK", key]).await;
        run(&b, &[b"UNLINK", key]).await;
    }
    run(&a, &[b"HSET", b"wsk:mv:a", b"f", b"00014"]).await;
    run(&a, &[b"SET", b"wsk:mv:taken", b"keep"]).await;

    // Rename with NX refuses an existing target (reply 0) and keeps both.
    assert_eq!(
        int(&a, &[b"RENAMENX", b"wsk:mv:a", b"wsk:mv:taken"]).await,
        0
    );
    assert_eq!(
        int(&a, &[b"RENAMENX", b"wsk:mv:a", b"wsk:mv:bin\xff"]).await,
        1
    );
    assert_eq!(int(&a, &[b"EXISTS", b"wsk:mv:bin\xff"]).await, 1);
    assert_eq!(int(&a, &[b"EXISTS", b"wsk:mv:a"]).await, 0);

    // Copy to database 12, then again without REPLACE (refused: 0).
    let copy: [&[u8]; 5] = [b"COPY", b"wsk:mv:bin\xff", b"wsk:mv:bin\xff", b"DB", b"12"];
    assert_eq!(int(&a, &copy).await, 1);
    assert_eq!(int(&a, &copy).await, 0);
    assert_eq!(
        flat(&q(&b, &[b"HGETALL", b"wsk:mv:bin\xff"]).await),
        vec![b"f".to_vec(), b"00014".to_vec()]
    );

    // Copy to connection: DUMP + PTTL on the source, RESTORE … REPLACE on
    // the target connection, as the dialog does.
    run(&a, &[b"PEXPIRE", b"wsk:mv:taken", b"900000"]).await;
    let payload = bytes(&q(&a, &[b"DUMP", b"wsk:mv:taken"]).await[0][0]);
    let ttl = int(&a, &[b"PTTL", b"wsk:mv:taken"]).await;
    assert!(ttl > 0);
    run(&b, &[b"SET", b"wsk:mv:taken", b"old"]).await;
    let ttl = ttl.to_string();
    run(
        &b,
        &[
            b"RESTORE",
            b"wsk:mv:taken",
            ttl.as_bytes(),
            &payload,
            b"REPLACE",
        ],
    )
    .await;
    assert_eq!(
        bytes(&q(&b, &[b"GET", b"wsk:mv:taken"]).await[0][0]),
        b"keep"
    );
    assert!(int(&b, &[b"PTTL", b"wsk:mv:taken"]).await > 0);

    for key in names {
        run(&a, &[b"UNLINK", key]).await;
        run(&b, &[b"UNLINK", key]).await;
    }
}
