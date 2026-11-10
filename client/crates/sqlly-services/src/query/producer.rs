//! The query producer: opens (or checks out) the right native driver for a
//! profile's engine and streams the query onto the `EventBus` + `RowChannel`
//! pair the drain loop consumes. Also owns the server-side cancel plumbing
//! ([`CancelHook`] / [`CancelSlot`]) and the statement-timeout race.

#![allow(clippy::too_many_arguments)]

use std::sync::{Arc, Mutex};

use sqlly_core::{ConnectionProfile, EventBus, EventSubscription, StudioResult};

use crate::query::manual_tx::{self, PinnedClient};
use crate::tds_pool::{MySqlPool, PgPool, TdsPool};

/// A backend-specific server-side cancel action, registered by the producer
/// once its client is open. Invoked (at most once) by `cancel()` before the
/// producer task is aborted, so the server stops the statement instead of
/// just this process abandoning the socket. Backends without a server-side
/// cancel simply never register (all four native backends register one:
/// TDS `KILL <spid>`, PostgreSQL wire cancel, MySQL `KILL QUERY`, SQLite
/// `interrupt`).
pub type CancelHook = Box<dyn FnOnce() + Send>;

/// Shared slot the producer publishes its cancel hook into. Public so hosts
/// that drive [`run_query_producer`] directly (the CLI engine) can fire the
/// server-side cancel before aborting the producer task.
pub type CancelSlot = Arc<Mutex<Option<CancelHook>>>;

