//! Non-docker live SQL Server integration test.
//!
//! Unlike `sqlserver_container.rs` (which spins up its own Docker container for
//! CI), this test connects to an **already-running** SQL Server described by a
//! local gitignored config file (`.sqlly-local-sql.json`, for local dev) or by
//! `SQLLY_SQL_*` environment variables (for a CI pipeline that stands up its own
//! server). If neither source is present, [`LiveSqlConfig::load`] returns `None`
//! and the test self-skips, so the default `cargo test` run stays hermetic.

use sqlly_core::{EventBus, EventPayload};
use sqlly_engines::tds::{SqlServerTransport, TiberiusTransport};
use sqlly_testkit::LiveSqlConfig;

fn drain_payloads(subscription: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
    let mut payloads = Vec::new();
    while let Ok(event) = subscription.try_recv() {
        payloads.push(event.payload);
    }
    payloads
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_server_executes_cancels_and_streams_multi_result_sets() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!(
            "skipping live SQL test: no {} file and no SQLLY_SQL_* env vars",
            sqlly_testkit::LOCAL_SQL_CONFIG_FILE
        );
        return;
    };

    let profile = cfg.to_profile("live");
    let transport = TiberiusTransport;

    // 1) Connectivity.
    transport
        .test_connection(&profile)
        .await
        .expect("live connection should open");

    // 2) Multi-result-set execution streamed over the bounded row channel.
    //    Lifecycle events flow on the bus; rows flow on the mpsc channel with
    //    real backpressure and zero drops.
    let events = EventBus::default();
    let mut subscription = events.subscribe();
    let (rows_tx, mut rows_rx) = sqlly_core::row_channel(16);
    let drain = tokio::spawn(async move {
        let mut total = 0usize;
        while let Some(batch) = rows_rx.recv().await {
            total += batch.len();
        }
        total
    });

    transport
        .execute_query(
            &profile,
            "SELECT 1 AS first_result; SELECT 2 AS second_result;",
            &events,
            Some(&rows_tx),
        )
        .await
        .expect("live multi-result-set query should succeed");
    drop(rows_tx);

    let row_count = drain.await.expect("row drain joins");
    let payloads = drain_payloads(&mut subscription);
    let result_set_count = payloads
        .iter()
        .filter(|p| matches!(p, EventPayload::ResultSetStarted { .. }))
        .count();
    assert_eq!(
        result_set_count, 2,
        "two result sets, payloads: {payloads:?}"
    );
    assert_eq!(row_count, 2, "zero rows dropped over the bounded channel");

    // 3) Cancellation: a long WAITFOR is aborted before it completes.
    let cancel_profile = profile.clone();
    let cancel_task = tokio::spawn(async move {
        let cancel_events = EventBus::default();
        let (cancel_tx, _cancel_rx) = sqlly_core::row_channel(16);
        TiberiusTransport
            .execute_query(
                &cancel_profile,
                "WAITFOR DELAY '00:00:10'; SELECT 99 AS should_not_finish;",
                &cancel_events,
                Some(&cancel_tx),
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    cancel_task.abort();
    assert!(
        cancel_task.await.unwrap_err().is_cancelled(),
        "long-running query should be cancellable"
    );
}

/// GO batch separators split the buffer into independent batches (result-set
/// numbering stays continuous), a failing batch is reported but the run
/// continues (SSMS semantics), and a `USE` surfaces a `DatabaseChanged` event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_server_handles_go_batches_errors_and_use() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping live GO/USE test: no live SQL config");
        return;
    };
    let profile = cfg.to_profile("live-go");
    let transport = TiberiusTransport;

    async fn run(profile: &sqlly_core::ConnectionProfile, sql: &str) -> Vec<EventPayload> {
        let events = EventBus::default();
        let mut subscription = events.subscribe();
        let (rows_tx, mut rows_rx) = sqlly_core::row_channel(64);
        let drain = tokio::spawn(async move { while rows_rx.recv().await.is_some() {} });
        TiberiusTransport
            .execute_query(profile, sql, &events, Some(&rows_tx))
            .await
            .expect("GO/USE query should complete (batch errors are non-fatal)");
        drop(rows_tx);
        drain.await.expect("row drain joins");
        drain_payloads(&mut subscription)
    }

    // 1) Three GO batches → three result sets numbered 0, 1, 2.
    let payloads = run(&profile, "SELECT 1\nGO\nSELECT 2\nGO\nSELECT 3").await;
    let indices: Vec<usize> = payloads
        .iter()
        .filter_map(|p| match p {
            EventPayload::ResultSetStarted {
                result_set_index, ..
            } => Some(*result_set_index),
            _ => None,
        })
        .collect();
    assert_eq!(
        indices,
        vec![0, 1, 2],
        "continuous result-set indices across GO"
    );

    // 2) A failing middle batch is reported as a Message; the batches on either
    //    side still run (continue-after-error).
    let payloads = run(
        &profile,
        "SELECT 1\nGO\nSELECT no_such_column_zzz\nGO\nSELECT 3",
    )
    .await;
    let result_sets = payloads
        .iter()
        .filter(|p| matches!(p, EventPayload::ResultSetStarted { .. }))
        .count();
    let messages = payloads
        .iter()
        .filter(|p| matches!(p, EventPayload::Message { .. }))
        .count();
    assert_eq!(result_sets, 2, "batches 1 and 3 still produce result sets");
    assert!(messages >= 1, "the failing batch is reported as a message");

    // 3) `USE tempdb` retargets the session and surfaces a DatabaseChanged event
    //    (the test connection's default database is never tempdb).
    let _ = &transport;
    let payloads = run(&profile, "USE tempdb\nGO\nSELECT DB_NAME()").await;
    let changed = payloads.iter().find_map(|p| match p {
        EventPayload::DatabaseChanged { name, .. } => Some(name.clone()),
        _ => None,
    });
    assert_eq!(
        changed.as_deref().map(|s| s.to_ascii_lowercase()),
        Some("tempdb".to_string()),
        "USE tempdb surfaces DatabaseChanged(tempdb), payloads: {payloads:?}"
    );

    // 4) A `USE` inside a dynamic EXEC is scoped to that dynamic batch and does
    //    NOT change the calling session — so no DatabaseChanged must fire. This
    //    pins the SQL Server behavior the `may_change_database` gate relies on.
    let payloads = run(&profile, "EXEC(N'USE tempdb')").await;
    assert!(
        !payloads
            .iter()
            .any(|p| matches!(p, EventPayload::DatabaseChanged { .. })),
        "dynamic EXEC USE must not change the session database, payloads: {payloads:?}"
    );
}

