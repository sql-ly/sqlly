//! `SQLLY.KEYINFO <key>` — the key view's header strip in one row: the
//! key's type, TTL, internal encoding, memory footprint and length.
//!
//! Composed from read-only probes only: `TYPE`, `TTL` and `OBJECT ENCODING`
//! pipelined, then `MEMORY USAGE … SAMPLES 5` (unmeasurable on servers or
//! users without it — `NULL`, never an error, the same rule the memory
//! analyzer applies) and the length command the type calls for (`STRLEN`,
//! `HLEN`, `LLEN`, `SCARD`, `ZCARD`, `XLEN`, `JSON.OBJLEN` falling back to
//! `JSON.ARRLEN`). A key that does not exist answers with type `none` and
//! empty columns rather than failing, so a view can say the key is gone.
//! The key travels as its exact bytes end to end.

use redis::Value;
use sqlly_core::{CellValue, StudioError, StudioResult};

use super::RedisClient;
use super::analyze::memory_unavailable_reason;
use super::shape::{Shaped, bulk_cell};

/// `MEMORY USAGE … SAMPLES n` depth (matches the memory analyzer).
const USAGE_SAMPLES: usize = 5;

/// The columns of the one-row answer.
pub const COLUMNS: [(&str, &str); 6] = [
    ("key", "string"),
    ("type", "string"),
    ("ttl_seconds", "integer"),
    ("encoding", "string"),
    ("memory_bytes", "integer"),
    ("length", "integer"),
];

/// What the probes found about one key.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KeyInfoRow {
    pub key: Vec<u8>,
    /// `string`, `hash`, …, `none` when the key does not exist.
    pub kind: String,
    /// `None` when the key never expires (or is gone).
    pub ttl_seconds: Option<i64>,
    pub encoding: Option<String>,
    /// `None` when the server or user cannot measure memory.
    pub memory_bytes: Option<i64>,
    /// `None` for types without a length probe (and for missing keys).
    pub length: Option<i64>,
}

impl KeyInfoRow {
    /// The row as the grid sees it: the key byte-exact, counts as integers.
    pub fn to_cells(&self) -> Vec<CellValue> {
        vec![
            bulk_cell(self.key.clone()),
            CellValue::String(self.kind.clone()),
            self.ttl_seconds
                .map(CellValue::I64)
                .unwrap_or(CellValue::Null),
            self.encoding
                .clone()
                .map(CellValue::String)
                .unwrap_or(CellValue::Null),
            self.memory_bytes
                .map(CellValue::I64)
                .unwrap_or(CellValue::Null),
            self.length.map(CellValue::I64).unwrap_or(CellValue::Null),
        ]
    }
}

/// The one-row result set for a probed key.
pub fn shape(row: &KeyInfoRow) -> Shaped {
    Shaped::new(&COLUMNS, vec![row.to_cells()])
}

/// The length probe for a type: the command words before the key, and the
/// fallback to try when the first answers with an error (`JSON.OBJLEN` on a
/// document whose root is an array).
pub fn length_commands(kind: &str) -> &'static [&'static [&'static str]] {
    match kind {
        "string" => &[&["STRLEN"]],
        "hash" => &[&["HLEN"]],
        "list" => &[&["LLEN"]],
        "set" => &[&["SCARD"]],
        "zset" => &[&["ZCARD"]],
        "stream" => &[&["XLEN"]],
        "ReJSON-RL" | "json" => &[&["JSON.OBJLEN"], &["JSON.ARRLEN"]],
        _ => &[],
    }
}

fn status_text(value: Option<Value>) -> Option<String> {
    match value {
        Some(Value::SimpleString(s)) => Some(s),
        Some(Value::BulkString(b)) => Some(String::from_utf8_lossy(&b).into_owned()),
        Some(Value::Okay) => Some("OK".to_string()),
        _ => None,
    }
}