/// Open the right driver for the profile's engine and stream the query.
/// One arm per truly distinct connection kind: TDS (`sqlly-tds`),
/// PostgreSQL wire (`sqlly-pg`), MySQL wire (`sqlly-mysql`), and in-process
/// SQLite (`sqlly-sqlite`). Engines without a native driver yet fail with a
/// clear capability error instead of a protocol mismatch.
///
/// The TDS arm runs on the warm pool: it checks out the lane's idle client
/// (opening fresh on a miss), checks the client back in on clean completion,
/// and drops it on any error — an errored or aborted TDS stream is in an
/// indeterminate state and must never be reused (see
/// [`sqlly_engines::tds::execute_on_client`]). A warm client that fails *before any
/// event was published* is provably stale pre-execution (nothing reached the
/// server), so it is retried once on a fresh connection; once an event has
/// been emitted there is no retry, guaranteeing no duplicated rows or side
/// effects.
/// Returns the first batch-level SQL error, when the drivers surface one
/// out-of-band (TDS). Callers that only consume the event stream can ignore
/// it; the CLI engine uses it to pick its terminal frame.
///
/// When the profile carries a statement timeout, the whole run races a
/// deadline: on expiry the registered server-side cancel fires (KILL / wire
/// cancel / KILL QUERY / interrupt), the producer gets a short grace period to
/// unwind, and the result is a typed [`StudioError::QueryTimeout`].
///
/// [`StudioError::QueryTimeout`]: sqlly_core::StudioError::QueryTimeout
#[allow(clippy::too_many_arguments)] // two call sites; a params struct would just relocate the list
pub async fn run_query_producer(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pool: &TdsPool,
    pg_pool: &PgPool,
    mysql_pool: &MySqlPool,
    session: Option<&str>,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let pinned = manual_tx::PinnedSessions::default();
    run_query_producer_with_pinned(
        profile,
        sql,
        events,
        rows_tx,
        pool,
        pg_pool,
        mysql_pool,
        session,
        &pinned,
        cancel_slot,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_query_producer_with_pinned(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pool: &TdsPool,
    pg_pool: &PgPool,
    mysql_pool: &MySqlPool,
    session: Option<&str>,
    pinned: &manual_tx::PinnedSessions,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let run = run_query_producer_unlimited(
        profile,
        sql,
        events,
        rows_tx,
        pool,
        pg_pool,
        mysql_pool,
        session,
        pinned,
        cancel_slot,
    );
    let Some(limit) = profile.statement_timeout() else {
        return run.await;
    };
    tokio::pin!(run);
    tokio::select! {
        result = &mut run => result,
        _ = tokio::time::sleep(limit) => {
            // Stop the statement on the server, then give the producer a short
            // grace period to unwind through its normal error path (so the
            // client is dropped/checked-in by its own rules). Whatever it
            // reports is superseded by the typed timeout.
            if let Some(hook) = cancel_slot.lock().expect("cancel slot poisoned").take() {
                hook();
            }
            let _ = tokio::time::timeout(std::time::Duration::from_secs(5), &mut run).await;
            Err(sqlly_core::StudioError::QueryTimeout(limit))
        }
    }
}

/// Engine dispatch: one helper per connection kind, no timeout race.
#[allow(clippy::too_many_arguments)]
async fn run_query_producer_unlimited(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pool: &TdsPool,
    pg_pool: &PgPool,
    mysql_pool: &MySqlPool,
    session: Option<&str>,
    pinned: &manual_tx::PinnedSessions,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    use sqlly_core::ConnectionKind;
    match profile.database_type.connection_kind() {
        ConnectionKind::Tds => {
            run_tds_query(
                profile,
                sql,
                events,
                rows_tx,
                pool,
                session,
                pinned,
                cancel_slot,
            )
            .await
        }
        ConnectionKind::PostgresWire => {
            run_pg_query(
                profile,
                sql,
                events,
                rows_tx,
                pg_pool,
                session,
                pinned,
                cancel_slot,
            )
            .await
        }
        ConnectionKind::MySqlWire => {
            run_mysql_query(
                profile,
                sql,
                events,
                rows_tx,
                mysql_pool,
                session,
                pinned,
                cancel_slot,
            )
            .await
        }
        ConnectionKind::SqliteFile => {
            run_sqlite_query(profile, sql, events, rows_tx, cancel_slot).await
        }
        ConnectionKind::DuckDbFile => {
            run_duckdb_query(profile, sql, events, rows_tx, cancel_slot).await
        }
        ConnectionKind::ClickHouseHttp => {
            run_clickhouse_query(profile, sql, events, rows_tx, cancel_slot).await
        }
        ConnectionKind::OracleNet => {
            run_oracle_query(profile, sql, events, rows_tx, cancel_slot).await
        }
        ConnectionKind::HranaHttp => {
            run_libsql_query(profile, sql, events, rows_tx, cancel_slot).await
        }
        ConnectionKind::Resp => run_redis_query(profile, sql, events, rows_tx, cancel_slot).await,
        other => Err(sqlly_core::StudioError::CapabilityNotSupported(format!(
            "no driver yet for {} ({other:?} connections)",
            profile.database_type.display_name()
        ))),
    }
}

/// The stale-warm retry contract shared by every pooled backend: a probe
/// subscription created BEFORE the warm execution attempt has seen zero
/// events on failure ⟺ nothing reached the server ⟺ safe to retry the query
/// on a fresh connection without duplicating rows or side effects.
fn warm_failure_was_pre_execution(probe: &mut EventSubscription) -> bool {
    matches!(
        probe.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    )
}

fn pinned_pool_key(session: &str, profile: &ConnectionProfile) -> String {
    TdsPool::pool_key(Some(session), profile)
}

async fn run_on_pinned_tds(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    key: &str,
    pinned: &manual_tx::PinnedSessions,
    mut client: PinnedClient,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let PinnedClient::Tds(ref mut warm) = client else {
        return Err(sqlly_core::StudioError::Other(
            "pinned client kind mismatch".to_string(),
        ));
    };
    let mut probe = events.subscribe();
    match sqlly_engines::tds::cancel_handle(warm, profile).await {
        Ok(handle) => register_tds_cancel(cancel_slot, handle),
        Err(error) => tracing::debug!(%error, "sql server cancel handle unavailable"),
    }
    match sqlly_engines::tds::execute_on_client_keep_transaction(
        warm,
        profile,
        sql,
        events,
        Some(rows_tx),
    )
    .await
    {
        Ok(outcome) => {
            if outcome.should_discard_client() {
                pinned.mark_lost(key);
            } else {
                pinned.put_back(key, client);
            }
            Ok(outcome.first_error)
        }
        Err(err) if warm_failure_was_pre_execution(&mut probe) => {
            pinned.mark_lost(key);
            Err(err)
        }
        Err(err) => {
            pinned.put_back(key, client);
            Err(err)
        }
    }
}

async fn run_on_pinned_pg(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    key: &str,
    pinned: &manual_tx::PinnedSessions,
    client: PinnedClient,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let PinnedClient::Pg(ref warm) = client else {
        return Err(sqlly_core::StudioError::Other(
            "pinned client kind mismatch".to_string(),
        ));
    };
    let mut probe = events.subscribe();
    register_pg_cancel(cancel_slot, warm);
    match sqlly_engines::pg::execute_on_client(warm, profile, sql, events, Some(rows_tx)).await {
        Ok(()) => {
            pinned.put_back(key, client);
            Ok(None)
        }
        Err(err) if warm_failure_was_pre_execution(&mut probe) => {
            pinned.mark_lost(key);
            Err(err)
        }
        Err(err) => {
            pinned.put_back(key, client);
            Err(err)
        }
    }
}

async fn run_on_pinned_mysql(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    key: &str,
    pinned: &manual_tx::PinnedSessions,
    client: PinnedClient,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let PinnedClient::MySql(ref warm) = client else {
        return Err(sqlly_core::StudioError::Other(
            "pinned client kind mismatch".to_string(),
        ));
    };
    let mut probe = events.subscribe();
    register_mysql_cancel(warm, profile, cancel_slot).await;
    match sqlly_engines::mysql::execute_on_client(warm, profile, sql, events, Some(rows_tx)).await {
        Ok(outcome) => {
            if outcome.should_discard_client() {
                pinned.mark_lost(key);
            } else {
                pinned.put_back(key, client);
            }
            Ok(None)
        }
        Err(err) if warm_failure_was_pre_execution(&mut probe) => {
            pinned.mark_lost(key);
            Err(err)
        }
        // `USE other; SELECT * FROM missing` fails after the USE has already
        // moved the session; keeping it would run the tab's later statements
        // in `other` while the tab still names the profile's database.
        Err(err) if warm.left_profile_database() => {
            pinned.mark_lost(key);
            Err(err)
        }
        Err(err) => {
            pinned.put_back(key, client);
            Err(err)
        }
    }
}

/// Stores a TDS `KILL <spid>` hook (fired from a short side connection —
/// tiberius exposes no Attention signal) into the shared cancel slot.
/// Registration is best-effort — without the KILL permission the request
/// fails at cancel time and the run degrades to client-side cancel only.
pub fn register_tds_cancel(cancel_slot: &CancelSlot, handle: sqlly_engines::tds::TdsCancelHandle) {
    *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || {
        tokio::spawn(async move {
            if let Err(error) = handle.cancel().await {
                tracing::debug!(%error, "sql server cancel request failed");
            }
        });
    }));
}