/// Diagnostic benchmark (run with `--ignored --nocapture`) that isolates the
/// cost of establishing a TDS connection from the cost of running a trivial
/// query. Proves whether connect-per-query (the current engine behavior) is the
/// dominant latency vs. reusing a warm connection.
///
///   cargo test -p sqlly-tds --test sqlserver_live -- --ignored --nocapture connect_overhead
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "diagnostic benchmark; requires a live SQL Server"]
async fn connect_overhead_dominates_small_query_latency() {
    use std::time::{Duration, Instant};
    use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
    use tokio::net::TcpStream;
    use tokio_util::compat::TokioAsyncWriteCompatExt;

    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!("skipping benchmark: no live SQL config");
        return;
    };

    fn tiberius_config(cfg: &LiveSqlConfig) -> Config {
        let mut config = Config::new();
        config.host(&cfg.server);
        config.port(cfg.port);
        config.database(&cfg.database);
        config.authentication(AuthMethod::sql_server(&cfg.username, &cfg.password));
        config.encryption(if cfg.encrypt {
            EncryptionLevel::Required
        } else {
            EncryptionLevel::Off
        });
        if cfg.trust_server_certificate {
            config.trust_cert();
        }
        config
    }

    async fn fresh_client(cfg: &LiveSqlConfig) -> Client<tokio_util::compat::Compat<TcpStream>> {
        let config = tiberius_config(cfg);
        let tcp = TcpStream::connect(config.get_addr()).await.unwrap();
        tcp.set_nodelay(true).unwrap();
        Client::connect(config, tcp.compat_write()).await.unwrap()
    }

    const ITERS: u32 = 10;
    let profile = cfg.to_profile("bench");
    let transport = TiberiusTransport;

    // Warm up the server/page cache so the first-connect penalty isn't counted.
    let _ = transport.test_connection(&profile).await;

    // (A) Current engine behavior: full connect + query, every iteration.
    let mut cold_total = Duration::ZERO;
    for _ in 0..ITERS {
        let events = EventBus::default();
        let _sub = events.subscribe();
        let start = Instant::now();
        transport
            .execute_query(&profile, "SELECT 1", &events, None)
            .await
            .unwrap();
        cold_total += start.elapsed();
    }

    // (B) Reuse one warm tiberius client across all iterations.
    let mut client = fresh_client(&cfg).await;
    // Don't count the first warm query (statement cache / first-use effects).
    let _ = client
        .simple_query("SELECT 1")
        .await
        .unwrap()
        .into_results()
        .await
        .unwrap();
    let mut warm_total = Duration::ZERO;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _ = client
            .simple_query("SELECT 1")
            .await
            .unwrap()
            .into_results()
            .await
            .unwrap();
        warm_total += start.elapsed();
    }

    // (C) Isolate just the connect handshake (TCP + TLS + LOGIN7).
    let mut connect_total = Duration::ZERO;
    for _ in 0..ITERS {
        let start = Instant::now();
        let _client = fresh_client(&cfg).await;
        connect_total += start.elapsed();
    }

    let cold_ms = cold_total.as_secs_f64() * 1000.0 / ITERS as f64;
    let warm_ms = warm_total.as_secs_f64() * 1000.0 / ITERS as f64;
    let connect_ms = connect_total.as_secs_f64() * 1000.0 / ITERS as f64;

    eprintln!("\n=== TDS latency breakdown (avg of {ITERS}) ===");
    eprintln!("connect-per-query (current):  {cold_ms:8.2} ms");
    eprintln!("warm reused connection:       {warm_ms:8.2} ms");
    eprintln!("connect handshake only:       {connect_ms:8.2} ms");
    eprintln!("=> reuse would save ~{:.2} ms/query\n", cold_ms - warm_ms);
}