/// Probe `key` on the client's current database.
pub async fn key_info(client: &RedisClient, key: &[u8]) -> StudioResult<KeyInfoRow> {
    let mut pipe = redis::pipe();
    pipe.cmd("TYPE").arg(key);
    pipe.cmd("TTL").arg(key);
    let replies = client.run_pipeline(&pipe).await?;
    let mut replies = replies.into_iter();
    let kind = status_text(replies.next()).unwrap_or_else(|| "none".to_string());
    let ttl_seconds = match replies.next() {
        Some(Value::Int(n)) if n >= 0 => Some(n),
        _ => None,
    };
    let mut row = KeyInfoRow {
        key: key.to_vec(),
        kind,
        ttl_seconds,
        ..Default::default()
    };
    if row.kind == "none" {
        return Ok(row);
    }
    // OBJECT ENCODING may be renamed or ACL-denied; the strip just leaves
    // the encoding blank then.
    let mut encoding = redis::cmd("OBJECT");
    encoding.arg("ENCODING").arg(key);
    row.encoding = match client.run(&encoding).await {
        Ok(value) => status_text(Some(value)),
        Err(StudioError::QueryCancelled) => return Err(StudioError::QueryCancelled),
        Err(_) => None,
    };
    let mut usage = redis::cmd("MEMORY");
    usage
        .arg("USAGE")
        .arg(key)
        .arg("SAMPLES")
        .arg(USAGE_SAMPLES);
    row.memory_bytes = match client.run(&usage).await {
        Ok(Value::Int(n)) => Some(n),
        Ok(_) => None,
        Err(error) => match memory_unavailable_reason(&error) {
            Some(_) => None,
            None => return Err(error),
        },
    };
    for words in length_commands(&row.kind) {
        let (name, rest) = words.split_first().expect("a length command has a name");
        let mut cmd = redis::cmd(name);
        for word in rest {
            cmd.arg(*word);
        }
        cmd.arg(key);
        match client.run(&cmd).await {
            Ok(Value::Int(n)) => {
                row.length = Some(n);
                break;
            }
            // JSON.OBJLEN on an array root answers nil or an error: try the
            // next probe; a cancel still cancels.
            Ok(_) => continue,
            Err(StudioError::QueryCancelled) => return Err(StudioError::QueryCancelled),
            Err(_) => continue,
        }
    }
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyinfo_row_keeps_the_key_bytes_and_nulls_the_unknowns() {
        let row = KeyInfoRow {
            key: vec![0x00, 0x30, 0x30, 0x30, 0x31, 0x34],
            kind: "string".into(),
            ttl_seconds: Some(42),
            encoding: Some("embstr".into()),
            memory_bytes: None,
            length: Some(6),
        };
        let shaped = shape(&row);
        let names: Vec<&str> = shaped.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "key",
                "type",
                "ttl_seconds",
                "encoding",
                "memory_bytes",
                "length"
            ]
        );
        assert_eq!(shaped.rows.len(), 1);
        // A NUL byte makes the key non-UTF-8? No — NUL is valid UTF-8, so
        // the cell is text with the NUL kept; the bytes are untouched.
        assert_eq!(
            shaped.rows[0][0],
            CellValue::String("\u{0}00014".to_string())
        );
        assert_eq!(shaped.rows[0][2], CellValue::I64(42));
        assert_eq!(shaped.rows[0][4], CellValue::Null);
        assert_eq!(shaped.rows[0][5], CellValue::I64(6));
        let binary = KeyInfoRow {
            key: vec![0xff, 0xfe],
            kind: "none".into(),
            ..Default::default()
        };
        let shaped = shape(&binary);
        assert_eq!(shaped.rows[0][0], CellValue::Bytes(vec![0xff, 0xfe]));
        assert_eq!(shaped.rows[0][1], CellValue::String("none".into()));
        assert!(shaped.rows[0][2..].iter().all(|c| *c == CellValue::Null));
    }

    #[test]
    fn keyinfo_length_probe_follows_the_type() {
        assert_eq!(length_commands("string"), &[&["STRLEN"][..]][..]);
        assert_eq!(length_commands("hash"), &[&["HLEN"][..]][..]);
        assert_eq!(length_commands("list"), &[&["LLEN"][..]][..]);
        assert_eq!(length_commands("set"), &[&["SCARD"][..]][..]);
        assert_eq!(length_commands("zset"), &[&["ZCARD"][..]][..]);
        assert_eq!(length_commands("stream"), &[&["XLEN"][..]][..]);
        assert_eq!(
            length_commands("ReJSON-RL"),
            &[&["JSON.OBJLEN"][..], &["JSON.ARRLEN"][..]][..]
        );
        assert!(length_commands("MBbloom--").is_empty());
        assert!(length_commands("none").is_empty());
    }

    #[test]
    fn keyinfo_memory_failure_is_null_only_for_noperm_and_unknown_command() {
        assert!(
            memory_unavailable_reason(&StudioError::PermissionDenied(
                "NOPERM this user has no permissions".into()
            ))
            .is_some()
        );
        assert!(
            memory_unavailable_reason(&StudioError::QueryExecution(
                "ERR unknown command 'MEMORY'".into()
            ))
            .is_some()
        );
        assert!(
            memory_unavailable_reason(&StudioError::Connection("reset by peer".into())).is_none()
        );
    }

    /// Live: one key of each type on database 5 (keys prefixed `wsa:`),
    /// then the same probes as a user whose ACL denies MEMORY — the row
    /// still comes back, with memory_bytes empty. Opt-in through
    /// `SQLLY_REDIS_URL`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keyinfo_live_probes_every_type_and_survives_noperm_memory() {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let mut profile = super::super::profile_from_url("redis-keyinfo", &url).expect("profile");
        profile.database = Some("5".to_string());
        let client = super::super::open_client(&profile).await.expect("client");
        let prefix = format!("wsa:keyinfo:{}", std::process::id());
        let keys: Vec<String> = ["s", "h", "l", "set", "z", "x", "ttl", "bin"]
            .iter()
            .map(|k| format!("{prefix}:{k}"))
            .collect();
        let mut seed = redis::pipe();
        seed.cmd("SET").arg(&keys[0]).arg("00014");
        seed.cmd("HSET")
            .arg(&keys[1])
            .arg("a")
            .arg("1")
            .arg("b")
            .arg("2");
        seed.cmd("RPUSH").arg(&keys[2]).arg("x").arg("y").arg("z");
        seed.cmd("SADD").arg(&keys[3]).arg("m");
        seed.cmd("ZADD")
            .arg(&keys[4])
            .arg("1.5")
            .arg("a")
            .arg("2")
            .arg("b");
        seed.cmd("XADD").arg(&keys[5]).arg("*").arg("f").arg("v");
        seed.cmd("SET").arg(&keys[6]).arg("t").arg("EX").arg(600);
        seed.cmd("SET").arg(&keys[7]).arg(&[0xffu8, 0x00, 0xfe][..]);
        client.run_pipeline(&seed).await.expect("seed");

        let expect = [
            ("string", Some(5)),
            ("hash", Some(2)),
            ("list", Some(3)),
            ("set", Some(1)),
            ("zset", Some(2)),
            ("stream", Some(1)),
            ("string", Some(1)),
            ("string", Some(3)),
        ];
        for (key, (kind, length)) in keys.iter().zip(expect) {
            let row = key_info(&client, key.as_bytes()).await.expect(key);
            assert_eq!(row.kind, kind, "{key}");
            assert_eq!(row.length, length, "{key}");
            assert!(row.encoding.is_some(), "{key}");
            assert!(row.memory_bytes.is_some_and(|b| b > 0), "{key}");
            assert_eq!(row.key, key.as_bytes());
        }
        let ttl = key_info(&client, keys[6].as_bytes()).await.unwrap();
        assert!(ttl.ttl_seconds.is_some_and(|t| t > 0 && t <= 600));
        let plain = key_info(&client, keys[0].as_bytes()).await.unwrap();
        assert_eq!(plain.ttl_seconds, None);
        // The value "00014" is never read here, but the row for the binary
        // key must still carry its exact bytes.
        let bin = key_info(&client, &[0xffu8, 0x00][..]).await.unwrap();
        assert_eq!(bin.kind, "none");
        assert_eq!(bin.key, vec![0xff, 0x00]);
        // Through the script path, as the view runs it.
        let text = format!(
            "SQLLY.KEYINFO {}",
            sqlly_intel::query::safety::redis::quote_argument(keys[1].as_bytes())
        );
        let set = super::super::query_rows_on_client(&client, &text)
            .await
            .expect("script");
        assert_eq!(set.columns.len(), 6);
        assert_eq!(set.rows[0][1], CellValue::String("hash".into()));
        assert_eq!(set.rows[0][5], CellValue::I64(2));

        // A user whose ACL denies MEMORY.
        let user = format!("wsa-nomem-{}", std::process::id());
        let mut setuser = redis::cmd("ACL");
        setuser
            .arg("SETUSER")
            .arg(&user)
            .arg("on")
            .arg(">wsa-test-pw")
            .arg("~*")
            .arg("+@all")
            .arg("-memory");
        if client.run(&setuser).await.is_err() {
            eprintln!("skipping the NOPERM half: the test user cannot run ACL SETUSER");
        } else {
            let mut limited = profile.clone();
            if let sqlly_core::AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } = &mut limited.authentication
            {
                *username = user.clone();
                *secret_ref = sqlly_core::SecretRef {
                    provider: sqlly_core::SecretProvider::InMemory,
                    key: "wsa-test-pw".to_string(),
                };
            }
            let limited_client = super::super::open_client(&limited)
                .await
                .expect("limited client");
            let row = key_info(&limited_client, keys[1].as_bytes())
                .await
                .expect("noperm row");
            assert_eq!(row.kind, "hash");
            assert_eq!(row.memory_bytes, None);
            assert_eq!(row.length, Some(2));
            let mut deluser = redis::cmd("ACL");
            deluser.arg("DELUSER").arg(&user);
            client.run(&deluser).await.expect("cleanup user");
        }

        let mut cleanup = redis::cmd("DEL");
        for key in &keys {
            cleanup.arg(key);
        }
        client.run(&cleanup).await.expect("cleanup");
    }
}
