//! The Redis tools on a large keyspace: the key browser's bounded `SCAN`
//! walk, the memory analyzer's sample, and the bulk dry run against a
//! database holding one million keys, timed so the validation report can
//! say how long each path takes and that each one stays bounded (the
//! browser lists at most its limit and says the walk was incomplete; the
//! analyzer samples and extrapolates; the dry run caps and says so).
//!
//! Opt-in: set `SQLLY_REDIS_SCALE_URL` to a database that already holds the
//! keys (for example `redis://127.0.0.1:6380/14` after piping one million
//! `SET wsl:<n> …` lines in). The suite never writes and never flushes.

// The timed async helpers below nest deeply enough to overflow rustc's
// default query depth when computing their layout.
#![recursion_limit = "256"]
#![cfg(not(target_arch = "wasm32"))]

use std::time::{Duration, Instant};

use sqlly_core::{CellValue, ConnectionProfile};
use sqlly_engines::redis::{RedisClient, open_client, profile_from_url, query_rows_on_client};

/// The key count the fixture is expected to hold.
const EXPECTED_KEYS: i64 = 1_000_000;

/// The key browser's default walk limit (`browse::DEFAULT_KEY_LIMIT`).
const BROWSER_LIMIT: usize = 10_000;

/// The bulk dialog's dry-run cap (`redis_bulk::BULK_KEY_LIMIT`).
const BULK_LIMIT: usize = 25_000;

fn profile() -> Option<ConnectionProfile> {
    let url = std::env::var("SQLLY_REDIS_SCALE_URL").ok()?;
    let mut profile = profile_from_url("redis-scale", &url).ok()?;
    profile.auto_wrap_rollback = false;
    profile.timeout_seconds = profile.timeout_seconds.max(120);
    Some(profile)
}

async fn timed(
    client: &RedisClient,
    label: &str,
    script: &str,
) -> (sqlly_core::MaterializedResultSet, Duration) {
    let started = Instant::now();
    let result = query_rows_on_client(client, script)
        .await
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    let elapsed = started.elapsed();
    eprintln!(
        "[scale] {label}: {} rows in {:.3} s",
        result.rows.len(),
        elapsed.as_secs_f64()
    );
    (result, elapsed)
}

fn i64_cell(cell: &CellValue) -> i64 {
    match cell {
        CellValue::I64(n) => *n,
        other => panic!("expected an integer cell, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_million_keys_stay_bounded_on_every_tool_path() {
    let Some(profile) = profile() else {
        eprintln!("skipping: SQLLY_REDIS_SCALE_URL not set");
        return;
    };
    let client = open_client(&profile).await.expect("open the scale fixture");
    let (dbsize, _) = timed(&client, "DBSIZE", "DBSIZE").await;
    let total = i64_cell(&dbsize.rows[0][0]);
    assert!(
        total >= EXPECTED_KEYS,
        "the scale fixture holds {total} keys; populate it with one million first"
    );

    // Key browser: a prefix every key matches lists the limit and says so.
    let (listing, browser) = timed(
        &client,
        "SQLLY.KEYS wsl:* (browser, default limit)",
        "SQLLY.KEYS wsl:* 10000",
    )
    .await;
    assert_eq!(listing.rows.len(), BROWSER_LIMIT);
    assert_eq!(
        listing.rows[0][3],
        CellValue::Bool(false),
        "walk incomplete"
    );
    assert!(
        browser < Duration::from_secs(30),
        "browser walk took {browser:?}"
    );

    // Key browser: a pattern nothing matches is the worst case — the walk
    // visits every slot before the cursor returns to zero.
    let (empty, full_walk) = timed(
        &client,
        "SQLLY.KEYS nomatch:* (full SCAN of the keyspace)",
        "SQLLY.KEYS nomatch:* 10000",
    )
    .await;
    assert!(empty.rows.is_empty());
    assert!(
        full_walk < Duration::from_secs(120),
        "full walk took {full_walk:?}"
    );

    // Bulk dry run: the dialog's cap plus one, so it can tell the user the
    // scan capped.
    let (dry_run, bulk) = timed(
        &client,
        "SQLLY.KEYS wsl:* 25001 (bulk dry run)",
        "SQLLY.KEYS wsl:* 25001",
    )
    .await;
    assert_eq!(dry_run.rows.len(), BULK_LIMIT + 1);
    assert_eq!(dry_run.rows[0][3], CellValue::Bool(false));
    assert!(bulk < Duration::from_secs(60), "dry run took {bulk:?}");

    // Memory analyzer: the default sample, then the maximum.
    for sample in [1000usize, 50_000] {
        let (summary, analyzer) = timed(
            &client,
            &format!("SQLLY.MEMSUMMARY {sample}"),
            &format!("SQLLY.MEMSUMMARY {sample}"),
        )
        .await;
        let names: Vec<&str> = summary.columns.iter().map(|c| c.name.as_str()).collect();
        let sampled =
            i64_cell(&summary.rows[0][names.iter().position(|n| *n == "sampled_keys").unwrap()]);
        let reported_total =
            i64_cell(&summary.rows[0][names.iter().position(|n| *n == "total_keys").unwrap()]);
        assert_eq!(
            sampled, sample as i64,
            "sampled exactly the requested count"
        );
        assert_eq!(reported_total, total, "the summary carries DBSIZE");
        assert_eq!(
            summary.rows[0][names.iter().position(|n| *n == "complete").unwrap()],
            CellValue::Bool(false),
            "a sample of {sample} from {total} keys is not complete"
        );
        assert!(
            analyzer < Duration::from_secs(120),
            "analyzer sample {sample} took {analyzer:?}"
        );
        let (top, _) = timed(
            &client,
            &format!("SQLLY.TOPKEYS {sample} 25"),
            &format!("SQLLY.TOPKEYS {sample} 25"),
        )
        .await;
        assert_eq!(top.rows.len(), 25);
    }

    // Logical database list with the memory sample.
    let (dbs, _) = timed(&client, "SQLLY.DATABASES MEMORY", "SQLLY.DATABASES MEMORY").await;
    assert!(dbs.rows.iter().any(|r| i64_cell(&r[1]) >= EXPECTED_KEYS));
}