/// Server-side cancellation: `KILL <spid>` from a side connection ends a
/// `WAITFOR DELAY` promptly instead of letting it run out. Requires the
/// login to hold ALTER ANY CONNECTION (the local dev/sa config does); in
/// production the handle is best-effort and degrades to client-side cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_server_side_cancel_kills_running_batch() {
    let Some(cfg) = LiveSqlConfig::load() else {
        eprintln!(
            "skipping live SQL test: no {} file and no SQLLY_SQL_* env vars",
            sqlly_testkit::LOCAL_SQL_CONFIG_FILE
        );
        return;
    };
    let profile = cfg.to_profile("live-cancel");
    let mut client = sqlly_engines::tds::open_client(&profile)
        .await
        .expect("open");
    let cancel = sqlly_engines::tds::cancel_handle(&mut client, &profile)
        .await
        .expect("cancel handle");

    let started = std::time::Instant::now();
    let run_profile = profile.clone();
    let run = tokio::spawn(async move {
        let bus = EventBus::default();
        sqlly_engines::tds::execute_on_client(
            &mut client,
            &run_profile,
            "WAITFOR DELAY '00:00:30'",
            &bus,
            None,
        )
        .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    cancel.cancel().await.expect("cancel request sends");

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), run)
        .await
        .expect("cancel must end the batch promptly")
        .expect("task join");
    // The killed session surfaces as a stream error or an errored outcome —
    // either way the batch must not have waited out the full delay.
    assert!(
        result.is_err() || result.is_ok_and(|outcome| outcome.had_error),
        "killed session must not report a clean success"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "must not wait out the full delay"
    );
}