/// Checks out the lane's warm TDS client and runs the sub-millisecond
/// `@@SPID` probe, which doubles as a liveness check: a warm client that
/// fails it is provably stale before any of the user's SQL ran, so it is
/// dropped here and the caller's fresh path takes over (same guarantee as
/// the zero-events retry contract). On success the cancel hook is already
/// registered.
/// How long a warm SQL Server client gets to answer the `SELECT @@SPID`
/// probe. A healthy server answers in milliseconds; a client whose network
/// went away (Wi-Fi switch, VPN reconnect, sleep) never answers, and without
/// a bound the query would wait on it until the operating system gave up on
/// the socket, minutes later. Past this, the client is dropped and the query
/// opens a fresh connection.
const WARM_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Runs a warm-client probe, failing it once `limit` passes (the caller
/// passes [`WARM_PROBE_TIMEOUT`]).
async fn within_warm_probe_timeout<T>(
    limit: std::time::Duration,
    probe: impl std::future::Future<Output = StudioResult<T>>,
) -> StudioResult<T> {
    match tokio::time::timeout(limit, probe).await {
        Ok(result) => result,
        Err(_) => {
            let seconds = limit.as_secs_f64().to_string();
            Err(sqlly_core::StudioError::ConnectionMessage(
                sqlly_core::DisplayMessage::new(
                    "services.query.errors.pooled_connection_timeout",
                    format!(
                        "Connection error: the pooled connection did not answer within {seconds} seconds"
                    ),
                )
                .with_arg("seconds", seconds),
            ))
        }
    }
}

async fn checkout_probed_tds_client(
    pool: &TdsPool,
    key: &str,
    profile: &ConnectionProfile,
    cancel_slot: &CancelSlot,
) -> Option<sqlly_engines::tds::TdsClient> {
    let mut warm = pool.checkout(key)?;
    match within_warm_probe_timeout(
        WARM_PROBE_TIMEOUT,
        sqlly_engines::tds::cancel_handle(&mut warm, profile),
    )
    .await
    {
        Ok(handle) => {
            register_tds_cancel(cancel_slot, handle);
            Some(warm)
        }
        Err(error) => {
            tracing::debug!(
                %error,
                "warm TDS client failed the spid probe; opening fresh"
            );
            None
        }
    }
}

/// Opens a fresh TDS connection (logging the connect latency under
/// `log_label`), registers the server-side cancel hook, streams the query,
/// and checks the client back into the pool on a clean, non-discarding
/// completion.
#[allow(clippy::too_many_arguments)] // the producer's own streaming context, threaded through
async fn execute_tds_on_fresh_client(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pool: &TdsPool,
    key: String,
    session: Option<&str>,
    cancel_slot: &CancelSlot,
    log_label: &str,
) -> StudioResult<Option<String>> {
    let connect_started = std::time::Instant::now();
    let mut client = sqlly_engines::tds::open_client(profile).await?;
    tracing::debug!(
        connect_ms = connect_started.elapsed().as_millis(),
        lane = session.unwrap_or("default"),
        "{log_label}"
    );
    match sqlly_engines::tds::cancel_handle(&mut client, profile).await {
        Ok(handle) => register_tds_cancel(cancel_slot, handle),
        Err(error) => tracing::debug!(%error, "sql server cancel handle unavailable"),
    }
    let result =
        sqlly_engines::tds::execute_on_client(&mut client, profile, sql, events, Some(rows_tx))
            .await;
    if let Ok(outcome) = &result
        && !outcome.should_discard_client()
    {
        pool.checkin(key, client);
    }
    result.map(|outcome| outcome.first_error)
}

/// TDS arm: warm-pool checkout with the spid probe, one stale-warm retry on
/// a provably pre-execution failure, fresh connection otherwise. A clean
/// completion checks the client back in unless a `USE` changed the live
/// database (the client no longer matches its pool key).
#[allow(clippy::too_many_arguments)]
async fn run_tds_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pool: &TdsPool,
    session: Option<&str>,
    pinned: &manual_tx::PinnedSessions,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    if let Some(session) = session {
        let key = pinned_pool_key(session, profile);
        if let Some(client) = pinned.take(&key)? {
            return run_on_pinned_tds(
                profile,
                sql,
                events,
                rows_tx,
                &key,
                pinned,
                client,
                cancel_slot,
            )
            .await;
        }
    }
    let key = TdsPool::pool_key(session, profile);
    let warm = checkout_probed_tds_client(pool, &key, profile, cancel_slot).await;
    if let Some(mut warm) = warm {
        // Probe subscription created BEFORE execution: any event the
        // warm attempt publishes lands here. Zero events on failure
        // ⟺ nothing reached the server ⟺ safe to retry fresh.
        let mut probe = events.subscribe();
        match sqlly_engines::tds::execute_on_client(&mut warm, profile, sql, events, Some(rows_tx))
            .await
        {
            Ok(outcome) => {
                // A `USE` changed the live database, so this client no
                // longer matches its pool key — discard it and let the
                // next query reopen at the new database.
                if !outcome.should_discard_client() {
                    pool.checkin(key, warm);
                }
                return Ok(outcome.first_error);
            }
            Err(err) if warm_failure_was_pre_execution(&mut probe) => {
                tracing::debug!(
                    error = %err,
                    "warm TDS client was stale pre-execution; retrying on a fresh connection"
                );
                drop(warm);
                drop(probe);
                return execute_tds_on_fresh_client(
                    profile,
                    sql,
                    events,
                    rows_tx,
                    pool,
                    key,
                    session,
                    cancel_slot,
                    "fresh TDS connection opened (stale-warm retry)",
                )
                .await;
            }
            Err(err) => return Err(err),
        }
    }

    execute_tds_on_fresh_client(
        profile,
        sql,
        events,
        rows_tx,
        pool,
        key,
        session,
        cancel_slot,
        "fresh TDS connection opened",
    )
    .await
}

