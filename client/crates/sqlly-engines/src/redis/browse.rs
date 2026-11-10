//! Keyspace walks for the key browser and the storage tools: a bounded
//! `SCAN` (never `KEYS *`) with `TYPE`/`TTL` pipelined per page, the
//! logical-database roster from `CONFIG GET databases` + `INFO keyspace`,
//! and a bounded `MEMORY USAGE` sample per database.

use redis::Value;
use sqlly_core::{StudioError, StudioResult};

use super::RedisClient;
use super::info::{KeyspaceLine, info_map, parse_keyspace};

/// `SCAN` page size.
pub const SCAN_COUNT: usize = 1000;
/// Keys a single walk stops at when no smaller limit is given.
pub const DEFAULT_KEY_LIMIT: usize = 10_000;
/// Keys sampled per database for `MEMORY USAGE` when no size is given.
pub const DEFAULT_MEMORY_SAMPLE: usize = 200;

/// One key with its type and TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyInfo {
    pub key: Vec<u8>,
    /// `string`, `hash`, `list`, `set`, `zset`, `stream`, `ReJSON-RL`, …;
    /// `none` when the key vanished between the scan and the probe.
    pub kind: String,
    /// `None` when the key has no expiry (`TTL` = -1) or vanished (-2).
    pub ttl_seconds: Option<i64>,
}

impl KeyInfo {
    pub fn key_text(&self) -> String {
        String::from_utf8_lossy(&self.key).into_owned()
    }
}

/// A walk's outcome: the keys seen, whether the cursor came back to 0, and
/// the cursor to continue from when it did not (`"0"` once complete).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyListing {
    pub keys: Vec<KeyInfo>,
    pub complete: bool,
    /// The `SCAN` cursor a further walk continues from; `"0"` when the walk
    /// covered the whole keyspace.
    pub cursor: String,
}

