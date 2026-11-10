//! RESP3 negotiation and reply parity.
//!
//! The query connection asks for RESP3 (`HELLO 3`, carrying the ACL
//! credentials) and falls back to RESP2 without a word when the server has no
//! `HELLO` (Redis before 6.0, some proxies), refuses protocol 3 (`NOPROTO`),
//! or an ACL rule forbids the command. Which protocol the connection speaks is
//! recorded on the client and reported by `SQLLY.SERVERINFO`.
//!
//! RESP3 gives some replies a different wire form — `HGETALL` and
//! `CONFIG GET` answer with maps, `ZRANGE … WITHSCORES` with `[member,
//! score]` pairs, scores as doubles, `INFO` as verbatim text. A result grid
//! must not change because the server is newer, so [`resp2_form`] folds the
//! core commands' RESP3 replies back into their RESP2 shape before
//! [`super::shape::shape_reply`] sees them. Module commands (`FT.*`, `TS.*`,
//! `JSON.*`, …) are left alone: their shapers read both forms themselves,
//! and their RESP3 replies are structurally different, not just retyped.

use redis::{ProtocolVersion, Value};

/// True when a connection attempt failed only because the server will not
/// speak RESP3: no `HELLO` command, `NOPROTO`, or an ACL that refuses
/// `HELLO`. Anything else (a wrong password, a refused socket) is a real
/// failure the RESP2 retry would only repeat.
pub(crate) fn hello_refused(error: &redis::RedisError) -> bool {
    if error.kind() == redis::ErrorKind::RESP3NotSupported {
        return true;
    }
    let detail = error.detail().unwrap_or_default().to_ascii_lowercase();
    match error.code() {
        Some("NOPROTO") => true,
        Some("NOPERM") => detail.contains("hello"),
        Some("ERR") => detail.contains("unknown command") && detail.contains("hello"),
        _ => {
            let text = error.to_string().to_ascii_lowercase();
            text.contains("unknown command") && text.contains("hello")
        }
    }
}

/// The protocol as `SQLLY.SERVERINFO` and the test log spell it.
pub fn protocol_label(protocol: ProtocolVersion) -> &'static str {
    if protocol.supports_resp3() {
        "RESP3"
    } else {
        "RESP2"
    }
}

/// Commands whose RESP2 reply is a flat `[field, value, …]` array and whose
/// RESP3 reply is a map.
fn map_reply(name: &str, sub: &str, args: &[Vec<u8>]) -> bool {
    match name {
        "HGETALL" | "HELLO" => true,
        "CONFIG" => sub == "GET",
        "MEMORY" => sub == "STATS",
        "HRANDFIELD" => has_flag(args, "WITHVALUES"),
        _ => false,
    }
}

/// Commands whose RESP2 reply is a flat `[member, score, …]` array and whose
/// RESP3 reply is a list of `[member, score]` pairs.
fn pair_list_reply(name: &str, args: &[Vec<u8>]) -> bool {
    match name {
        "ZRANGE" | "ZRANGEBYSCORE" | "ZREVRANGE" | "ZREVRANGEBYSCORE" | "ZRANDMEMBER"
        | "ZUNION" | "ZINTER" | "ZDIFF" => has_flag(args, "WITHSCORES"),
        "ZPOPMIN" | "ZPOPMAX" => true,
        "HRANDFIELD" => has_flag(args, "WITHVALUES"),
        _ => false,
    }
}

fn has_flag(args: &[Vec<u8>], flag: &str) -> bool {
    args.iter()
        .skip(1)
        .any(|a| a.eq_ignore_ascii_case(flag.as_bytes()))
}

fn upper(args: &[Vec<u8>], index: usize) -> String {
    args.get(index)
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

/// A double as Redis prints it in a RESP2 bulk string: whole numbers without
/// a fraction, infinities as `inf` / `-inf`, everything else in the shortest
/// form that reads back to the same number.
pub(crate) fn double_text(d: f64) -> String {
    if d.is_infinite() {
        if d > 0.0 { "inf" } else { "-inf" }.to_string()
    } else if d.is_nan() {
        "nan".to_string()
    } else if d.fract() == 0.0 && d.abs() < 1e17 {
        format!("{}", d as i64)
    } else {
        format!("{d}")
    }
}

/// RESP3-only scalar types as their RESP2 spelling: doubles and big numbers
/// as bulk strings, booleans as 0 / 1, verbatim text as a bulk string, sets
/// as arrays. Containers are converted element by element.
fn scalars_as_resp2(value: Value) -> Value {
    match value {
        Value::Double(d) => Value::BulkString(double_text(d).into_bytes()),
        Value::Boolean(b) => Value::Int(i64::from(b)),
        Value::BigNumber(digits) => Value::BulkString(digits),
        Value::VerbatimString { text, .. } => Value::BulkString(text.into_bytes()),
        Value::Attribute { data, .. } => scalars_as_resp2(*data),
        Value::Set(items) | Value::Array(items) => {
            Value::Array(items.into_iter().map(scalars_as_resp2).collect())
        }
        Value::Map(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| (scalars_as_resp2(k), scalars_as_resp2(v)))
                .collect(),
        ),
        other => other,
    }
}