/// Stores the PostgreSQL wire-cancel hook into the shared cancel slot. The
/// cancel request runs on its own short connection; fire and forget — cancel
/// is best-effort by protocol design.
pub fn register_pg_cancel(cancel_slot: &CancelSlot, client: &sqlly_engines::pg::PgClient) {
    let handle = sqlly_engines::pg::cancel_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || {
        tokio::spawn(async move {
            if let Err(error) = handle.cancel().await {
                tracing::debug!(%error, "postgres cancel request failed");
            }
        });
    }));
}

/// PostgreSQL arm: warm-pool checkout with the same stale-warm retry
/// contract as TDS, fresh connection otherwise.
#[allow(clippy::too_many_arguments)]
async fn run_pg_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    pg_pool: &PgPool,
    session: Option<&str>,
    pinned: &manual_tx::PinnedSessions,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    if let Some(session) = session {
        let key = pinned_pool_key(session, profile);
        if let Some(client) = pinned.take(&key)? {
            return run_on_pinned_pg(
                profile,
                sql,
                events,
                rows_tx,
                &key,
                pinned,
                client,
                cancel_slot,
            )
            .await;
        }
    }
    let key = PgPool::pool_key(session, profile);
    if let Some(warm) = pg_pool.checkout(&key) {
        // Same stale-warm retry contract as TDS: zero events on
        // failure ⟺ nothing reached the server ⟺ safe to retry fresh.
        let mut probe = events.subscribe();
        register_pg_cancel(cancel_slot, &warm);
        match sqlly_engines::pg::execute_on_client(&warm, profile, sql, events, Some(rows_tx)).await
        {
            Ok(()) => {
                pg_pool.checkin(key, warm);
                return Ok(None);
            }
            Err(err) if warm_failure_was_pre_execution(&mut probe) => {
                tracing::debug!(
                    error = %err,
                    "warm PG client was stale pre-execution; retrying on a fresh connection"
                );
                drop(warm);
                drop(probe);
            }
            Err(err) => return Err(err),
        }
    }
    let client = sqlly_engines::pg::open_client(profile).await?;
    register_pg_cancel(cancel_slot, &client);
    let result =
        sqlly_engines::pg::execute_on_client(&client, profile, sql, events, Some(rows_tx)).await;
    if result.is_ok() {
        pg_pool.checkin(key, client);
    }
    result.map(|()| None)
}

