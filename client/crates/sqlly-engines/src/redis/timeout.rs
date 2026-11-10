//! The query connection's per-command response timeout.
//!
//! A command that gets no reply within the connection's command timeout
//! fails with an error naming the command and the limit, instead of leaving
//! the tab waiting forever on a stuck server or a half-dead network path.
//! The limit is the profile's query timeout: unset means 30 seconds, `0`
//! means no limit. Commands that block by design for a time the user wrote
//! (`BLPOP key 20`, `XREAD BLOCK 5000 …`, `WAIT 1 3000`) get that time on top,
//! so they are never cut short. The live tools' dedicated connections
//! (Profiler, Pub/Sub) wait without a limit — silence is normal there.
//!
//! A timed-out command is not retried: it may still run on the server, so
//! the error says so and the user decides whether to run it again.

use std::time::Duration;

use sqlly_core::{ConnectionProfile, StudioError};

/// The command timeout when the profile sets none.
pub const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// The response timeout for the profile's query connection: the profile's
/// query timeout, [`DEFAULT_RESPONSE_TIMEOUT`] when unset, `None` for `0`.
pub fn response_timeout(profile: &ConnectionProfile) -> Option<Duration> {
    match profile.statement_timeout_seconds {
        None => Some(DEFAULT_RESPONSE_TIMEOUT),
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds)),
    }
}

fn upper(args: &[Vec<u8>], index: usize) -> String {
    args.get(index)
        .map(|a| String::from_utf8_lossy(a).to_ascii_uppercase())
        .unwrap_or_default()
}

fn number(args: &[Vec<u8>], index: usize) -> Option<f64> {
    std::str::from_utf8(args.get(index)?)
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite() && *n >= 0.0)
}

/// How long a blocking command asks the server to wait, from its own
/// arguments: seconds for the list / sorted-set pops, milliseconds after
/// `BLOCK` for the stream reads and for `WAIT` / `WAITAOF`. `None` when the
/// command does not block (or blocks forever, which the safety layer refuses
/// before it is sent).
pub(crate) fn declared_block(args: &[Vec<u8>]) -> Option<Duration> {
    let name = upper(args, 0);
    let seconds = |n: f64| (n > 0.0).then(|| Duration::from_secs_f64(n));
    let millis = |n: f64| (n > 0.0).then(|| Duration::from_secs_f64(n / 1000.0));
    match name.as_str() {
        // The timeout is the last argument.
        "BLPOP" | "BRPOP" | "BZPOPMIN" | "BZPOPMAX" | "BRPOPLPUSH" | "BLMOVE" => {
            number(args, args.len().checked_sub(1)?).and_then(seconds)
        }
        // The timeout comes first: `BLMPOP timeout numkeys …`.
        "BLMPOP" | "BZMPOP" => number(args, 1).and_then(seconds),
        "XREAD" | "XREADGROUP" => {
            let at = args.iter().position(|a| a.eq_ignore_ascii_case(b"BLOCK"))?;
            number(args, at + 1).and_then(millis)
        }
        "WAIT" => number(args, 2).and_then(millis),
        "WAITAOF" => number(args, 3).and_then(millis),
        _ => None,
    }
}

/// The time to wait for one command's reply: the connection's limit plus
/// whatever the command itself asks the server to block for.
pub(crate) fn budget(limit: Option<Duration>, args: &[Vec<u8>]) -> Option<Duration> {
    let limit = limit?;
    Some(limit + declared_block(args).unwrap_or_default())
}

/// A short label for the command in the error: its name (and subcommand for
/// the container commands), never its arguments, which may hold values.
pub(crate) fn command_label(args: &[Vec<u8>]) -> String {
    let name = upper(args, 0);
    if name.is_empty() {
        return "the command".to_string();
    }
    let sub = upper(args, 1);
    let container = matches!(
        name.as_str(),
        "CLIENT"
            | "CONFIG"
            | "CLUSTER"
            | "COMMAND"
            | "MEMORY"
            | "OBJECT"
            | "SCRIPT"
            | "FUNCTION"
            | "XINFO"
            | "XGROUP"
            | "ACL"
            | "SLOWLOG"
            | "LATENCY"
            | "MODULE"
            | "DEBUG"
    );
    if container && !sub.is_empty() && sub.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        format!("{name} {sub}")
    } else {
        name
    }
}

