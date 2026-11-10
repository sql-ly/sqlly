//! # Redis Sentinel
//!
//! A profile with [`RedisSentinelConfig`] reaches its data through the
//! master the sentinels currently name, never through `server:port`:
//!
//! - **Resolve.** [`open_client`] asks each sentinel in order
//!   `SENTINEL GET-MASTER-ADDR-BY-NAME <master>`, connects to the address the
//!   first one names, and checks `ROLE` says `master` (a sentinel that has
//!   not caught up with a failover yet is skipped). The answering sentinel
//!   and the master are kept on the client as a [`SentinelRoute`], which
//!   Test Connection reports.
//! - **Prefer replica for reads.** When the profile asks for it, a script
//!   whose every command is read-only (by the shared safety classifier) runs
//!   on a replica the sentinels report healthy, discovered through the
//!   `redis` crate's Sentinel support and opened with `READONLY` (refused
//!   harmlessly by a non-cluster replica). Writes, `SELECT`, and the
//!   server-describing `SQLLY.*` commands always run on the master; with no
//!   healthy replica the master serves the read.
//! - **Failover.** When a command fails because the connection dropped or
//!   the server answered `READONLY` (the old master was demoted), the client
//!   re-resolves the master **once per script** and swaps its connection in
//!   place. A read-only script then retries the failed command (reads are
//!   idempotent); a script that writes is never retried — it fails with
//!   "master changed, re-run" so the user decides, because the write may or
//!   may not have landed before the connection went away (the #6292
//!   no-duplicate-writes rule).
//!
//! Sentinels are dialed directly with the profile's TLS mode and the
//! Sentinel password (when set); SSH tunnels, proxies and Kubernetes
//! port-forwards are refused for Sentinel profiles because every sentinel,
//! master and replica would need its own forward.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use redis::{ConnectionInfo, IntoConnectionInfo, RedisConnectionInfo, Value};
use sqlly_core::{
    ConnectionProfile, RedisSentinelConfig, SecretProvider, SecretRef, StudioError, StudioResult,
    TlsMode,
};
use sqlly_intel::query::safety::redis::{RedisCommand, command_access};

use super::RedisClient;
use super::shape::Shaped;

/// The longest one sentinel may take to answer before the next is asked.
const SENTINEL_ASK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long "no healthy replica" is remembered before asking again.
const NO_REPLICA_RETRY: Duration = Duration::from_secs(30);

/// Which master a Sentinel client is on and who named it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentinelRoute {
    /// The master name the sentinels watch.
    pub master_name: String,
    /// `host:port` of the current master.
    pub master: String,
    /// `host:port` of the sentinel that named it.
    pub sentinel: String,
    /// `host:port` of the replica read-only scripts run on, once one is in
    /// use.
    pub replica: Option<String>,
}

impl SentinelRoute {
    pub fn summary_message(&self) -> sqlly_core::DisplayMessage {
        let key = if self.replica.is_some() {
            "services.redis.sentinel.summary_replica"
        } else {
            "services.redis.sentinel.summary"
        };
        let message = sqlly_core::DisplayMessage::new(key, self.summary())
            .with_arg("sentinel", self.sentinel.clone())
            .with_arg("master", self.master.clone())
            .with_arg("master_name", self.master_name.clone());
        match &self.replica {
            Some(replica) => message.with_arg("replica", replica),
            None => message,
        }
    }

    /// English compatibility fallback; localized callers use [`Self::summary_message`].
    pub fn summary(&self) -> String {
        let mut text = format!(
            "Sentinel {} named {} as the master of '{}'",
            self.sentinel, self.master, self.master_name
        );
        if let Some(replica) = &self.replica {
            text.push_str(&format!("; read-only scripts use replica {replica}"));
        }
        text
    }
}

enum ReplicaSlot {
    /// Not looked for yet (or forgotten after a failover).
    Unknown,
    /// No healthy replica when last asked.
    None(Instant),
    Ready(Arc<RedisClient>),
}

/// Per-client Sentinel state: the profile (for reconnects), the route, the
/// replica, and how many failovers this client has followed.
pub struct SentinelSession {
    profile: ConnectionProfile,
    config: RedisSentinelConfig,
    route: parking_lot::Mutex<SentinelRoute>,
    replica: tokio::sync::Mutex<ReplicaSlot>,
    failovers: AtomicU64,
}

impl SentinelSession {
    /// The current route.
    pub fn route(&self) -> SentinelRoute {
        self.route.lock().clone()
    }

    /// How many times this client re-resolved the master.
    pub fn failovers(&self) -> u64 {
        self.failovers.load(Ordering::Relaxed)
    }

    async fn forget_replica(&self) {
        *self.replica.lock().await = ReplicaSlot::Unknown;
        self.route.lock().replica = None;
    }

    /// The replica for a read-only script, discovering one on first use.
    /// `None` means "run on the master".
    async fn replica_for(&self, master: &RedisClient) -> Option<Arc<RedisClient>> {
        let mut slot = self.replica.lock().await;
        let replica = match &*slot {
            ReplicaSlot::Ready(replica) => replica.clone(),
            ReplicaSlot::None(when) if when.elapsed() < NO_REPLICA_RETRY => return None,
            // An Entra profile dials the replica with the newest token the
            // master knows, never the one the session started with.
            _ => match open_replica(
                &master.entra_fresh_profile(&self.profile),
                &self.config,
                master,
            )
            .await
            {
                Some(replica) => {
                    let replica = Arc::new(replica);
                    self.route.lock().replica = Some(replica.endpoint());
                    *slot = ReplicaSlot::Ready(replica.clone());
                    replica
                }
                None => {
                    *slot = ReplicaSlot::None(Instant::now());
                    return None;
                }
            },
        };
        // Entra: a replica opened earlier follows the master's newer token
        // (its own keeper also re-authenticates before expiry).
        if let Err(error) = replica.refresh_entra(master.entra_token().as_deref()).await {
            tracing::debug!(%error, "Entra re-authentication of the Sentinel replica failed");
            *slot = ReplicaSlot::Unknown;
            self.route.lock().replica = None;
            return None;
        }
        // Follow the master's logical database.
        let db = master.current_database();
        if replica.current_database() != db && replica.select_database(db).await.is_err() {
            *slot = ReplicaSlot::Unknown;
            self.route.lock().replica = None;
            return None;
        }
        Some(replica)
    }