/// Stores the MySQL `KILL QUERY <id>` hook into the shared cancel slot. The
/// kill runs from a short side connection and stops the in-flight statement
/// without killing the session; MySQL permits it for the same user's
/// connections. Registered while the client is idle (cancel_handle briefly
/// locks the connection to read its server-assigned id).
pub async fn register_mysql_cancel(
    client: &sqlly_engines::mysql::MySqlClient,
    profile: &ConnectionProfile,
    cancel_slot: &CancelSlot,
) {
    match sqlly_engines::mysql::cancel_handle(client, profile).await {
        Ok(handle) => {
            *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || {
                tokio::spawn(async move {
                    if let Err(error) = handle.cancel().await {
                        tracing::debug!(%error, "mysql cancel request failed");
                    }
                });
            }));
        }
        Err(error) => tracing::debug!(%error, "mysql cancel handle unavailable"),
    }
}

/// MySQL arm: warm-pool checkout with the same stale-warm retry contract as
/// TDS/PG, fresh connection otherwise.
#[allow(clippy::too_many_arguments)]
async fn run_mysql_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    mysql_pool: &MySqlPool,
    session: Option<&str>,
    pinned: &manual_tx::PinnedSessions,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    if let Some(session) = session {
        let key = pinned_pool_key(session, profile);
        if let Some(client) = pinned.take(&key)? {
            return run_on_pinned_mysql(
                profile,
                sql,
                events,
                rows_tx,
                &key,
                pinned,
                client,
                cancel_slot,
            )
            .await;
        }
    }
    let key = MySqlPool::pool_key(session, profile);
    if let Some(warm) = mysql_pool.checkout(&key) {
        // Same stale-warm retry contract as TDS/PG: zero events on
        // failure ⟺ nothing reached the server ⟺ safe to retry fresh.
        let mut probe = events.subscribe();
        register_mysql_cancel(&warm, profile, cancel_slot).await;
        match sqlly_engines::mysql::execute_on_client(&warm, profile, sql, events, Some(rows_tx))
            .await
        {
            Ok(outcome) => {
                // A `USE` moved the live database off the pool key (or it
                // could not be confirmed) — discard rather than re-pool.
                if !outcome.should_discard_client() {
                    mysql_pool.checkin(key, warm);
                }
                return Ok(None);
            }
            Err(err) if warm_failure_was_pre_execution(&mut probe) => {
                tracing::debug!(
                    error = %err,
                    "warm MySQL client was stale pre-execution; retrying on a fresh connection"
                );
                drop(warm);
                drop(probe);
            }
            Err(err) => return Err(err),
        }
    }
    let client = sqlly_engines::mysql::open_client(profile).await?;
    register_mysql_cancel(&client, profile, cancel_slot).await;
    let result =
        sqlly_engines::mysql::execute_on_client(&client, profile, sql, events, Some(rows_tx)).await;
    if let Ok(outcome) = &result
        && !outcome.should_discard_client()
    {
        mysql_pool.checkin(key, client);
    }
    result.map(|_| None)
}