fn seconds_text(limit: Duration) -> String {
    let secs = limit.as_secs_f64();
    if secs.fract() == 0.0 {
        format!("{} s", limit.as_secs())
    } else {
        format!("{secs:.1} s")
    }
}

/// The error for a command whose reply did not arrive in time.
pub(crate) fn timeout_error(limit: Duration, what: &str) -> StudioError {
    StudioError::QueryExecution(format!(
        "{what} got no reply within {} (the connection's command timeout; set the query timeout \
         to 0 for no limit). It may still be running on the server, so it was not retried.",
        seconds_text(limit)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn args(line: &str) -> Vec<Vec<u8>> {
        line.split_whitespace()
            .map(|a| a.as_bytes().to_vec())
            .collect()
    }

    fn profile(port: u16) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "wsj timeout",
            "127.0.0.1",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p.port = port;
        p.encrypt = false;
        p.timeout_seconds = 5;
        p
    }

    #[test]
    fn the_limit_follows_the_query_timeout() {
        let mut p = profile(1);
        assert_eq!(response_timeout(&p), Some(Duration::from_secs(30)));
        p.statement_timeout_seconds = Some(0);
        assert_eq!(response_timeout(&p), None);
        p.statement_timeout_seconds = Some(2);
        assert_eq!(response_timeout(&p), Some(Duration::from_secs(2)));
    }

    #[test]
    fn blocking_commands_get_their_own_wait_on_top() {
        let limit = Some(Duration::from_secs(2));
        assert_eq!(budget(limit, &args("GET k")), limit);
        assert_eq!(
            budget(limit, &args("BLPOP a b 5")),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            budget(limit, &args("BLMPOP 1.5 1 k LEFT")),
            Some(Duration::from_millis(3500))
        );
        assert_eq!(
            budget(limit, &args("XREAD COUNT 1 BLOCK 3000 STREAMS s $")),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            budget(limit, &args("WAIT 1 500")),
            Some(Duration::from_millis(2500))
        );
        assert_eq!(budget(None, &args("BLPOP k 5")), None);
        // Forever-blocking forms add nothing (the safety layer refuses them).
        assert_eq!(budget(limit, &args("BLPOP k 0")), limit);
    }

    #[test]
    fn the_error_names_the_command_and_the_limit_but_never_values() {
        let text = timeout_error(
            Duration::from_secs(2),
            &command_label(&args("debug sleep 5")),
        )
        .to_string();
        assert!(text.contains("DEBUG SLEEP"), "{text}");
        assert!(text.contains("2 s"), "{text}");
        assert!(text.contains("not retried"), "{text}");
        assert_eq!(command_label(&args("SET secret:key hunter2")), "SET");
        assert_eq!(command_label(&args("CONFIG GET maxmemory")), "CONFIG GET");
        assert_eq!(command_label(&[]), "the command");
        assert!(
            timeout_error(Duration::from_millis(1500), "GET")
                .to_string()
                .contains("1.5 s")
        );
    }

    /// A minimal RESP2 server: refuses `HELLO` like a Redis 5, answers the
    /// connection setup, answers `PING`, answers `BLPOP` after a second, and
    /// never answers `GET`.
    async fn stuck_server() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut read = BufReader::new(read);
                    loop {
                        let mut line = String::new();
                        if read.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let Some(count) = line.trim().strip_prefix('*') else {
                            continue;
                        };
                        let count: usize = count.parse().unwrap_or(0);
                        let mut parts = Vec::new();
                        for _ in 0..count {
                            let mut len = String::new();
                            read.read_line(&mut len).await.unwrap();
                            let len: usize = len.trim().trim_start_matches('$').parse().unwrap();
                            let mut buf = vec![0; len + 2];
                            read.read_exact(&mut buf).await.unwrap();
                            buf.truncate(len);
                            parts.push(String::from_utf8_lossy(&buf).to_ascii_uppercase());
                        }
                        let reply: &[u8] = match parts.first().map(String::as_str) {
                            Some("HELLO") => {
                                b"-ERR unknown command `HELLO`, with args beginning with: `3`, \r\n"
                            }
                            Some("PING") => b"+PONG\r\n",
                            Some("GET") => continue,
                            Some("BLPOP") => {
                                tokio::time::sleep(Duration::from_secs(1)).await;
                                b"*-1\r\n"
                            }
                            _ => b"+OK\r\n",
                        };
                        if write.write_all(reply).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        port
    }

    /// Live: with writes paused for 1.5 s (`CLIENT PAUSE 1500 WRITE` from a
    /// second connection; the stock image disables `DEBUG SLEEP`, and a
    /// write pause only delays other clients' writes, never fails them), a
    /// `SET` on a connection with a 1 s command timeout fails with the
    /// timeout message, and a `BLPOP` that waits 1 s of its own is not cut
    /// short. Opt-in through `SQLLY_REDIS_URL`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_slow_command_fails_with_the_timeout_message() {
        let Some(mut p) = super::super::live_profile_from_env("wsj timeout") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        p.database = Some("15".into());
        p.statement_timeout_seconds = Some(1);
        let pauser = super::super::open_client(&p).await.expect("open");
        let client = super::super::open_client(&p).await.expect("open");
        super::super::query_rows_on_client(&pauser, "CLIENT PAUSE 1500 WRITE")
            .await
            .expect("pause");
        let error = super::super::query_rows_on_client(&client, "SET wsj:timeout 1")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("SET got no reply within 1 s"), "{error}");
        tokio::time::sleep(Duration::from_millis(800)).await;
        let rows = super::super::query_rows_on_client(&pauser, "BLPOP wsj:never 1")
            .await
            .expect("BLPOP gets its own second");
        assert_eq!(rows.rows.len(), 1);
        super::super::query_rows_on_client(&pauser, "DEL wsj:timeout")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_stuck_command_times_out_with_its_name_and_a_blocking_one_does_not() {
        let port = stuck_server().await;
        let mut p = profile(port);
        p.statement_timeout_seconds = Some(1);
        // The server has no HELLO: the connection falls back to RESP2.
        let client = super::super::open_client(&p).await.expect("open");
        assert_eq!(client.protocol(), redis::ProtocolVersion::RESP2);
        let started = std::time::Instant::now();
        let error = super::super::query_rows_on_client(&client, "GET k")
            .await
            .unwrap_err()
            .to_string();
        assert!(started.elapsed() < Duration::from_secs(4), "{error}");
        assert!(error.contains("line 1"), "{error}");
        assert!(error.contains("GET got no reply within 1 s"), "{error}");
        // A blocking pop gets its own wait on top of the 1 s limit, so it is
        // not cut short: 0.5 s is declared and the server answers after 1 s,
        // inside 1 + 0.5. (A fresh connection: the stuck GET above never gets
        // its reply, and replies on one connection arrive in order.)
        let client = super::super::open_client(&p).await.expect("open");
        let rows = super::super::query_rows_on_client(&client, "BLPOP k 0.5")
            .await
            .expect("blocking pop within its budget");
        assert_eq!(rows.rows.len(), 1);
        // No limit at all.
        p.statement_timeout_seconds = Some(0);
        let unlimited = super::super::open_client(&p).await.expect("open");
        let waited = tokio::time::timeout(
            Duration::from_millis(1500),
            super::super::query_rows_on_client(&unlimited, "GET k"),
        )
        .await;
        assert!(waited.is_err(), "no limit means the driver keeps waiting");
    }
}
