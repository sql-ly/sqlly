//! Byte-exact round trips through the Redis driver: a value that goes in
//! as bytes must come back as the same bytes on every path the app reads
//! it through — the script path (`SET` then `GET`), the key browser's
//! `SQLLY.KEYS` probe (as a key name), a hash field and value through
//! `HGETALL`, and a Pub/Sub payload — and must land in a `String` or
//! `Bytes` grid cell, never a numeric one. The fixtures are the values a
//! numeric coercion would mangle: `00014` (leading zeros), the empty
//! string, a NUL byte, invalid UTF-8, a 40-digit integer, `1e309` (an
//! `f64` overflow), and 100 KB / 2 MB strings.
//!
//! Opt-in: set `SQLLY_REDIS_URL` (for example `redis://127.0.0.1:6380/0`);
//! every test self-skips without it. The suite works in logical database
//! 12 under the `wsl:` prefix; the tests run in parallel on that database,
//! so each one removes exactly the keys it names and never flushes.

#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;

use sqlly_core::{CellValue, ConnectionProfile, EventBus, EventPayload};
use sqlly_engines::redis::live::{
    MAX_STORED_PAYLOAD_BYTES, PubSubPayload, open_pubsub, parse_subscribe_targets,
};
use sqlly_engines::redis::{
    RedisClient, execute_on_client, live_profile_from_env, open_client, query_rows_on_client,
};
use sqlly_intel::query::safety::redis::quote_argument;

/// The logical database this suite owns on the shared fixture.
const DATABASE: &str = "12";

/// Name and bytes of every value the suite pushes through each path.
fn fixtures() -> Vec<(&'static str, Vec<u8>)> {
    let hundred_kb: Vec<u8> = (0..100 * 1024)
        .map(|i| b"0123456789abcdef"[i % 16])
        .collect();
    let two_mb: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|i| b"The quick brown fox jumps over the lazy dog. "[i % 45])
        .collect();
    vec![
        ("leading-zeros", b"00014".to_vec()),
        ("empty", Vec::new()),
        ("nul", vec![0]),
        ("invalid-utf8", vec![0xff, 0xfe]),
        (
            "forty-digit-integer",
            b"1234567890123456789012345678901234567890".to_vec(),
        ),
        ("float-overflow", b"1e309".to_vec()),
        ("100kb", hundred_kb),
        ("2mb", two_mb),
    ]
}

fn profile() -> Option<ConnectionProfile> {
    let mut profile = live_profile_from_env("redis-bytes")?;
    profile.database = Some(DATABASE.into());
    profile.auto_wrap_rollback = false;
    // The 2 MB value makes the script, the reply and the Pub/Sub frame
    // large; give the fixture time on a loaded machine.
    profile.timeout_seconds = profile.timeout_seconds.max(60);
    Some(profile)
}

/// Open the suite's client on its own database.
async fn fresh_client(profile: &ConnectionProfile) -> RedisClient {
    let client = open_client(profile).await.expect("open the Redis fixture");
    assert_eq!(client.current_database(), 12);
    client
}

/// Remove exactly these keys (before a run, so a crashed earlier run
/// leaves nothing behind, and after it).
async fn remove_keys(client: &RedisClient, keys: &[Vec<u8>]) {
    let mut args = vec![b"UNLINK".to_vec()];
    args.extend(keys.iter().cloned());
    client.run_args(&args).await.unwrap();
}

/// A grid cell must hold exactly `expected`: as `String` when the bytes
/// are UTF-8, as `Bytes` otherwise, and never as a number, boolean or
/// null — `00014` as `I64(14)` is the bug this suite exists to catch.
fn assert_text_cell(path: &str, cell: &CellValue, expected: &[u8]) {
    match cell {
        CellValue::String(text) => {
            assert!(
                std::str::from_utf8(expected).is_ok(),
                "{path}: non-UTF-8 bytes came back as text {text:?}"
            );
            assert!(
                text.as_bytes() == expected,
                "{path}: text differs (got {} bytes, want {})",
                text.len(),
                expected.len()
            );
        }
        CellValue::Bytes(bytes) => {
            assert!(
                std::str::from_utf8(expected).is_err(),
                "{path}: UTF-8 bytes came back as a byte cell"
            );
            assert!(
                bytes.as_slice() == expected,
                "{path}: bytes differ (got {} bytes, want {})",
                bytes.len(),
                expected.len()
            );
        }
        other => panic!("{path}: value became a non-text cell {other:?}"),
    }
}