/// SQLite arm: in-process, no pool — the cancel hook is the connection's
/// `interrupt` handle.
/// SQLite arm: register the connection interrupt handle for Cancel.
pub fn register_sqlite_cancel(
    cancel_slot: &CancelSlot,
    client: &sqlly_engines::sqlite::SqliteClient,
) {
    let interrupt = sqlly_engines::sqlite::interrupt_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") =
        Some(Box::new(move || interrupt.interrupt()));
}

async fn run_sqlite_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::sqlite::open_client(profile).await?;
    register_sqlite_cancel(cancel_slot, &client);
    sqlly_engines::sqlite::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

/// DuckDB arm: in-process like SQLite (one shared database instance per file
/// inside the driver, no pool here); Cancel is the connection's `interrupt`.
pub fn register_duckdb_cancel(
    cancel_slot: &CancelSlot,
    client: &sqlly_engines::duckdb::DuckDbClient,
) {
    let interrupt = sqlly_engines::duckdb::interrupt_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") =
        Some(Box::new(move || interrupt.interrupt()));
}

async fn run_duckdb_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::duckdb::open_client(profile).await?;
    register_duckdb_cancel(cancel_slot, &client);
    sqlly_engines::duckdb::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

/// ClickHouse arm: one HTTP session per query, no pool. Cancel releases
/// the driver's response loop at once and sends `KILL QUERY` for the
/// statement's `query_id` from a second request.
pub fn register_clickhouse_cancel(
    cancel_slot: &CancelSlot,
    client: &sqlly_engines::clickhouse::ClickHouseClient,
) {
    let handle = sqlly_engines::clickhouse::cancel_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || {
        let kill = handle.cancel();
        tokio::spawn(async move {
            if let Err(error) = kill.await {
                tracing::debug!(%error, "clickhouse cancel request failed");
            }
        });
    }));
}