    /// Re-resolve the master and move `client` onto it in place, keeping the
    /// logical database it was on.
    async fn failover(&self, client: &RedisClient) -> StudioResult<()> {
        // Entra: the new master signs in with the newest token, not the one
        // the session started with (which may have expired by now).
        let profile = client.entra_fresh_profile(&self.profile);
        let (fresh, route) = resolve_and_connect(&profile, &self.config).await?;
        let db = client.current_database();
        if fresh.current_database() != db {
            fresh.select_database(db).await?;
        }
        *client.conn.lock() = fresh.conn.lock().clone();
        *client.endpoint.lock() = fresh.endpoint();
        // The client's Entra keeper keeps re-authenticating the swapped-in
        // connection; record the login it signed in with.
        client.adopt_entra_login(&fresh);
        *self.route.lock() = route;
        *self.replica.lock().await = ReplicaSlot::Unknown;
        self.failovers.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// The route of a Sentinel client, `None` for every other client.
pub fn route(client: &RedisClient) -> Option<SentinelRoute> {
    client.sentinel.as_ref().map(|s| s.route())
}

/// How many failovers a Sentinel client has followed (0 for others).
pub fn failovers(client: &RedisClient) -> u64 {
    client.sentinel.as_ref().map_or(0, |s| s.failovers())
}

/// Open a client on the master the sentinels name (the hook in
/// [`super::open_client`]).
pub(super) async fn open_client(
    profile: &ConnectionProfile,
    config: &RedisSentinelConfig,
) -> StudioResult<RedisClient> {
    let (mut client, route) = resolve_and_connect(profile, config).await?;
    client.sentinel = Some(Arc::new(SentinelSession {
        profile: profile.clone(),
        config: config.clone(),
        route: parking_lot::Mutex::new(route),
        replica: tokio::sync::Mutex::new(ReplicaSlot::Unknown),
        failovers: AtomicU64::new(0),
    }));
    Ok(client)
}

/// Test Connection for any Redis profile: connects like the query lane and,
/// on a Sentinel profile, says which master answered and which sentinel
/// named it (and the replica reads would use, when Prefer replica for reads
/// is on and one is healthy). `None` for a direct profile.
pub async fn test_connection_report(profile: &ConnectionProfile) -> StudioResult<Option<String>> {
    let client = super::open_client(profile).await?;
    let Some(session) = client.sentinel.clone() else {
        return Ok(None);
    };
    if session.config.prefer_replica_reads {
        let _ = session.replica_for(&client).await;
    }
    Ok(Some(session.route().summary()))
}

/// The `host:port` the live tools (Profiler, Pub/Sub) dial: the current
/// master on a Sentinel profile, the tunnel endpoint otherwise.
pub(crate) async fn data_endpoint(profile: &ConnectionProfile) -> StudioResult<(String, u16)> {
    let Some(config) = profile.redis_sentinel.as_ref() else {
        return crate::ssh::TunnelManager::global()
            .endpoint_for(profile)
            .await;
    };
    let (client, _) = resolve_and_connect(profile, config).await?;
    let endpoint = client.endpoint();
    split_endpoint(&endpoint).ok_or_else(|| {
        StudioError::Connection(format!("Sentinel named an unusable address {endpoint}"))
    })
}

fn split_endpoint(endpoint: &str) -> Option<(String, u16)> {
    let (host, port) = endpoint.rsplit_once(':')?;
    Some((host.to_string(), port.parse().ok()?))
}

fn refuse_transport(profile: &ConnectionProfile) -> StudioResult<()> {
    if profile.inline_transport().is_empty() && profile.tunnel_profile.is_none() {
        return Ok(());
    }
    Err(StudioError::CapabilityNotSupported(
        "Sentinel connections dial the sentinels and the master directly; SSH tunnels, proxies \
         and Kubernetes port-forwards are not supported with Sentinel yet"
            .into(),
    ))
}

/// The Sentinel password, when the profile names one.
fn sentinel_password(config: &RedisSentinelConfig) -> StudioResult<Option<String>> {
    match &config.sentinel_password_ref {
        None => Ok(None),
        Some(r) if r.provider == SecretProvider::InMemory && r.key.is_empty() => Ok(None),
        Some(r) => Ok(Some(r.resolve()?).filter(|p| !p.is_empty())),
    }
}

/// Connection info for one sentinel: the profile's TLS mode, the Sentinel
/// password, no database and no ACL user.
fn sentinel_info(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> StudioResult<ConnectionInfo> {
    let addr = super::connection_addr(profile, host, port)?;
    let mut settings = RedisConnectionInfo::default();
    if let Some(password) = password {
        settings = settings.set_password(password);
    }
    Ok(addr
        .into_connection_info()
        .map_err(super::redis_error)?
        .set_redis_settings(settings))
}

fn ask_timeout(profile: &ConnectionProfile) -> Duration {
    profile.timeout().min(SENTINEL_ASK_TIMEOUT)
}

/// One sentinel's answer for the master name: `Ok(None)` when it does not
/// know the name.
async fn ask_sentinel(
    profile: &ConnectionProfile,
    info: ConnectionInfo,
    master_name: &str,
) -> StudioResult<Option<(String, u16)>> {
    let timeout = ask_timeout(profile);
    let client = redis::Client::open(info).map_err(super::redis_error)?;
    let config = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(Some(timeout));
    let mut conn = tokio::time::timeout(
        timeout,
        client.get_multiplexed_async_connection_with_config(&config),
    )
    .await
    .map_err(|_| StudioError::Connection(format!("timed out after {timeout:?}")))?
    .map_err(super::redis_error)?;
    let mut cmd = redis::cmd("SENTINEL");
    cmd.arg("GET-MASTER-ADDR-BY-NAME").arg(master_name);
    let reply = cmd
        .query_async::<Value>(&mut conn)
        .await
        .map_err(super::redis_error)?;
    parse_master_addr(reply)
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

/// `SENTINEL GET-MASTER-ADDR-BY-NAME` answers `[host, port]`, or nil when
/// the sentinel does not monitor that name.
fn parse_master_addr(reply: Value) -> StudioResult<Option<(String, u16)>> {
    match reply {
        Value::Nil => Ok(None),
        Value::Array(items) if items.is_empty() => Ok(None),
        Value::Array(items) if items.len() == 2 => {
            let host = value_text(&items[0]);
            let port = value_text(&items[1]).and_then(|p| p.parse::<u16>().ok());
            match (host, port) {
                (Some(host), Some(port)) if !host.is_empty() => Ok(Some((host, port))),
                _ => Err(StudioError::QueryExecution(
                    "the sentinel answered an unreadable master address".into(),
                )),
            }
        }
        other => Err(StudioError::QueryExecution(format!(
            "the sentinel answered an unexpected reply: {other:?}"
        ))),
    }
}

/// The first element of a `ROLE` reply (`master`, `slave`, `sentinel`).
async fn role_of(client: &RedisClient) -> StudioResult<String> {
    let reply = client.run(&redis::cmd("ROLE")).await?;
    match reply {
        Value::Array(items) => items.first().and_then(value_text).ok_or_else(|| {
            StudioError::QueryExecutionMessage(sqlly_core::DisplayMessage::new(
                "services.redis.errors.role_no_response",
                "Query execution error: ROLE answered nothing",
            ))
        }),
        other => Err(StudioError::QueryExecution(format!(
            "ROLE answered an unexpected reply: {other:?}"
        ))),
    }
}

fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Ask the sentinels in order, connect to the first master that confirms
/// its role, and say who named it.
async fn resolve_and_connect(
    profile: &ConnectionProfile,
    config: &RedisSentinelConfig,
) -> StudioResult<(RedisClient, SentinelRoute)> {
    refuse_transport(profile)?;
    if let Some(problem) = config.problem() {
        return Err(StudioError::InvalidProfile(problem));
    }
    let sentinels = config
        .sentinel_addresses()
        .map_err(StudioError::InvalidProfile)?;
    let password = sentinel_password(config)?;
    let master_name = config.master_name.trim();
    let mut failures = Vec::new();
    let mut unknown = 0usize;
    for (host, port) in &sentinels {
        let sentinel = host_port(host, *port);
        let info = sentinel_info(profile, host, *port, password.as_deref())?;
        let (master_host, master_port) = match ask_sentinel(profile, info, master_name).await {
            Ok(Some(addr)) => addr,
            Ok(None) => {
                unknown += 1;
                failures.push(format!("{sentinel}: does not know '{master_name}'"));
                continue;
            }
            Err(StudioError::Authentication(text)) => {
                failures.push(format!("{sentinel}: sentinel sign-in failed ({text})"));
                continue;
            }
            Err(e) => {
                failures.push(format!("{sentinel}: {}", error_text(&e)));
                continue;
            }
        };
        let master = host_port(&master_host, master_port);
        let client = match super::open_client_at(profile, &master_host, master_port).await {
            Ok(client) => client,
            // Every sentinel names the same master: a sign-in failure there
            // is the profile's user/password, not the sentinel's.
            Err(e @ StudioError::Authentication(_)) => return Err(e),
            Err(e) => {
                failures.push(format!(
                    "{sentinel} named {master}, which did not answer: {}",
                    error_text(&e)
                ));
                continue;
            }
        };
        match role_of(&client).await {
            Ok(role) if role.eq_ignore_ascii_case("master") => {}
            Ok(role) => {
                failures.push(format!(
                    "{sentinel} named {master}, but it is a {role} (a failover may be in progress)"
                ));
                continue;
            }
            // ROLE refused by ACL: trust the sentinel.
            Err(StudioError::PermissionDenied(_)) => {}
            Err(e) => {
                failures.push(format!("{sentinel} named {master}: {}", error_text(&e)));
                continue;
            }
        }
        let route = SentinelRoute {
            master_name: master_name.to_string(),
            master,
            sentinel,
            replica: None,
        };
        return Ok((client, route));
    }
    if unknown == sentinels.len() {
        return Err(StudioError::InvalidProfile(format!(
            "none of the sentinels monitors a master named '{master_name}'"
        )));
    }
    Err(StudioError::Connection(format!(
        "no sentinel could name a reachable master for '{master_name}': {}",
        failures.join("; ")
    )))
}

/// An error's text without its kind prefix (keyed `*Message` errors
/// included), for the `; `-joined per-sentinel failure list.
fn error_text(error: &StudioError) -> String {
    error.detail()
}

/// The crate's TLS mode for the data nodes it checks during replica
/// discovery, following the profile's.
fn node_tls_mode(profile: &ConnectionProfile) -> Option<redis::TlsMode> {
    match profile.effective_tls_mode() {
        TlsMode::Disable | TlsMode::Legacy => None,
        TlsMode::Require => Some(redis::TlsMode::Insecure),
        TlsMode::VerifyCa | TlsMode::VerifyFull => Some(redis::TlsMode::Secure),
    }
}

/// Find a healthy replica through the `redis` crate's Sentinel support (it
/// asks the sentinels for `SENTINEL REPLICAS`, drops the ones flagged down
/// and checks each one's `ROLE`), then open our own client on the first one
/// that answers, so TLS material, timeouts and the startup script match the
/// master's. Shares the master's Cancel.
async fn open_replica(
    profile: &ConnectionProfile,
    config: &RedisSentinelConfig,
    master: &RedisClient,
) -> Option<RedisClient> {
    let password = sentinel_password(config).ok()?;
    let infos = config
        .sentinel_addresses()
        .ok()?
        .iter()
        .map(|(host, port)| sentinel_info(profile, host, *port, password.as_deref()))
        .collect::<StudioResult<Vec<_>>>()
        .ok()?;
    let (user, data_password) = super::credentials(profile).ok()?;
    let mut node = RedisConnectionInfo::default();
    if let Some(user) = user {
        node = node.set_username(user);
    }
    if let Some(password) = data_password {
        node = node.set_password(password);
    }
    let mut node_info =
        redis::sentinel::SentinelNodeConnectionInfo::default().set_redis_connection_info(node);
    if let Some(mode) = node_tls_mode(profile) {
        node_info = node_info.set_tls_mode(mode);
    }
    let mut sentinel = redis::sentinel::Sentinel::build(infos).ok()?;
    let clients = tokio::time::timeout(
        profile.timeout(),
        sentinel.async_get_replica_clients(config.master_name.trim(), Some(&node_info)),
    )
    .await
    .ok()?
    .ok()?;
    for candidate in clients {
        let (host, port) = match candidate.get_connection_info().addr() {
            redis::ConnectionAddr::Tcp(host, port) => (host.clone(), *port),
            redis::ConnectionAddr::TcpTls { host, port, .. } => (host.clone(), *port),
            _ => continue,
        };
        let Ok(mut replica) = super::open_client_at(profile, &host, port).await else {
            continue;
        };
        // Cluster replicas need READONLY to serve reads; a non-cluster
        // replica serves them anyway and refuses the command, which is fine.
        let _ = replica.run(&redis::cmd("READONLY")).await;
        replica.cancel = master.cancel.clone();
        return Some(replica);
    }
    None
}

// ---------------------------------------------------------------------------
// Script routing and failover
// ---------------------------------------------------------------------------

/// Where one script runs and whether it already followed a failover.
#[derive(Default)]
pub(super) struct ScriptRoute {
    read_only: bool,
    replica: Option<Arc<RedisClient>>,
    failed_over: bool,
}

/// `SQLLY.*` commands that only read keys, so a replica answers them as
/// well as the master. The others describe the server and stay on the
/// master.
const REPLICA_VIRTUAL_COMMANDS: &[&str] = &[
    "SQLLY.KEYS",
    "SQLLY.KEYINFO",
    "SQLLY.HPAGE",
    "SQLLY.LPAGE",
    "SQLLY.SPAGE",
    "SQLLY.ZPAGE",
    "SQLLY.XPAGE",
    "SQLLY.STRPAGE",
];

/// Every command is read-only by the shared classifier.
pub fn script_is_read_only(commands: &[RedisCommand]) -> bool {
    !commands.is_empty() && commands.iter().all(|c| command_access(c).is_read_only())
}

/// A read-only script a replica can answer: no `SELECT` (the replica's
/// database must stay the master's) and no server-describing `SQLLY.*`.
pub fn replica_eligible(commands: &[RedisCommand]) -> bool {
    script_is_read_only(commands)
        && commands.iter().all(|c| {
            let name = c.name();
            name != "SELECT"
                && (!name.starts_with("SQLLY.")
                    || REPLICA_VIRTUAL_COMMANDS.contains(&name.as_str()))
        })
}

/// Plan a script: a no-op for direct profiles; on Sentinel profiles,
/// classify it and pick the replica when Prefer replica for reads allows.
pub(super) async fn route_script(client: &RedisClient, commands: &[RedisCommand]) -> ScriptRoute {
    let Some(session) = client.sentinel.as_ref() else {
        return ScriptRoute::default();
    };
    let read_only = script_is_read_only(commands);
    let replica = if session.config.prefer_replica_reads && replica_eligible(commands) {
        session.replica_for(client).await
    } else {
        None
    };
    ScriptRoute {
        read_only,
        replica,
        failed_over: false,
    }
}

/// A connection loss or a demoted master: the errors that mean "ask the
/// sentinels again".
pub fn is_failover_error(error: &StudioError) -> bool {
    match error {
        StudioError::Connection(_) | StudioError::ConnectionMessage(_) => true,
        StudioError::QueryExecution(text) => {
            let text = strip_line_prefix(text);
            ["READONLY", "MASTERDOWN", "LOADING"]
                .iter()
                .any(|code| text.starts_with(code))
                || text.starts_with("UNBLOCKED")
        }
        _ => false,
    }
}

fn strip_line_prefix(text: &str) -> &str {
    if let Some(rest) = text.strip_prefix("line ")
        && let Some((digits, tail)) = rest.split_once(": ")
        && digits.chars().all(|c| c.is_ascii_digit())
    {
        return tail;
    }
    text
}

/// The error a write script gets after a failover: never retried.
pub fn master_changed_error(
    line: usize,
    before: &str,
    route: &SentinelRoute,
    cause: &StudioError,
) -> StudioError {
    let what = if route.master == before {
        format!(
            "the connection to master {} was lost and has been re-established",
            route.master
        )
    } else {
        format!(
            "Sentinel {} now names {} as the master of '{}' (was {before})",
            route.sentinel, route.master, route.master_name
        )
    };
    StudioError::QueryExecution(format!(
        "line {line}: master changed, re-run — {what}. The script writes, so it was not retried \
         (it may have stopped part-way; check before re-running). Cause: {}",
        error_text(cause)
    ))
}

/// Run one command of a planned script, following a failover at most once.
pub(super) async fn run_in_script(
    client: &RedisClient,
    route: &mut ScriptRoute,
    command: &RedisCommand,
) -> StudioResult<Shaped> {
    let target = route.replica.as_deref().unwrap_or(client);
    let mut error = match super::run_command(target, command).await {
        Ok(shaped) => return Ok(shaped),
        Err(error) => error,
    };
    let Some(session) = client.sentinel.as_ref() else {
        return Err(error);
    };
    if !is_failover_error(&error) {
        return Err(error);
    }
    if route.replica.take().is_some() {
        // The replica went away: the master serves the rest of the script.
        session.forget_replica().await;
        match super::run_command(client, command).await {
            Ok(shaped) => return Ok(shaped),
            Err(e) if is_failover_error(&e) => error = e,
            Err(e) => return Err(e),
        }
    }
    if route.failed_over {
        return Err(error);
    }
    route.failed_over = true;
    let before = session.route().master;
    session.failover(client).await?;
    if route.read_only {
        return super::run_command(client, command).await;
    }
    Err(master_changed_error(
        command.line,
        &before,
        &session.route(),
        &error,
    ))
}

// ---------------------------------------------------------------------------
// Profiles from a spec (live tests, CLI)
// ---------------------------------------------------------------------------

/// A Sentinel profile from `host:port[,host:port…]/master[/db]`, the shape
/// `SQLLY_REDIS_SENTINEL` uses (`127.0.0.1:26379/mymaster`).
pub fn profile_from_spec(name: &str, spec: &str) -> StudioResult<ConnectionProfile> {
    let spec = spec.trim();
    let (hosts, path) = spec.split_once('/').ok_or_else(|| {
        StudioError::InvalidProfile(format!(
            "a Sentinel spec is host:port/master, not \"{spec}\""
        ))
    })?;
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let master_name = segments.next().unwrap_or_default().to_string();
    let database = segments.next().map(str::to_string);
    let config = RedisSentinelConfig {
        master_name,
        sentinels: sqlly_core::redis_sentinel::split_sentinel_list(hosts),
        sentinel_password_ref: None,
        prefer_replica_reads: false,
    };
    if let Some(problem) = config.problem() {
        return Err(StudioError::InvalidProfile(problem));
    }
    let (first_host, first_port) = config
        .sentinel_addresses()
        .map_err(StudioError::InvalidProfile)?[0]
        .clone();
    let mut profile = ConnectionProfile::sql_login(
        name,
        first_host,
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Redis;
    profile.port = first_port;
    profile.database = database;
    profile.encrypt = false;
    profile.trust_server_certificate = false;
    profile.timeout_seconds = profile.timeout_seconds.max(30);
    profile.redis_sentinel = Some(config);
    Ok(profile)
}

/// The live-test Sentinel profile from `SQLLY_REDIS_SENTINEL`, when set.
pub fn live_profile_from_env(name: &str) -> Option<ConnectionProfile> {
    let spec = std::env::var("SQLLY_REDIS_SENTINEL").ok()?;
    match profile_from_spec(name, &spec) {
        Ok(profile) => Some(profile),
        Err(error) => {
            eprintln!("SQLLY_REDIS_SENTINEL is not usable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_intel::query::safety::redis::parse_script;

    fn commands(text: &str) -> Vec<RedisCommand> {
        parse_script(text).expect("parse")
    }

    #[test]
    fn error_text_drops_the_kind_prefix_from_keyed_errors() {
        assert_eq!(
            error_text(&super::super::Refusal::Hello.into_error()),
            "the server refused both RESP3 and RESP2 handshakes"
        );
        assert_eq!(
            error_text(&StudioError::QueryExecutionMessage(
                sqlly_core::DisplayMessage::raw("Query execution error: ROLE answered nothing")
            )),
            "ROLE answered nothing"
        );
        assert_eq!(
            error_text(&StudioError::Connection("refused".into())),
            "refused"
        );
    }

    #[test]
    fn master_addr_reply_parses_or_says_unknown() {
        let reply = Value::Array(vec![
            Value::BulkString(b"10.0.0.5".to_vec()),
            Value::BulkString(b"6390".to_vec()),
        ]);
        assert_eq!(
            parse_master_addr(reply).unwrap(),
            Some(("10.0.0.5".to_string(), 6390))
        );
        assert_eq!(parse_master_addr(Value::Nil).unwrap(), None);
        assert!(
            parse_master_addr(Value::Array(vec![
                Value::BulkString(b"h".to_vec()),
                Value::BulkString(b"port".to_vec()),
            ]))
            .is_err()
        );
    }

    #[test]
    fn failover_errors_are_connection_loss_and_demotion_only() {
        assert!(is_failover_error(&StudioError::Connection(
            "broken pipe".into()
        )));
        assert!(is_failover_error(&StudioError::QueryExecution(
            "line 2: READONLY You can't write against a read only replica.".into()
        )));
        assert!(is_failover_error(&StudioError::QueryExecution(
            "MASTERDOWN Link with MASTER is down".into()
        )));
        assert!(!is_failover_error(&StudioError::QueryExecution(
            "line 1: WRONGTYPE Operation against a key holding the wrong kind of value".into()
        )));
        assert!(!is_failover_error(&StudioError::QueryCancelled));
        assert!(!is_failover_error(&StudioError::Authentication(
            "WRONGPASS".into()
        )));
    }

    #[test]
    fn read_only_scripts_and_replica_eligibility() {
        assert!(script_is_read_only(&commands("GET a\nHGETALL h")));
        assert!(!script_is_read_only(&commands("GET a\nSET a 1")));
        assert!(!script_is_read_only(&commands("DEL a")));
        assert!(!script_is_read_only(&[]));
        assert!(replica_eligible(&commands("GET a\nSQLLY.KEYS user:* 100")));
        // SELECT and server-describing virtual commands stay on the master.
        assert!(!replica_eligible(&commands("SELECT 2\nGET a")));
        assert!(!replica_eligible(&commands("SQLLY.SERVERINFO")));
        assert!(!replica_eligible(&commands("SET a 1")));
    }

    #[test]
    fn master_changed_message_names_the_new_master_and_never_promises_a_retry() {
        let route = SentinelRoute {
            master_name: "mymaster".into(),
            master: "127.0.0.1:6391".into(),
            sentinel: "127.0.0.1:26379".into(),
            replica: None,
        };
        let StudioError::QueryExecution(text) = master_changed_error(
            3,
            "127.0.0.1:6390",
            &route,
            &StudioError::QueryExecution("READONLY You can't write".into()),
        ) else {
            panic!("query error expected");
        };
        assert!(text.starts_with("line 3: master changed, re-run"), "{text}");
        assert!(text.contains("127.0.0.1:6391"), "{text}");
        assert!(text.contains("was 127.0.0.1:6390"), "{text}");
        assert!(text.contains("not retried"), "{text}");
        let StudioError::QueryExecution(same) = master_changed_error(
            1,
            "127.0.0.1:6391",
            &route,
            &StudioError::Connection("reset".into()),
        ) else {
            panic!("query error expected");
        };
        assert!(same.contains("master changed, re-run"));
        assert!(same.contains("re-established"), "{same}");
    }

    #[test]
    fn spec_builds_a_sentinel_profile() {
        let profile = profile_from_spec("s", "127.0.0.1:26379,127.0.0.1:26380/mymaster/3").unwrap();
        let config = profile.redis_sentinel.as_ref().unwrap();
        assert_eq!(config.master_name, "mymaster");
        assert_eq!(config.sentinels.len(), 2);
        assert_eq!(profile.server, "127.0.0.1");
        assert_eq!(profile.port, 26379);
        assert_eq!(profile.database.as_deref(), Some("3"));
        assert!(profile_from_spec("s", "127.0.0.1:26379").is_err());
        assert!(profile_from_spec("s", "127.0.0.1:26379/").is_err());
    }

    #[test]
    fn route_summary_names_master_and_sentinel() {
        let route = SentinelRoute {
            master_name: "mymaster".into(),
            master: "10.0.0.5:6379".into(),
            sentinel: "10.0.0.1:26379".into(),
            replica: Some("10.0.0.6:6379".into()),
        };
        assert_eq!(
            route.summary(),
            "Sentinel 10.0.0.1:26379 named 10.0.0.5:6379 as the master of 'mymaster'; \
             read-only scripts use replica 10.0.0.6:6379"
        );
    }

    #[tokio::test]
    async fn tunnelled_sentinel_profiles_are_refused_before_dialing() {
        let mut profile = profile_from_spec("s", "127.0.0.1:1/mymaster").unwrap();
        profile.transport.proxy = Some(sqlly_core::ProxyHop {
            kind: sqlly_core::ProxyKind::Socks5,
            host: "127.0.0.1".into(),
            port: 1080,
            username: None,
            password: None,
        });
        let error = super::super::open_client(&profile)
            .await
            .err()
            .expect("refused");
        assert!(
            matches!(&error, StudioError::CapabilityNotSupported(t) if t.contains("Sentinel")),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn unreachable_sentinels_are_all_named_in_the_error() {
        let mut profile = profile_from_spec("s", "127.0.0.1:1,127.0.0.1:2/mymaster").unwrap();
        profile.timeout_seconds = 2;
        let error = super::super::open_client(&profile)
            .await
            .err()
            .expect("no sentinel");
        let text = error.to_string();
        assert!(text.contains("127.0.0.1:1"), "{text}");
        assert!(text.contains("127.0.0.1:2"), "{text}");
    }

    // -----------------------------------------------------------------------
    // Live: SQLLY_REDIS_SENTINEL=127.0.0.1:26379/mymaster against a local
    // Sentinel deployment
    // -----------------------------------------------------------------------

    /// The live tests share one fixture and one of them fails it over, so
    /// they run one at a time.
    static LIVE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    fn live() -> Option<ConnectionProfile> {
        let profile = live_profile_from_env("sentinel-live");
        if profile.is_none() {
            eprintln!("SQLLY_REDIS_SENTINEL not set; skipping the live Sentinel test");
        }
        profile
    }

    /// A raw connection to a sentinel or data node (no SQLLY policy).
    async fn raw(addr: &str) -> redis::aio::MultiplexedConnection {
        let client = redis::Client::open(format!("redis://{addr}")).unwrap();
        client.get_multiplexed_async_connection().await.unwrap()
    }

    async fn sentinel_master(sentinel: &str, name: &str) -> String {
        let mut conn = raw(sentinel).await;
        let reply: Value = redis::cmd("SENTINEL")
            .arg("GET-MASTER-ADDR-BY-NAME")
            .arg(name)
            .query_async(&mut conn)
            .await
            .unwrap();
        let (host, port) = parse_master_addr(reply).unwrap().unwrap();
        host_port(&host, port)
    }

    async fn role(addr: &str) -> String {
        let mut conn = raw(addr).await;
        let reply: Value = redis::cmd("ROLE").query_async(&mut conn).await.unwrap();
        match reply {
            Value::Array(items) => value_text(&items[0]).unwrap(),
            other => panic!("ROLE: {other:?}"),
        }
    }

    async fn run_script(client: &RedisClient, text: &str) -> StudioResult<MaterializedResult> {
        super::super::query_rows_on_client(client, text)
            .await
            .map(|rs| MaterializedResult { rows: rs.rows })
    }

    struct MaterializedResult {
        rows: Vec<Vec<sqlly_core::CellValue>>,
    }

    /// Any cell of the result reads `text`.
    fn mentions(result: &MaterializedResult, text: &str) -> bool {
        result.rows.iter().flatten().any(|cell| match cell {
            sqlly_core::CellValue::String(s) => s == text,
            sqlly_core::CellValue::Bytes(b) => b == text.as_bytes(),
            _ => false,
        })
    }

    fn first_text(result: &MaterializedResult) -> String {
        match &result.rows[0][0] {
            sqlly_core::CellValue::String(s) => s.clone(),
            sqlly_core::CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn live_resolves_the_master_and_reports_the_sentinel() {
        let Some(profile) = live() else { return };
        let _serial = LIVE.lock().await;
        let config = profile.redis_sentinel.clone().unwrap();
        let first_sentinel = config.sentinels[0].clone();
        let expected = sentinel_master(&first_sentinel, &config.master_name).await;
        let client = super::super::open_client(&profile).await.expect("open");
        let found = route(&client).expect("sentinel route");
        assert_eq!(found.master, expected);
        assert_eq!(found.sentinel, first_sentinel);
        assert_eq!(client.endpoint(), expected);
        let report = test_connection_report(&profile).await.unwrap().unwrap();
        assert!(report.contains(&expected), "{report}");
        assert!(report.contains(&first_sentinel), "{report}");
        // The staged connection test traces the master the sentinels name
        // (not the sentinel the server field holds) and opens the session.
        let mut lines = Vec::new();
        let mut sink = |r: &super::super::diagnose::StageReport| lines.push(r.line());
        let trace = super::super::diagnose::trace_connection(&profile, &mut sink).await;
        assert!(trace.failed().is_none(), "{lines:#?}");
        assert!(
            lines[0].starts_with("Resolve ok") && lines[0].contains(&expected),
            "{lines:#?}"
        );
        assert!(lines.iter().any(|l| l.starts_with("AUTH ok")), "{lines:#?}");
        assert!(
            lines.last().is_some_and(|l| l.starts_with("Session ok")),
            "{lines:#?}"
        );

        // A dead first sentinel is skipped and the next one answers.
        let mut skipping = profile.clone();
        skipping.timeout_seconds = 3;
        skipping.redis_sentinel.as_mut().unwrap().sentinels = vec![
            "127.0.0.1:1".into(),
            first_sentinel.replace("26379", "26380"),
        ];
        let client = super::super::open_client(&skipping).await.expect("open");
        assert_eq!(route(&client).unwrap().sentinel, "127.0.0.1:26380");

        // An unknown master name is a profile problem, not a connection one.
        let mut unknown = profile.clone();
        unknown.redis_sentinel.as_mut().unwrap().master_name = "nosuchmaster".into();
        let error = super::super::open_client(&unknown).await.err().unwrap();
        assert!(
            matches!(&error, StudioError::InvalidProfile(t) if t.contains("nosuchmaster")),
            "{error:?}"
        );
    }

    /// A `redis+sentinel://` URL — pasted, or provisioned from an
    /// environment variable — reaches the master the sentinels name.
    #[tokio::test]
    async fn live_sentinel_url_and_provisioned_connection_reach_the_master() {
        let Some(profile) = live() else { return };
        let _serial = LIVE.lock().await;
        let config = profile.redis_sentinel.clone().unwrap();
        let expected = sentinel_master(&config.sentinels[0], &config.master_name).await;
        let url = format!(
            "redis+sentinel://{},127.0.0.1:1/{}/12",
            config.sentinels.join(","),
            config.master_name
        );

        let pasted = super::super::profile_from_url("sentinel-url", &url).expect("url");
        let client = super::super::open_client(&pasted).await.expect("open");
        assert_eq!(client.endpoint(), expected);
        let reply = run_script(&client, "SET sqlly:provisioned ok\nGET sqlly:provisioned")
            .await
            .expect("set/get");
        assert!(mentions(&reply, "ok"));

        let vars: std::collections::BTreeMap<String, String> = [
            ("SENTINEL_LIST", config.sentinels.join(",")),
            (
                "SQLLY_CONNECTION_FLEET",
                format!(
                    "redis+sentinel://${{SENTINEL_LIST}}/{}/12",
                    config.master_name
                ),
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let provisioned = sqlly_core::provisioning::provision(None, &vars);
        assert!(
            provisioned.warnings.is_empty(),
            "{:?}",
            provisioned.warnings
        );
        let profile = &provisioned.connections[0].profile;
        let client = super::super::open_client(profile).await.expect("open");
        assert_eq!(client.endpoint(), expected);
        let reply = run_script(&client, "GET sqlly:provisioned")
            .await
            .expect("get");
        assert_eq!(first_text(&reply), "ok");
    }

    #[tokio::test]
    async fn live_prefer_replica_routes_reads_to_a_replica_and_writes_to_the_master() {
        let Some(mut profile) = live() else { return };
        let _serial = LIVE.lock().await;
        profile.database = Some("11".into());
        profile
            .redis_sentinel
            .as_mut()
            .unwrap()
            .prefer_replica_reads = true;
        let client = super::super::open_client(&profile).await.expect("open");
        let master = client.endpoint();
        run_script(&client, "SET wsc:replica-read hello")
            .await
            .expect("write on master");
        // The write went to the master: the master holds it.
        let mut on_master = raw(&master).await;
        let _: Value = redis::cmd("SELECT")
            .arg(11)
            .query_async(&mut on_master)
            .await
            .unwrap();
        let held: Option<String> = redis::cmd("GET")
            .arg("wsc:replica-read")
            .query_async(&mut on_master)
            .await
            .unwrap();
        assert_eq!(held.as_deref(), Some("hello"));
        // Read through the replica until replication catches up (a replica
        // demoted by the failover test may still be resyncing).
        let started = Instant::now();
        loop {
            let read = run_script(&client, "GET wsc:replica-read")
                .await
                .expect("read");
            if first_text(&read) == "hello" {
                break;
            }
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "the replica never caught up"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let route = route(&client).unwrap();
        let replica = route.replica.clone().expect("a replica served the read");
        assert_ne!(replica, master);
        assert_eq!(role(&replica).await, "slave");
        // A read through the replica answers what the replica is.
        let read = run_script(&client, "ROLE").await.expect("role");
        assert!(mentions(&read, "slave"), "the replica answered ROLE");
        // A script with any write is a write script: all of it stays on the
        // master (query_rows_on_client returns the last result, ROLE's).
        let mixed = run_script(&client, "SET wsc:replica-read hello\nROLE")
            .await
            .unwrap();
        assert!(mentions(&mixed, "master"), "the master answered ROLE");
        run_script(&client, "DEL wsc:replica-read")
            .await
            .expect("cleanup");
    }

    /// Run with `--test-threads=1` or alone: it fails the fixture over.
    #[tokio::test]
    async fn live_failover_retries_reads_and_refuses_to_retry_writes() {
        let Some(mut profile) = live() else { return };
        let _serial = LIVE.lock().await;
        profile.database = Some("12".into());
        let config = profile.redis_sentinel.clone().unwrap();
        let sentinel = config.sentinels[0].clone();
        let reader = super::super::open_client(&profile).await.expect("reader");
        let writer = super::super::open_client(&profile).await.expect("writer");
        let old_master = reader.endpoint();
        run_script(&writer, "SET wsc:failover before")
            .await
            .expect("write");
        let reader_id = match reader
            .run(&redis::cmd("CLIENT").arg("ID").clone())
            .await
            .unwrap()
        {
            Value::Int(id) => id,
            other => panic!("CLIENT ID: {other:?}"),
        };

        // Fail over and wait until the sentinel names the other node and the
        // old master has been demoted.
        let mut sentinel_conn = raw(&sentinel).await;
        let _: Value = redis::cmd("SENTINEL")
            .arg("FAILOVER")
            .arg(&config.master_name)
            .query_async(&mut sentinel_conn)
            .await
            .expect("SENTINEL FAILOVER");
        let started = Instant::now();
        let new_master = loop {
            let now = sentinel_master(&sentinel, &config.master_name).await;
            if now != old_master
                && role(&old_master).await == "slave"
                && role(&now).await == "master"
            {
                break now;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "failover never finished"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        };

        // Read-only: drop the reader's connection so the read must
        // re-resolve; it succeeds on the new master without the user.
        let mut old = raw(&old_master).await;
        let _: Result<Value, _> = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(reader_id)
            .query_async(&mut old)
            .await;
        let read = run_script(&reader, "GET wsc:failover")
            .await
            .expect("read after failover");
        assert_eq!(first_text(&read), "before");
        assert_eq!(reader.endpoint(), new_master);
        assert_eq!(failovers(&reader), 1);

        // Write: the writer is still on the demoted master. The first run
        // fails with the master-changed message and is not retried …
        let error = run_script(&writer, "SET wsc:failover after")
            .await
            .err()
            .expect("a write is never retried");
        let text = error.to_string();
        assert!(text.contains("master changed, re-run"), "{text}");
        assert!(text.contains(&new_master), "{text}");
        assert_eq!(failovers(&writer), 1);
        let mut on_new = raw(&new_master).await;
        let _: Value = redis::cmd("SELECT")
            .arg(12)
            .query_async(&mut on_new)
            .await
            .unwrap();
        let held: Option<String> = redis::cmd("GET")
            .arg("wsc:failover")
            .query_async(&mut on_new)
            .await
            .unwrap();
        assert_eq!(
            held.as_deref(),
            Some("before"),
            "the refused write did not land"
        );
        // … and the re-run succeeds on the new master.
        run_script(&writer, "SET wsc:failover after")
            .await
            .expect("re-run");
        assert_eq!(writer.endpoint(), new_master);
        let held: Option<String> = redis::cmd("GET")
            .arg("wsc:failover")
            .query_async(&mut on_new)
            .await
            .unwrap();
        assert_eq!(held.as_deref(), Some("after"));
        run_script(&writer, "DEL wsc:failover")
            .await
            .expect("cleanup");
    }

    fn entra_jwt(oid: &str, exp: i64) -> String {
        use base64::Engine as _;
        let enc = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!(
            "{}.{}.sig",
            enc.encode(br#"{"alg":"none"}"#),
            enc.encode(serde_json::json!({"oid": oid, "exp": exp}).to_string())
        )
    }

    /// The master's replicas from `ROLE` (`host:port`).
    async fn replicas_of(master: &str) -> Vec<String> {
        let mut conn = raw(master).await;
        let reply: Value = redis::cmd("ROLE").query_async(&mut conn).await.unwrap();
        let Value::Array(items) = reply else {
            return Vec::new();
        };
        let Some(Value::Array(replicas)) = items.get(2) else {
            return Vec::new();
        };
        replicas
            .iter()
            .filter_map(|r| match r {
                Value::Array(parts) => Some(format!(
                    "{}:{}",
                    value_text(parts.first()?)?,
                    value_text(parts.get(1)?)?
                )),
                _ => None,
            })
            .collect()
    }

    async fn on_nodes(nodes: &[String], args: &[&str]) {
        for node in nodes {
            let mut conn = raw(node).await;
            let mut cmd = redis::cmd(args[0]);
            for arg in &args[1..] {
                cmd.arg(*arg);
            }
            let _ = cmd.query_async::<Value>(&mut conn).await;
        }
    }

    /// Entra on a Sentinel profile, with an ACL user on the master and the
    /// replica standing in for the Azure identity: the master and the
    /// replica sign in with the token's oid, a newer token re-authenticates
    /// both, and once the old token is revoked and the user's connections
    /// are killed, the re-resolved master (the failover path) signs in with
    /// the newest token rather than the one the session started with.
    #[tokio::test]
    async fn live_sentinel_entra_reauth_covers_master_replica_and_failover() {
        let Some(mut profile) = live() else { return };
        let _serial = LIVE.lock().await;
        let config = profile.redis_sentinel.clone().unwrap();
        let master = sentinel_master(&config.sentinels[0], &config.master_name).await;
        let mut nodes = vec![master.clone()];
        nodes.extend(replicas_of(&master).await);
        let user = format!("wsk_entra_sentinel_{}", std::process::id());
        let first = entra_jwt(&user, 4_000_000_000);
        let second = entra_jwt(&user, 4_000_000_100);
        let grant_first = format!(">{first}");
        let grant_second = format!(">{second}");
        on_nodes(
            &nodes,
            &[
                "ACL",
                "SETUSER",
                &user,
                "on",
                &grant_first,
                &grant_second,
                "~wsk:*",
                "+@all",
            ],
        )
        .await;
        profile.authentication = sqlly_core::AuthenticationType::EntraBrowser;
        profile.runtime_entra_access_token = Some(first.clone());
        profile
            .redis_sentinel
            .as_mut()
            .unwrap()
            .prefer_replica_reads = true;
        let client = super::super::open_client(&profile)
            .await
            .expect("entra open");
        assert_eq!(client.entra_token().as_deref(), Some(first.as_str()));
        // A read-only script runs on the replica, signed in as the user.
        let whoami = run_script(&client, "ACL WHOAMI").await.expect("whoami");
        assert!(mentions(&whoami, &user));
        // A newer token re-authenticates the master; the replica follows it
        // on its next use.
        assert!(client.refresh_entra(Some(&second)).await.expect("reauth"));
        let whoami = run_script(&client, "ACL WHOAMI")
            .await
            .expect("whoami after");
        assert!(mentions(&whoami, &user));
        // Revoke the first token and kill the user's connections: the read
        // is retried on the re-resolved master, which must sign in with the
        // newest token.
        on_nodes(
            &nodes,
            &["ACL", "SETUSER", &user, "resetpass", &grant_second],
        )
        .await;
        on_nodes(&nodes, &["CLIENT", "KILL", "USER", &user]).await;
        let read = run_script(&client, "EXISTS wsk:entra:none")
            .await
            .expect("read after the old token was revoked");
        assert_eq!(read.rows.len(), 1);
        assert_eq!(client.entra_token().as_deref(), Some(second.as_str()));
        drop(client);
        on_nodes(&nodes, &["ACL", "DELUSER", &user]).await;
    }
}