/// Fold a core command's RESP3 reply into the form the same command answers
/// with under RESP2, so the shaped result set is identical on both
/// protocols. A RESP2 reply passes through unchanged.
pub fn resp2_form(args: &[Vec<u8>], value: Value) -> Value {
    let name = upper(args, 0);
    if name.contains('.') {
        return value;
    }
    let sub = upper(args, 1);
    let value = match value {
        Value::Map(entries) if map_reply(&name, &sub, args) => {
            Value::Array(entries.into_iter().flat_map(|(k, v)| [k, v]).collect())
        }
        Value::Array(items)
            if pair_list_reply(&name, args)
                && !items.is_empty()
                && items
                    .iter()
                    .all(|item| matches!(item, Value::Array(pair) if pair.len() == 2)) =>
        {
            Value::Array(
                items
                    .into_iter()
                    .flat_map(|item| match item {
                        Value::Array(pair) => pair,
                        other => vec![other],
                    })
                    .collect(),
            )
        }
        other => other,
    };
    scalars_as_resp2(value)
}

#[cfg(test)]
mod tests {
    use super::super::shape::shape_reply;
    use super::*;

    fn args(line: &str) -> Vec<Vec<u8>> {
        line.split_whitespace()
            .map(|a| a.as_bytes().to_vec())
            .collect()
    }

    fn bulk(text: &str) -> Value {
        Value::BulkString(text.as_bytes().to_vec())
    }

    /// Shape a reply the way the driver does: parity fold, then shaping.
    fn shaped(line: &str, value: Value) -> super::super::shape::Shaped {
        let args = args(line);
        shape_reply(&args, resp2_form(&args, value))
    }

    #[test]
    fn hgetall_shapes_identically_under_resp2_and_resp3() {
        let resp2 = Value::Array(vec![bulk("name"), bulk("ada"), bulk("zip"), bulk("00014")]);
        let resp3 = Value::Map(vec![
            (bulk("name"), bulk("ada")),
            (bulk("zip"), bulk("00014")),
        ]);
        let a = shaped("HGETALL user:1", resp2);
        let b = shaped("HGETALL user:1", resp3);
        assert_eq!(a, b);
        assert_eq!(a.columns[0].name, "field");
        assert_eq!(a.rows[1][1], sqlly_core::CellValue::String("00014".into()));
    }

    #[test]
    fn config_get_shapes_identically_under_resp2_and_resp3() {
        let resp2 = Value::Array(vec![
            bulk("maxmemory"),
            bulk("0"),
            bulk("databases"),
            bulk("16"),
        ]);
        let resp3 = Value::Map(vec![
            (bulk("maxmemory"), bulk("0")),
            (bulk("databases"), bulk("16")),
        ]);
        assert_eq!(shaped("CONFIG GET *", resp2), shaped("config get *", resp3));
    }

    #[test]
    fn scored_pairs_and_scalar_scores_match_resp2_text() {
        let resp2 = Value::Array(vec![bulk("a"), bulk("1.5"), bulk("b"), bulk("2")]);
        let resp3 = Value::Array(vec![
            Value::Array(vec![bulk("a"), Value::Double(1.5)]),
            Value::Array(vec![bulk("b"), Value::Double(2.0)]),
        ]);
        assert_eq!(
            shaped("ZRANGE z 0 -1 WITHSCORES", resp2),
            shaped("ZRANGE z 0 -1 WITHSCORES", resp3)
        );
        assert_eq!(
            shaped("ZSCORE z a", bulk("1.5")),
            shaped("ZSCORE z a", Value::Double(1.5))
        );
        assert_eq!(
            shaped("ZSCORE z a", bulk("inf")),
            shaped("ZSCORE z a", Value::Double(f64::INFINITY))
        );
        let resp2 = Value::Array(vec![bulk("f"), bulk("v")]);
        let resp3 = Value::Array(vec![Value::Array(vec![bulk("f"), bulk("v")])]);
        assert_eq!(
            shaped("HRANDFIELD h 1 WITHVALUES", resp2),
            shaped("HRANDFIELD h 1 WITHVALUES", resp3)
        );
    }

