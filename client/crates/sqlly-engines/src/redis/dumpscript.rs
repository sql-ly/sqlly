//! `SQLLY.DUMPSCRIPT pattern [limit]` — export the keys a glob matches as a
//! command file the key browser's Import Command File… runs back.
//!
//! One row per line, column `line`: a header comment, then for every key
//! the commands that rebuild it — `SET` for strings, `HSET` / `RPUSH` /
//! `SADD` / `ZADD` in chunks of [`ITEMS_PER_LINE`] for the collections,
//! `XADD` per stream entry with its original id, `JSON.SET key $ …` for
//! ReJSON documents — followed by `PEXPIRE key <ms>` when the key had a
//! time to live (relative: it counts from the import). Every argument goes
//! through the editor's `quote_argument`, so binary keys and values travel
//! as `"\xHH"` escapes and the import writes back the identical bytes.
//!
//! Read-only by construction (`SCAN`, `TYPE`, `PTTL`, `GET`, `HGETALL`,
//! `LRANGE`, `SMEMBERS`, `ZRANGE … WITHSCORES`, `XRANGE`, `JSON.GET`); a
//! `SQLLY.*` name is read-only by rule. The walk is bounded by the key
//! limit and by [`SCRIPT_BYTE_BUDGET`] — the import's own file cap — so an
//! export always fits back through the import; a capped or budget-stopped
//! export ends with a `# partial` line saying so. Types the import cannot
//! rebuild (module types other than ReJSON) are listed as `# skipped`
//! comments, never silently dropped. Stream consumer groups are not
//! exported (only entries).

use redis::Value;
use sqlly_core::{CellValue, StudioResult};
use sqlly_intel::query::safety::redis::quote_argument;

use super::RedisClient;
use super::browse;
use super::shape::Shaped;

/// Collection members one command line carries.
pub const ITEMS_PER_LINE: usize = 500;
/// Keys an export walks when the command names no limit.
pub const DEFAULT_KEY_LIMIT: usize = 10_000;
/// Largest script an export produces: the bulk import's file cap, so every
/// export can be imported again.
pub const SCRIPT_BYTE_BUDGET: usize = 8 * 1024 * 1024;

/// One stream entry: its id and field/value pairs.
pub type StreamEntry = (Vec<u8>, Vec<(Vec<u8>, Vec<u8>)>);

/// One key's value, as the export rebuilds it. Bytes throughout.
#[derive(Debug, Clone, PartialEq)]
pub enum KeyDump {
    String(Vec<u8>),
    Hash(Vec<(Vec<u8>, Vec<u8>)>),
    List(Vec<Vec<u8>>),
    Set(Vec<Vec<u8>>),
    /// `(score, member)`, the score as the server spelled it.
    ZSet(Vec<(Vec<u8>, Vec<u8>)>),
    /// `(id, field/value pairs)`.
    Stream(Vec<StreamEntry>),
    /// A ReJSON document's JSON text.
    Json(Vec<u8>),
    /// The key vanished between the scan and the read.
    Gone,
    /// A type the import cannot rebuild.
    Unsupported(String),
}