async fn run_clickhouse_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::clickhouse::open_client(profile).await?;
    register_clickhouse_cancel(cancel_slot, &client);
    sqlly_engines::clickhouse::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

/// Oracle arm: one session per query (no pool yet); Cancel is the session's
/// `break_execution`, which fails the running statement with ORA-01013.
pub fn register_oracle_cancel(
    cancel_slot: &CancelSlot,
    client: &sqlly_engines::oracle::OracleClient,
) {
    let handle = sqlly_engines::oracle::break_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") =
        Some(Box::new(move || handle.break_execution()));
}

async fn run_oracle_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::oracle::open_client(profile).await?;
    register_oracle_cancel(cancel_slot, &client);
    sqlly_engines::oracle::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

/// libSQL / Turso arm: one Hrana request per script, no pool. Cancel
/// releases the driver's in-flight request; the server has no kill
/// primitive, so the statement finishes there on its own.
pub fn register_libsql_cancel(
    cancel_slot: &CancelSlot,
    client: &sqlly_engines::libsql::LibSqlClient,
) {
    let handle = sqlly_engines::libsql::cancel_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || handle.cancel()));
}

async fn run_libsql_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::libsql::open_client(profile).await?;
    register_libsql_cancel(cancel_slot, &client);
    sqlly_engines::libsql::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

/// Redis arm: one connection per script, no pool. Cancel releases the
/// driver's command loop at once; dropping the client closes the socket.
pub fn register_redis_cancel(cancel_slot: &CancelSlot, client: &sqlly_engines::redis::RedisClient) {
    let handle = sqlly_engines::redis::cancel_handle(client);
    *cancel_slot.lock().expect("cancel slot poisoned") = Some(Box::new(move || handle.cancel()));
}

async fn run_redis_query(
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: &sqlly_core::RowSender,
    cancel_slot: &CancelSlot,
) -> StudioResult<Option<String>> {
    let client = sqlly_engines::redis::open_client(profile).await?;
    register_redis_cancel(cancel_slot, &client);
    sqlly_engines::redis::execute_on_client(&client, profile, sql, events, Some(rows_tx))
        .await
        .map(|()| None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMIT: std::time::Duration = std::time::Duration::from_millis(20);

    #[tokio::test]
    async fn a_warm_probe_that_never_answers_gives_up() {
        let started = std::time::Instant::now();
        let result =
            within_warm_probe_timeout(LIMIT, std::future::pending::<StudioResult<()>>()).await;
        assert!(matches!(
            result,
            Err(sqlly_core::StudioError::ConnectionMessage(ref message))
                if message.key() == Some("services.query.errors.pooled_connection_timeout")
        ));
        assert!(started.elapsed() >= LIMIT);
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    #[tokio::test]
    async fn a_warm_probe_that_answers_keeps_its_result() {
        let result =
            within_warm_probe_timeout(LIMIT, async { Ok::<_, sqlly_core::StudioError>(42) }).await;
        assert_eq!(result.unwrap(), 42);
    }

    #[test]
    fn the_warm_probe_limit_is_short() {
        assert!(WARM_PROBE_TIMEOUT <= std::time::Duration::from_secs(5));
    }
}