/// Run a script through the streaming path (the query tab's contract) and
/// collect each result set's rows in order.
async fn run_script(
    client: &RedisClient,
    profile: &ConnectionProfile,
    text: &str,
) -> Vec<Vec<Vec<CellValue>>> {
    let events = EventBus::new(1024);
    let mut subscription = events.subscribe();
    let (tx, mut rx) = sqlly_core::row_channel(8);
    let drain_rows = tokio::spawn(async move {
        let mut by_index: std::collections::BTreeMap<usize, Vec<Vec<CellValue>>> =
            Default::default();
        while let Some(batch) = rx.recv().await {
            by_index
                .entry(batch.result_set_index)
                .or_default()
                .extend(batch.rows);
        }
        by_index
    });
    execute_on_client(client, profile, text, &events, Some(&tx))
        .await
        .unwrap_or_else(|e| panic!("script failed: {e}"));
    drop(tx);
    let mut by_index = drain_rows.await.unwrap();
    let mut result_sets = 0;
    while let Ok(event) = subscription.try_recv() {
        if matches!(event.payload, EventPayload::ResultSetStarted { .. }) {
            result_sets += 1;
        }
    }
    (0..result_sets)
        .map(|i| by_index.remove(&i).unwrap_or_default())
        .collect()
}

fn single_cell(rows: &[Vec<CellValue>]) -> &CellValue {
    assert_eq!(rows.len(), 1, "expected one row, got {rows:?}");
    assert_eq!(rows[0].len(), 1, "expected one column, got {:?}", rows[0]);
    &rows[0][0]
}

/// `SET` through the script path (quoted the way the editor and the
/// dialogs quote values), then `GET` through both the streaming path and
/// the materialized path the tools use.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn script_set_and_get_round_trip_every_value_byte_for_byte() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    let client = fresh_client(&profile).await;
    let keys: Vec<Vec<u8>> = fixtures()
        .iter()
        .map(|(name, _)| format!("wsl:bytes:{name}").into_bytes())
        .collect();
    remove_keys(&client, &keys).await;
    for (name, value) in fixtures() {
        let key = format!("wsl:bytes:{name}");
        let script = format!("SET {key} {}\nGET {key}\n", quote_argument(&value));
        let sets = run_script(&client, &profile, &script).await;
        assert_eq!(sets.len(), 2, "{name}: SET and GET answer one set each");
        assert_eq!(single_cell(&sets[0]), &CellValue::String("OK".into()));
        assert_text_cell(
            &format!("GET {name} (streamed)"),
            single_cell(&sets[1]),
            &value,
        );

        let materialized = query_rows_on_client(&client, &format!("GET {key}"))
            .await
            .unwrap();
        assert_text_cell(
            &format!("GET {name} (materialized)"),
            single_cell(&materialized.rows),
            &value,
        );

        // The server holds exactly the bytes the script sent.
        let stored = client
            .run_args(&[b"GET".to_vec(), key.clone().into_bytes()])
            .await
            .unwrap();
        assert_eq!(
            stored,
            redis::Value::BulkString(value.clone()),
            "{name}: the script path altered the stored bytes"
        );
    }
    remove_keys(&client, &keys).await;
}

/// The key browser's `SQLLY.KEYS` probe lists key names byte for byte,
/// including a name that is only the prefix, one with a NUL, one that is
/// not UTF-8, and a 2 MB name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn key_browser_probe_keeps_key_names_byte_exact() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    let client = fresh_client(&profile).await;
    let expected: Vec<Vec<u8>> = fixtures()
        .into_iter()
        .map(|(_, value)| [b"wsl:kb:".as_slice(), &value].concat())
        .collect();
    remove_keys(&client, &expected).await;
    for key in &expected {
        client
            .run_args(&[b"SET".to_vec(), key.clone(), b"1".to_vec()])
            .await
            .unwrap();
    }
    let listing = query_rows_on_client(&client, "SQLLY.KEYS wsl:kb:* 100")
        .await
        .unwrap();
    assert_eq!(listing.columns[0].name, "key");
    assert_eq!(listing.rows.len(), expected.len(), "every key listed once");
    for (name, _) in fixtures() {
        let wanted = &expected[fixtures().iter().position(|(n, _)| *n == name).unwrap()];
        let found = listing.rows.iter().find(|row| match &row[0] {
            CellValue::String(text) => text.as_bytes() == wanted.as_slice(),
            CellValue::Bytes(bytes) => bytes == wanted,
            _ => false,
        });
        let row = found.unwrap_or_else(|| panic!("{name}: key missing or altered in the listing"));
        assert_text_cell(&format!("SQLLY.KEYS {name}"), &row[0], wanted);
        assert_eq!(row[1], CellValue::String("string".into()), "{name}: type");
    }
    remove_keys(&client, &expected).await;
}

/// A hash field name and its value both survive `HGETALL` unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hgetall_keeps_fields_and_values_byte_exact() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    let client = fresh_client(&profile).await;
    let keys: Vec<Vec<u8>> = fixtures()
        .iter()
        .map(|(name, _)| format!("wsl:h:{name}").into_bytes())
        .collect();
    remove_keys(&client, &keys).await;
    for (name, value) in fixtures() {
        let key = format!("wsl:h:{name}");
        client
            .run_args(&[
                b"HSET".to_vec(),
                key.clone().into_bytes(),
                value.clone(),
                value.clone(),
            ])
            .await
            .unwrap();
        let result = query_rows_on_client(&client, &format!("HGETALL {key}"))
            .await
            .unwrap();
        let names: Vec<&str> = result.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["field", "value"]);
        assert_eq!(result.rows.len(), 1, "{name}: one field");
        assert_text_cell(&format!("HGETALL {name} field"), &result.rows[0][0], &value);
        assert_text_cell(&format!("HGETALL {name} value"), &result.rows[0][1], &value);
    }
    remove_keys(&client, &keys).await;
}