fn line(args: &[&[u8]]) -> String {
    args.iter()
        .map(|a| quote_argument(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn chunked<'a>(
    command: &'a [u8],
    key: &'a [u8],
    items: impl Iterator<Item = Vec<&'a [u8]>>,
    out: &mut Vec<String>,
) {
    let mut batch: Vec<&[u8]> = Vec::new();
    let mut count = 0;
    for item in items {
        batch.extend(item);
        count += 1;
        if count == ITEMS_PER_LINE {
            let mut args = vec![command, key];
            args.append(&mut batch);
            out.push(line(&args));
            count = 0;
        }
    }
    if count > 0 {
        let mut args = vec![command, key];
        args.append(&mut batch);
        out.push(line(&args));
    }
}

/// The command lines that rebuild `key` from `dump`, then its expiry.
pub fn dump_lines(key: &[u8], dump: &KeyDump, pttl_ms: Option<i64>) -> Vec<String> {
    let mut out = Vec::new();
    match dump {
        KeyDump::String(value) => out.push(line(&[b"SET", key, value])),
        KeyDump::Hash(pairs) => chunked(
            b"HSET",
            key,
            pairs.iter().map(|(f, v)| vec![f.as_slice(), v.as_slice()]),
            &mut out,
        ),
        KeyDump::List(items) => chunked(
            b"RPUSH",
            key,
            items.iter().map(|v| vec![v.as_slice()]),
            &mut out,
        ),
        KeyDump::Set(items) => chunked(
            b"SADD",
            key,
            items.iter().map(|v| vec![v.as_slice()]),
            &mut out,
        ),
        KeyDump::ZSet(items) => chunked(
            b"ZADD",
            key,
            items.iter().map(|(s, m)| vec![s.as_slice(), m.as_slice()]),
            &mut out,
        ),
        KeyDump::Stream(entries) => {
            for (id, fields) in entries {
                let mut args: Vec<&[u8]> = vec![b"XADD", key, id];
                for (f, v) in fields {
                    args.push(f);
                    args.push(v);
                }
                out.push(line(&args));
            }
        }
        KeyDump::Json(doc) => out.push(line(&[b"JSON.SET", key, b"$", doc])),
        KeyDump::Gone => {
            out.push(format!(
                "# skipped {}: deleted while exporting",
                quote_argument(key)
            ));
            return out;
        }
        KeyDump::Unsupported(kind) => {
            out.push(format!(
                "# skipped {} (type {kind}): the import cannot rebuild this type",
                quote_argument(key)
            ));
            return out;
        }
    }
    if let Some(ms) = pttl_ms.filter(|ms| *ms > 0) {
        out.push(line(&[b"PEXPIRE", key, ms.to_string().as_bytes()]));
    }
    out
}

/// A reply element as exact bytes (RESP3 doubles in their shortest
/// round-trip spelling).
fn bytes_of(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::BulkString(b) => Some(b.clone()),
        Value::SimpleString(s) => Some(s.clone().into_bytes()),
        Value::Int(n) => Some(n.to_string().into_bytes()),
        Value::Double(d) => Some(format_double(*d).into_bytes()),
        Value::VerbatimString { text, .. } => Some(text.clone().into_bytes()),
        _ => None,
    }
}

fn format_double(d: f64) -> String {
    if d.is_infinite() {
        if d > 0.0 { "inf" } else { "-inf" }.to_string()
    } else {
        format!("{d}")
    }
}

fn items(value: Value) -> Vec<Value> {
    match value {
        Value::Array(items) | Value::Set(items) => items,
        Value::Map(pairs) => pairs.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        _ => Vec::new(),
    }
}

fn byte_list(value: Value) -> Vec<Vec<u8>> {
    items(value).iter().filter_map(bytes_of).collect()
}

fn pairs(value: Value) -> Vec<(Vec<u8>, Vec<u8>)> {
    // RESP3 may nest each pair as a two-element array (ZRANGE WITHSCORES).
    let flat: Vec<Value> = items(value)
        .into_iter()
        .flat_map(|item| match item {
            Value::Array(inner) if inner.len() == 2 => inner,
            other => vec![other],
        })
        .collect();
    flat.chunks(2)
        .filter_map(|c| match c {
            [a, b] => Some((bytes_of(a)?, bytes_of(b)?)),
            _ => None,
        })
        .collect()
}

/// Turn the reader's reply for `kind` into a [`KeyDump`].
pub fn dump_from_reply(kind: &str, reply: Value) -> KeyDump {
    match kind {
        "string" => match &reply {
            Value::Nil => KeyDump::Gone,
            other => bytes_of(other).map_or(KeyDump::Gone, KeyDump::String),
        },
        "hash" => KeyDump::Hash(pairs(reply)),
        "list" => KeyDump::List(byte_list(reply)),
        "set" => KeyDump::Set(byte_list(reply)),
        // ZRANGE … WITHSCORES answers member, score; the import wants
        // score, member.
        "zset" => KeyDump::ZSet(pairs(reply).into_iter().map(|(m, s)| (s, m)).collect()),
        "stream" => KeyDump::Stream(
            items(reply)
                .into_iter()
                .filter_map(|entry| {
                    let mut parts = items(entry).into_iter();
                    let id = bytes_of(&parts.next()?)?;
                    let fields = parts.next().map(pairs).unwrap_or_default();
                    Some((id, fields))
                })
                .collect(),
        ),
        "ReJSON-RL" => match &reply {
            Value::Nil => KeyDump::Gone,
            other => bytes_of(other).map_or(KeyDump::Gone, KeyDump::Json),
        },
        other => KeyDump::Unsupported(other.to_string()),
    }
}

/// The read command a type is exported with, or `None` for types the
/// import cannot rebuild.
fn reader(kind: &str, key: &[u8]) -> Option<redis::Cmd> {
    let mut cmd = match kind {
        "string" => redis::cmd("GET"),
        "hash" => redis::cmd("HGETALL"),
        "list" => {
            let mut c = redis::cmd("LRANGE");
            c.arg(key).arg(0).arg(-1);
            return Some(c);
        }
        "set" => redis::cmd("SMEMBERS"),
        "zset" => {
            let mut c = redis::cmd("ZRANGE");
            c.arg(key).arg(0).arg(-1).arg("WITHSCORES");
            return Some(c);
        }
        "stream" => {
            let mut c = redis::cmd("XRANGE");
            c.arg(key).arg("-").arg("+");
            return Some(c);
        }
        "ReJSON-RL" => redis::cmd("JSON.GET"),
        _ => return None,
    };
    cmd.arg(key);
    Some(cmd)
}

fn text_of(value: &Value) -> String {
    bytes_of(value)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default()
}

/// Run the export: walk `pattern` (at most `limit` keys), read each key
/// and render its lines, stopping before the script outgrows
/// [`SCRIPT_BYTE_BUDGET`].
pub async fn dump_script(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
) -> StudioResult<Shaped> {
    let listing = browse::scan_keys(client, pattern, limit).await?;
    let mut lines = vec![format!(
        "# SQLLY Redis export: keys matching {} in database {}; run it with Import Command File…",
        quote_argument(pattern.as_bytes()),
        client.current_database()
    )];
    let mut bytes = lines[0].len() + 1;
    let mut exported = 0usize;
    let mut skipped = 0usize;
    let mut over_budget = false;
    for listed in &listing.keys {
        let key = listed.key.as_slice();
        let mut probe = redis::pipe();
        probe.cmd("TYPE").arg(key).cmd("PTTL").arg(key);
        let replies = client.run_pipeline(&probe).await?;
        let kind = replies.first().map(text_of).unwrap_or_default();
        let pttl = match replies.get(1) {
            Some(Value::Int(ms)) => Some(*ms),
            _ => None,
        };
        let dump = if kind == "none" {
            KeyDump::Gone
        } else {
            match reader(&kind, key) {
                Some(cmd) => dump_from_reply(&kind, client.run(&cmd).await?),
                None => KeyDump::Unsupported(kind.clone()),
            }
        };
        let rendered = dump_lines(key, &dump, pttl);
        let size: usize = rendered.iter().map(|l| l.len() + 1).sum();
        if bytes + size > SCRIPT_BYTE_BUDGET {
            over_budget = true;
            break;
        }
        bytes += size;
        if matches!(dump, KeyDump::Gone | KeyDump::Unsupported(_)) {
            skipped += 1;
        } else {
            exported += 1;
        }
        lines.extend(rendered);
    }
    if over_budget {
        lines.push(format!(
            "# partial: stopped after {exported} keys to stay within the {} MB import limit; export a narrower pattern for the rest",
            SCRIPT_BYTE_BUDGET / 1_048_576
        ));
    } else if !listing.complete {
        lines.push(format!(
            "# partial: the scan stopped at {limit} keys; {exported} exported. Export again with a higher limit or a narrower pattern for the rest"
        ));
    } else {
        lines.push(format!(
            "# complete: {exported} keys exported, {skipped} skipped"
        ));
    }
    let rows = lines
        .into_iter()
        .map(|l| vec![CellValue::String(l)])
        .collect();
    Ok(Shaped::new(&[("line", "string")], rows))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    fn bulk(b: &[u8]) -> Value {
        Value::BulkString(b.to_vec())
    }

    /// Parse the rendered lines back the way the import does and return
    /// the argument lists.
    fn reparse(lines: &[String]) -> Vec<Vec<Vec<u8>>> {
        parse_script(&lines.join("\n"))
            .unwrap()
            .into_iter()
            .map(|c| c.args)
            .collect()
    }

    #[test]
    fn dumpscript_lines_round_trip_binary_bytes() {
        let key = b"wsk:\x00bin\xff key".to_vec();
        let value = b"00014\x00\xfe\"\\\n".to_vec();
        let lines = dump_lines(&key, &KeyDump::String(value.clone()), Some(1500));
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].contains("\\x00") && lines[0].is_ascii(),
            "{}",
            lines[0]
        );
        let args = reparse(&lines);
        assert_eq!(args[0], vec![b"SET".to_vec(), key.clone(), value]);
        assert_eq!(args[1], vec![b"PEXPIRE".to_vec(), key, b"1500".to_vec()]);
    }

    #[test]
    fn dumpscript_collections_chunk_and_keep_order() {
        let items: Vec<Vec<u8>> = (0..1201).map(|i| format!("v{i}").into_bytes()).collect();
        let lines = dump_lines(b"l", &KeyDump::List(items.clone()), Some(-1));
        assert_eq!(lines.len(), 3, "500 + 500 + 201, no PEXPIRE for -1");
        let back: Vec<Vec<u8>> = reparse(&lines)
            .into_iter()
            .flat_map(|args| args.into_iter().skip(2))
            .collect();
        assert_eq!(back, items);

        let z = dump_from_reply(
            "zset",
            Value::Array(vec![bulk(b"m1"), bulk(b"1.5"), bulk(b"m2"), bulk(b"-inf")]),
        );
        assert_eq!(
            reparse(&dump_lines(b"z", &z, None))[0],
            vec![
                b"ZADD".to_vec(),
                b"z".to_vec(),
                b"1.5".to_vec(),
                b"m1".to_vec(),
                b"-inf".to_vec(),
                b"m2".to_vec()
            ]
        );
        // RESP3 nests each member/score pair and sends doubles.
        let z3 = dump_from_reply(
            "zset",
            Value::Array(vec![Value::Array(vec![bulk(b"m"), Value::Double(0.1)])]),
        );
        assert_eq!(z3, KeyDump::ZSet(vec![(b"0.1".to_vec(), b"m".to_vec())]));

        let h = dump_from_reply("hash", Value::Map(vec![(bulk(b"f"), bulk(b"\x01"))]));
        assert_eq!(
            reparse(&dump_lines(b"h", &h, None))[0],
            vec![
                b"HSET".to_vec(),
                b"h".to_vec(),
                b"f".to_vec(),
                b"\x01".to_vec()
            ]
        );
    }

    #[test]
    fn dumpscript_streams_json_and_unsupported() {
        let s = dump_from_reply(
            "stream",
            Value::Array(vec![Value::Array(vec![
                bulk(b"1-1"),
                Value::Array(vec![bulk(b"a"), bulk(b"1")]),
            ])]),
        );
        assert_eq!(
            reparse(&dump_lines(b"s", &s, None))[0],
            vec![
                b"XADD".to_vec(),
                b"s".to_vec(),
                b"1-1".to_vec(),
                b"a".to_vec(),
                b"1".to_vec()
            ]
        );
        let j = dump_from_reply("ReJSON-RL", bulk(br#"{"a":[1,2]}"#));
        assert_eq!(
            reparse(&dump_lines(b"j", &j, None))[0][3],
            br#"{"a":[1,2]}"#.to_vec()
        );
        let skipped = dump_lines(b"t", &KeyDump::Unsupported("TSDB-TYPE".into()), Some(5));
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].starts_with("# skipped"));
        assert_eq!(dump_from_reply("string", Value::Nil), KeyDump::Gone);
    }

    #[test]
    fn dumpscript_budget_matches_the_import_cap() {
        assert_eq!(SCRIPT_BYTE_BUDGET, 8 * 1024 * 1024);
    }
}