/// Escape glob metacharacters so a literal prefix matches only itself.
pub fn escape_match_pattern(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len());
    for c in prefix.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub(crate) fn scan_page(value: Value) -> (String, Vec<Vec<u8>>) {
    let mut iter = match value {
        Value::Array(items) => items.into_iter(),
        _ => return ("0".to_string(), Vec::new()),
    };
    let cursor = match iter.next() {
        Some(Value::BulkString(b)) => String::from_utf8_lossy(&b).into_owned(),
        Some(Value::SimpleString(s)) => s,
        Some(Value::Int(n)) => n.to_string(),
        _ => "0".to_string(),
    };
    let keys = match iter.next() {
        Some(Value::Array(keys)) | Some(Value::Set(keys)) => keys
            .into_iter()
            .filter_map(|k| match k {
                Value::BulkString(b) => Some(b),
                Value::SimpleString(s) => Some(s.into_bytes()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    (cursor, keys)
}

/// Walk the current database with `SCAN … MATCH pattern COUNT 1000` until
/// the cursor returns to 0 or `limit` keys have been seen, then probe each
/// key's type and TTL in pipelined pages.
pub async fn scan_keys(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
) -> StudioResult<KeyListing> {
    scan_keys_from(client, pattern, limit, "0").await
}

/// [`scan_keys`] continued from an earlier walk's cursor: the next `limit`
/// keys after the page `start_cursor` names, so a listing that stopped at
/// its budget can be extended without rescanning what it already showed.
pub async fn scan_keys_from(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
    start_cursor: &str,
) -> StudioResult<KeyListing> {
    if client.is_cluster() {
        // Every master in turn, with a cursor that names the master.
        return super::cluster::scan_keys_across(client, pattern, limit, start_cursor).await;
    }
    let limit = limit.max(1);
    let mut cursor = normalize_cursor(start_cursor);
    let mut raw: Vec<Vec<u8>> = Vec::new();
    let mut complete = false;
    loop {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor.as_str());
        if !pattern.is_empty() && pattern != "*" {
            cmd.arg("MATCH").arg(pattern);
        }
        cmd.arg("COUNT")
            .arg(SCAN_COUNT.min(limit.saturating_sub(raw.len()).max(10)));
        let page = client.run(&cmd).await?;
        let (next, keys) = scan_page(page);
        raw.extend(keys);
        cursor = next;
        if cursor == "0" {
            complete = true;
            break;
        }
        if raw.len() >= limit {
            break;
        }
    }
    // The last page is kept whole rather than cut at `limit`: a `SCAN`
    // cursor names the next bucket, so dropping keys the server already
    // handed over would make a continuation from `cursor` skip them for
    // good. `COUNT` is sized to the remaining budget, so the overshoot is
    // at most one page's chained entries.
    let keys = probe_keys(client, raw).await?;
    Ok(KeyListing {
        keys,
        complete,
        cursor,
    })
}

/// A cursor as `SCAN` wants it: decimal digits, `"0"` for anything else
/// (an empty or malformed cursor starts a fresh walk rather than erroring).
pub fn normalize_cursor(cursor: &str) -> String {
    let cursor = cursor.trim();
    let digits = if cursor.bytes().all(|b| b.is_ascii_digit()) {
        cursor.trim_start_matches('0')
    } else {
        ""
    };
    if digits.is_empty() {
        "0".to_string()
    } else {
        digits.to_string()
    }
}

/// `TYPE` + `TTL` for every key, pipelined in pages of 1000.
pub async fn probe_keys(client: &RedisClient, keys: Vec<Vec<u8>>) -> StudioResult<Vec<KeyInfo>> {
    let mut out = Vec::with_capacity(keys.len());
    for page in keys.chunks(SCAN_COUNT) {
        let mut pipe = redis::pipe();
        for key in page {
            pipe.cmd("TYPE").arg(key.as_slice());
            pipe.cmd("TTL").arg(key.as_slice());
        }
        let replies: Vec<Value> = client.run_pipeline(&pipe).await?;
        let mut replies = replies.into_iter();
        for key in page {
            let kind = match replies.next() {
                Some(Value::SimpleString(s)) => s,
                Some(Value::BulkString(b)) => String::from_utf8_lossy(&b).into_owned(),
                Some(Value::Okay) => "OK".to_string(),
                _ => "none".to_string(),
            };
            let ttl = match replies.next() {
                Some(Value::Int(n)) if n >= 0 => Some(n),
                _ => None,
            };
            out.push(KeyInfo {
                key: key.clone(),
                kind,
                ttl_seconds: ttl,
            });
        }
    }
    Ok(out)
}

/// The number of logical databases (`CONFIG GET databases`, 16 when the
/// server refuses or does not know — cluster-mode servers answer 1).
pub async fn database_count(client: &RedisClient) -> usize {
    if client.is_cluster() {
        return 1;
    }
    let mut cmd = redis::cmd("CONFIG");
    cmd.arg("GET").arg("databases");
    match client.run(&cmd).await {
        Ok(Value::Array(items)) if items.len() >= 2 => match &items[1] {
            Value::BulkString(b) => String::from_utf8_lossy(b).parse().unwrap_or(16),
            Value::Int(n) => *n as usize,
            _ => 16,
        },
        Ok(Value::Map(pairs)) if !pairs.is_empty() => match &pairs[0].1 {
            Value::BulkString(b) => String::from_utf8_lossy(b).parse().unwrap_or(16),
            Value::Int(n) => *n as usize,
            _ => 16,
        },
        _ => 16,
    }
}

/// The logical databases and their keyspace lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Databases {
    /// One line per database (zeros when empty or when counts are unknown).
    pub lines: Vec<KeyspaceLine>,
    /// False when the server refused `INFO` (an ACL without it, as on
    /// ElastiCache with a narrow user): every database is still listed, but
    /// its key counts are not known.
    pub counts_known: bool,
}

/// Every logical database with its keyspace line (zeros when empty). A user
/// denied `INFO` still gets the list, with `counts_known` false.
pub async fn databases(client: &RedisClient) -> StudioResult<Databases> {
    if client.is_cluster() {
        // One logical database, its counts summed over the masters.
        let line = super::cluster::keyspace_across(client).await;
        return Ok(match line {
            Ok(line) => Databases {
                lines: vec![line],
                counts_known: true,
            },
            Err(StudioError::PermissionDenied(_)) => Databases {
                lines: vec![empty_line(0)],
                counts_known: false,
            },
            Err(error) => return Err(error),
        });
    }
    let count = database_count(client).await;
    let mut cmd = redis::cmd("INFO");
    cmd.arg("keyspace");
    let (text, counts_known) = match client.run(&cmd).await {
        Ok(Value::BulkString(b)) => (String::from_utf8_lossy(&b).into_owned(), true),
        Ok(Value::VerbatimString { text, .. } | Value::SimpleString(text)) => (text, true),
        Ok(_) => (String::new(), true),
        Err(StudioError::PermissionDenied(_)) => (String::new(), false),
        Err(error) => return Err(error),
    };
    let lines = parse_keyspace(&text);
    Ok(Databases {
        lines: (0..count as i64)
            .map(|db| {
                lines
                    .iter()
                    .find(|l| l.db == db)
                    .cloned()
                    .unwrap_or_else(|| empty_line(db))
            })
            .collect(),
        counts_known,
    })
}

fn empty_line(db: i64) -> KeyspaceLine {
    KeyspaceLine {
        db,
        keys: 0,
        expires: 0,
        avg_ttl_ms: 0,
    }
}

/// Memory sampled from one database: `MEMORY USAGE` over up to `sample`
/// scanned keys.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MemorySample {
    pub sampled_keys: i64,
    pub sampled_bytes: i64,
    pub largest_key: Option<String>,
    pub largest_bytes: i64,
    /// `MEMORY USAGE` is unsupported on this server.
    pub unsupported: bool,
}

/// Sample the CURRENT database's memory: scan up to `sample` keys, ask
/// `MEMORY USAGE key SAMPLES 5` for each in one pipeline.
pub async fn sample_memory(client: &RedisClient, sample: usize) -> StudioResult<MemorySample> {
    let listing = scan_keys_raw(client, "*", sample.max(1)).await?;
    let mut out = MemorySample::default();
    if listing.is_empty() {
        return Ok(out);
    }
    let mut pipe = redis::pipe();
    for key in &listing {
        pipe.cmd("MEMORY")
            .arg("USAGE")
            .arg(key.as_slice())
            .arg("SAMPLES")
            .arg(5);
    }
    let replies: Vec<Value> = match client.run_pipeline(&pipe).await {
        Ok(replies) => replies,
        // Only "cannot measure here" (unknown command, ACL NOPERM) means
        // unsupported; any other error reply is a real error.
        Err(error) if super::analyze::memory_unavailable_reason(&error).is_some() => {
            out.unsupported = true;
            return Ok(out);
        }
        Err(other) => return Err(other),
    };
    for (key, reply) in listing.iter().zip(replies) {
        if let Value::Int(bytes) = reply {
            out.sampled_keys += 1;
            out.sampled_bytes += bytes;
            if bytes > out.largest_bytes {
                out.largest_bytes = bytes;
                out.largest_key = Some(String::from_utf8_lossy(key).into_owned());
            }
        }
    }
    Ok(out)
}

/// Keys only (no type/TTL probe).
async fn scan_keys_raw(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
) -> StudioResult<Vec<Vec<u8>>> {
    if client.is_cluster() {
        return super::cluster::scan_raw_across(client, pattern, limit).await;
    }
    let mut cursor = "0".to_string();
    let mut raw: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor.as_str());
        if !pattern.is_empty() && pattern != "*" {
            cmd.arg("MATCH").arg(pattern);
        }
        cmd.arg("COUNT")
            .arg(SCAN_COUNT.min(limit.saturating_sub(raw.len()).max(10)));
        let (next, keys) = scan_page(client.run(&cmd).await?);
        raw.extend(keys);
        cursor = next;
        if cursor == "0" || raw.len() >= limit {
            break;
        }
    }
    raw.truncate(limit);
    Ok(raw)
}