/// TLS: the same round trip over a server that only speaks TLS, in every
/// mode the profile offers — `Require` (encrypt, trust anything),
/// `VerifyCa` / `VerifyFull` with the fixture's CA file, and `VerifyFull`
/// without it, which must refuse a self-signed server rather than connect.
/// Opt-in through `SQLLY_REDIS_TLS_URL` (`rediss://127.0.0.1:6383/0`) and
/// `SQLLY_REDIS_TLS_CA_FILE` (the PEM the server certificate chains to).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tls_modes_round_trip_and_verify_full_refuses_an_unknown_ca() {
    use sqlly_core::{StudioError, TlsMode};
    use sqlly_engines::redis::profile_from_url;
    let (Ok(url), Ok(ca_file)) = (
        std::env::var("SQLLY_REDIS_TLS_URL"),
        std::env::var("SQLLY_REDIS_TLS_CA_FILE"),
    ) else {
        eprintln!("skipping: SQLLY_REDIS_TLS_URL / SQLLY_REDIS_TLS_CA_FILE not set");
        return;
    };
    // The redis crate's rustls connector needs the process-level provider
    // the app installs at startup; a test binary has to do it itself.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut base = profile_from_url("redis-tls", &url).unwrap();
    assert_eq!(
        base.effective_tls_mode(),
        TlsMode::Require,
        "rediss:// means TLS"
    );
    base.database = Some(DATABASE.into());
    base.auto_wrap_rollback = false;
    let value = b"00014".to_vec();
    let key = b"wsl:tls:leading-zeros".to_vec();
    for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
        let mut profile = base.clone();
        profile.tls_mode = mode;
        if mode != TlsMode::Require {
            profile.tls_ca_file = Some(ca_file.clone().into());
        }
        let client = open_client(&profile)
            .await
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
        remove_keys(&client, std::slice::from_ref(&key)).await;
        let script = format!(
            "SET wsl:tls:leading-zeros {}\nGET wsl:tls:leading-zeros\n",
            quote_argument(&value)
        );
        let sets = run_script(&client, &profile, &script).await;
        assert_text_cell(
            &format!("GET over TLS {mode:?}"),
            single_cell(&sets[1]),
            &value,
        );
        remove_keys(&client, std::slice::from_ref(&key)).await;
    }
    // VerifyFull without the CA: the self-signed chain is refused up front.
    let mut strict = base.clone();
    strict.tls_mode = TlsMode::VerifyFull;
    match open_client(&strict).await {
        Err(StudioError::Connection(text)) => {
            assert!(
                !text.is_empty(),
                "the refusal must carry the TLS error text"
            );
        }
        Err(other) => panic!("expected a connection error, got {other:?}"),
        Ok(_) => panic!("VerifyFull connected to a self-signed server without its CA"),
    }
}

/// A published payload reaches the Pub/Sub viewer byte for byte up to the
/// viewer's stored cap; above it the kept prefix is exact and the message
/// reports its full length so the truncation is visible, never silent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pubsub_payloads_arrive_byte_exact_up_to_the_stored_cap() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    let client = fresh_client(&profile).await;
    let targets = parse_subscribe_targets("wsl:bytes");
    let mut listener = open_pubsub(&profile, &targets).await.unwrap();
    for (name, value) in fixtures() {
        client
            .run_args(&[b"PUBLISH".to_vec(), b"wsl:bytes".to_vec(), value.clone()])
            .await
            .unwrap();
        let message = tokio::time::timeout(Duration::from_secs(30), listener.next_message())
            .await
            .unwrap_or_else(|_| panic!("{name}: no message within 30 s"))
            .unwrap_or_else(|| panic!("{name}: the subscription ended"));
        assert_eq!(message.channel, "wsl:bytes");
        assert_eq!(
            message.payload_len,
            value.len(),
            "{name}: full length reported"
        );
        let kept: Vec<u8> = match &message.payload {
            PubSubPayload::Text(text) => {
                assert!(
                    std::str::from_utf8(&value[..text.len()]).is_ok(),
                    "{name}: non-UTF-8 payload shown as text"
                );
                text.as_bytes().to_vec()
            }
            PubSubPayload::Bytes(bytes) => bytes.clone(),
        };
        if value.len() <= MAX_STORED_PAYLOAD_BYTES {
            assert!(!message.payload_truncated(), "{name}: wrongly truncated");
            assert!(kept == value, "{name}: payload bytes differ");
        } else {
            assert!(message.payload_truncated(), "{name}: should be truncated");
            assert!(
                kept.len() <= MAX_STORED_PAYLOAD_BYTES && !kept.is_empty(),
                "{name}: kept {} bytes",
                kept.len()
            );
            assert!(
                kept.as_slice() == &value[..kept.len()],
                "{name}: the kept prefix differs from the published bytes"
            );
        }
    }
}