    #[test]
    fn verbatim_info_and_sets_match_resp2() {
        let text = "# Server\r\nredis_version:7.4.0\r\n";
        assert_eq!(
            shaped("INFO server", bulk(text)),
            shaped(
                "INFO server",
                Value::VerbatimString {
                    format: redis::VerbatimFormat::Text,
                    text: text.to_string()
                }
            )
        );
        assert_eq!(
            shaped("SMEMBERS s", Value::Array(vec![bulk("x")])),
            shaped("SMEMBERS s", Value::Set(vec![bulk("x")]))
        );
    }

    #[test]
    fn module_replies_and_resp2_replies_pass_through() {
        let map = Value::Map(vec![(bulk("k"), Value::Double(1.0))]);
        assert_eq!(resp2_form(&args("FT.INFO idx"), map.clone()), map);
        let flat = Value::Array(vec![bulk("a"), bulk("1")]);
        assert_eq!(resp2_form(&args("HGETALL h"), flat.clone()), flat);
        // A plain list of two-element arrays from a command that is not a
        // scored read keeps its nesting.
        let nested = Value::Array(vec![Value::Array(vec![bulk("a"), bulk("b")])]);
        assert_eq!(resp2_form(&args("LRANGE l 0 -1"), nested.clone()), nested);
    }

    #[test]
    fn hello_refusals_are_told_apart_from_real_failures() {
        let unknown: redis::RedisError = (
            redis::ErrorKind::RESP3NotSupported,
            "Redis Server doesn't support HELLO command therefore resp3 cannot be used",
        )
            .into();
        assert!(hello_refused(&unknown));
        let wrongpass = redis::RedisError::from((
            redis::ErrorKind::AuthenticationFailed,
            "WRONGPASS invalid username-password pair",
        ));
        assert!(!hello_refused(&wrongpass));
        let io = redis::RedisError::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        ));
        assert!(!hello_refused(&io));
        assert_eq!(protocol_label(ProtocolVersion::RESP3), "RESP3");
        assert_eq!(protocol_label(ProtocolVersion::RESP2), "RESP2");
    }

    /// Live: the query connection negotiates RESP3 on Redis 7, a forced
    /// RESP2 connection reads the same keys, and HGETALL / CONFIG GET /
    /// ZRANGE WITHSCORES / ZSCORE shape identically on both. SERVERINFO
    /// reports the protocol. Opt-in through `SQLLY_REDIS_URL`; works in
    /// logical database 15 on `wsj:` keys (10 is the legacy suite's empty database).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_resp3_and_resp2_shape_identically() {
        let Some(mut profile) = super::super::live_profile_from_env("wsj-hello") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.database = Some("15".into());
        let resp3 = super::super::open_client(&profile).await.expect("resp3");
        assert_eq!(resp3.protocol(), ProtocolVersion::RESP3);
        let (host, port) = crate::ssh::TunnelManager::global()
            .endpoint_for(&profile)
            .await
            .unwrap();
        let resp2 = match super::super::connect(&profile, &host, port, ProtocolVersion::RESP2).await
        {
            Ok(client) => client,
            Err(refusal) => panic!("resp2: {}", refusal.into_error()),
        };
        assert_eq!(resp2.protocol(), ProtocolVersion::RESP2);
        let setup = "DEL wsj:h wsj:z\nHSET wsj:h name ada zip 00014\nZADD wsj:z 1.5 a 2 b";
        super::super::query_rows_on_client(&resp3, setup)
            .await
            .unwrap();
        for line in [
            "HGETALL wsj:h",
            "CONFIG GET databases",
            "ZRANGE wsj:z 0 -1 WITHSCORES",
            "ZSCORE wsj:z a",
            "SMEMBERS wsj:none",
        ] {
            let a = super::super::query_rows_on_client(&resp3, line)
                .await
                .unwrap();
            let b = super::super::query_rows_on_client(&resp2, line)
                .await
                .unwrap();
            assert_eq!(a.columns, b.columns, "{line}");
            assert_eq!(a.rows, b.rows, "{line}");
        }
        let info = super::super::query_rows_on_client(&resp3, "SQLLY.SERVERINFO")
            .await
            .unwrap();
        let protocol = info.columns.iter().position(|c| c.name == "protocol");
        assert_eq!(
            info.rows[0][protocol.expect("protocol column")],
            sqlly_core::CellValue::String("RESP3".into())
        );
        super::super::query_rows_on_client(&resp3, "DEL wsj:h wsj:z")
            .await
            .unwrap();
    }

    #[test]
    fn double_text_matches_redis_spelling() {
        assert_eq!(double_text(2.0), "2");
        assert_eq!(double_text(-3.0), "-3");
        assert_eq!(double_text(1.5), "1.5");
        assert_eq!(double_text(0.1), "0.1");
        assert_eq!(double_text(f64::NEG_INFINITY), "-inf");
    }
}