/// `SELECT db` on the client's connection.
pub async fn select_database(client: &RedisClient, db: i64) -> StudioResult<()> {
    client.select_database(db).await
}

/// The whole `INFO` reply as a map.
pub async fn info(
    client: &RedisClient,
) -> StudioResult<std::collections::BTreeMap<String, String>> {
    let cmd = redis::cmd("INFO");
    // A cluster answers INFO per node: ask one master (the dashboard and
    // server-info rows describe a server, not a sum).
    let reply = match client.cluster_masters().await?.first() {
        Some(master) => client.run_on(master, &cmd).await?,
        None => client.run(&cmd).await?,
    };
    let text = match reply {
        Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
        Value::VerbatimString { text, .. } | Value::SimpleString(text) => text,
        other => {
            return Err(redis_error_text(format!(
                "INFO answered with an unexpected reply: {other:?}"
            )));
        }
    };
    Ok(info_map(&text))
}

fn redis_error_text(text: String) -> StudioError {
    StudioError::QueryExecution(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_glob_metacharacters_only() {
        assert_eq!(escape_match_pattern("user:"), "user:");
        assert_eq!(escape_match_pattern("a[b*?\\"), "a\\[b\\*\\?\\\\");
    }

    #[test]
    fn scan_page_reads_cursor_and_keys() {
        let (cursor, keys) = scan_page(Value::Array(vec![
            Value::BulkString(b"42".to_vec()),
            Value::Array(vec![Value::BulkString(b"a".to_vec()), Value::Int(1)]),
        ]));
        assert_eq!(cursor, "42");
        assert_eq!(keys, vec![b"a".to_vec()]);
        assert_eq!(scan_page(Value::Nil), ("0".to_string(), Vec::new()));
    }

    #[test]
    fn cursors_normalize_to_what_scan_accepts() {
        assert_eq!(normalize_cursor("0"), "0");
        assert_eq!(normalize_cursor(""), "0");
        assert_eq!(normalize_cursor("  "), "0");
        assert_eq!(normalize_cursor("42"), "42");
        assert_eq!(normalize_cursor(" 0042 "), "42");
        assert_eq!(normalize_cursor("abc"), "0");
        assert_eq!(normalize_cursor("-1"), "0");
    }
}
