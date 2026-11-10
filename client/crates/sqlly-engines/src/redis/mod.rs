//! # Redis driver
//!
//! Redis (and the servers that speak its protocol: Valkey, KeyDB,
//! DragonflyDB, Amazon ElastiCache and MemoryDB) over RESP through the
//! `redis` crate, mirroring the other drivers' shape: [`open_client`] /
//! [`execute_on_client`] for the streaming path, [`RedisTransport`]
//! implementing `crate::driver::SqlTransport` for the lifecycle service.
//!
//! Redis is not SQL. The "query" is a script of one command per line in
//! `redis-cli` syntax (or a JSON array of arguments per line), parsed and
//! classified by `sqlly_intel::query::safety::redis`. Each line runs in
//! order on one connection and answers with one result set shaped by
//! [`shape`]: scalars as a one-row `result` column, arrays one row per
//! element, hashes and scored sets as pairs, `INFO` / `CLIENT LIST` /
//! `SLOWLOG GET` as tables. A server error ends the script with Redis's own
//! text, prefixed by the line. `SELECT n` switches the connection's logical
//! database for the rest of the script (and reports the change on the
//! event bus). Commands that would block the connection forever
//! (`SUBSCRIBE`, `MONITOR`, `BLPOP … 0`, …) are refused before they are
//! sent. Cancel releases the running command and drops the connection.
//!
//! The connection policy is Redis-aware: the SELECT-only lock and the
//! AI/MCP read-only mode both mean "read-only commands only" from the
//! shared allowlist; the rollback wrap is refused because there is nothing
//! to roll back.
//!
//! A small family of `SQLLY.*` virtual commands answers the tools that need
//! composed information — `SQLLY.KEYS pattern [limit [cursor]]` (the key
//! browser's bounded `SCAN` walk with types and TTLs, reporting the cursor
//! a further walk continues from), `SQLLY.DATABASES` (logical databases with key
//! counts and a bounded memory sample), `SQLLY.MEMBYTYPE` / `SQLLY.TOPKEYS` /
//! `SQLLY.TTLBUCKETS` / `SQLLY.MEMBYPREFIX` / `SQLLY.MEMSUMMARY` (the
//! memory analyzer's reports,
//! all derived from one bounded sample per script run), `SQLLY.DASHBOARD <group>`, `SQLLY.WAITS`, `SQLLY.BLOCKING`,
//! `SQLLY.SERVERINFO`, `SQLLY.INDEXES` (the search module's indexes with
//! their key type, prefixes and document counts), `SQLLY.COMMANDS` (the server's `COMMAND DOCS` and
//! `COMMAND INFO` as rows, which feed command IntelliSense), `SQLLY.KEYINFO <key>` (the key view's header: type,
//! TTL, encoding, memory, length) and the key view's paged readers
//! `SQLLY.HPAGE` / `LPAGE` / `SPAGE` / `ZPAGE` / `XPAGE` / `STRPAGE` (one
//! page plus `next` and `total` columns; see [`page`]), the stream
//! tabs' `SQLLY.XGROUPS <key>` / `SQLLY.XPENDING <key> <group>` (see
//! [`streams`]), and `SQLLY.DUMPSCRIPT pattern [limit]` (a folder exported
//! as an importable command file, binary-safe; see [`dumpscript`]). They
//! only ever run read-only commands and are classified as read-only.
//!
//! TLS follows the profile's [`TlsMode`]: `Require` encrypts without
//! verification, `VerifyCa` checks the chain (the CA file when set) but not
//! the host name, `VerifyFull` both; a client certificate/key pair enables
//! mutual TLS. SSH tunnels, proxies and Kubernetes port-forwards come from
//! the shared [`crate::ssh::TunnelManager`], exactly like the socket
//! drivers.

#![forbid(unsafe_code)]

pub mod analyze;
pub mod browse;
pub mod cluster;
pub mod diagnose;
pub mod dumpscript;
pub mod entra;
pub mod hello;
pub mod info;
pub mod keyinfo;
pub mod live;
pub mod on_node;
pub mod page;
pub mod sentinel;
pub mod setname;
pub mod shape;
pub mod streams;
pub mod timeout;
pub mod trust;

use std::sync::Arc;

use redis::{
    AsyncConnectionConfig, ConnectionAddr, IntoConnectionInfo, ProtocolVersion,
    RedisConnectionInfo, Value,
};
use sqlly_core::{
    AuthenticationType, CellValue, ConnectionProfile, DatabaseSummary, EventBus, EventPayload,
    MaterializedResultSet, RowSender, SecretProvider, SecretRef, StudioError, StudioResult,
    TlsMode,
};
use sqlly_intel::query::safety::redis::{
    RedisCommand, apply_redis_policy, blocking_reason, parse_script,
};
use tokio_util::sync::CancellationToken;

use crate::driver::QueryStreamer;
pub use crate::driver::{ConnectionService, SqlTransport};
use shape::{Shaped, shape_reply};

/// The default port.
pub const DEFAULT_PORT: u16 = 6379;

/// A live Redis connection: one multiplexed connection (or a slot-routed
/// cluster connection, see [`cluster`]) plus the profile's bits the tools
/// need. Dropping it closes the socket(s).
pub struct RedisClient {
    /// Swappable so a Sentinel failover can move the client to the new
    /// master in place (see [`sentinel`]); every other client keeps the one
    /// connection it opened with. A cluster client holds the slot-routed
    /// connection (see [`cluster`]), which is never swapped.
    /// Shared with the Entra re-authentication task (see [`entra`]), which
    /// re-authenticates whatever connection the client holds at the time.
    conn: Arc<parking_lot::Mutex<cluster::RedisConn>>,
    endpoint: parking_lot::Mutex<String>,
    /// Set on Sentinel profiles: the route, the replica, and failover state.
    sentinel: Option<Arc<sentinel::SentinelSession>>,
    /// The logical database the connection was opened on.
    pub database: i64,
    current_database: Arc<parking_lot::Mutex<i64>>,
    cancel: CancellationToken,
    /// The memory analyzer's per-run sample (see [`analyze::AnalysisCache`]).
    analysis_cache: analyze::AnalysisCache,
    /// The protocol the connection negotiated (see [`hello`]).
    protocol: ProtocolVersion,
    /// Whether the connection is encrypted.
    tls: bool,
    /// How long one command may wait for its reply (see [`timeout`]).
    response_timeout: Option<std::time::Duration>,
    /// The connection's tunnel had dropped and was dialed again for it.
    tunnel_reestablished: bool,
    /// Entra sign-in (Azure Redis): the login the connection is on now, so
    /// a newer token re-authenticates it before the current one expires.
    entra: Option<Arc<parking_lot::Mutex<entra::EntraLogin>>>,
    /// Stops the Entra re-authentication task when the client is dropped.
    _entra_keeper: Option<tokio_util::sync::DropGuard>,
    /// The profile a cluster rebuild re-opens with a fresh token (set with
    /// the Entra session; unused otherwise).
    entra_profile: Option<Arc<ConnectionProfile>>,
}

/// The note a run carries when its connection needed a new tunnel.
pub const TUNNEL_REESTABLISHED_NOTE: &str = "Tunnel re-established: the SSH session had \
     dropped, so a new tunnel was opened for this run. Nothing was re-run; if a command was \
     running when the tunnel dropped, check whether it took effect before running it again.";

pub fn tunnel_reestablished_message() -> sqlly_core::DisplayMessage {
    sqlly_core::DisplayMessage::new(
        "services.redis.tunnel_reestablished",
        TUNNEL_REESTABLISHED_NOTE,
    )
}

impl RedisClient {
    /// Whether this connection's tunnel had dropped and was dialed again to
    /// open it (see [`crate::ssh::keepalive`]).
    pub fn tunnel_reestablished(&self) -> bool {
        self.tunnel_reestablished
    }

    /// The protocol the connection speaks: RESP3 when the server accepted
    /// `HELLO 3`, RESP2 otherwise.
    pub fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    /// `host:port` the client dialed (after tunnelling; the current master
    /// on a Sentinel profile).
    pub fn endpoint(&self) -> String {
        self.endpoint.lock().clone()
    }

    /// The logical database the connection is on now (after any `SELECT`).
    pub fn current_database(&self) -> i64 {
        *self.current_database.lock()
    }

    /// Run one command, racing Cancel and the connection's command timeout
    /// (see [`timeout`]).
    pub async fn run(&self, cmd: &redis::Cmd) -> StudioResult<Value> {
        let mut conn = self.conn.lock().clone();
        let args: Vec<Vec<u8>> = cmd
            .args_iter()
            .filter_map(|arg| match arg {
                redis::Arg::Simple(bytes) => Some(bytes.to_vec()),
                _ => None,
            })
            .collect();
        let budget = timeout::budget(self.response_timeout, &args);
        let reply = async {
            match budget {
                Some(limit) => tokio::time::timeout(limit, cmd.query_async::<Value>(&mut conn))
                    .await
                    .map_err(|_| timeout::timeout_error(limit, &timeout::command_label(&args)))?
                    .map_err(redis_error),
                None => cmd
                    .query_async::<Value>(&mut conn)
                    .await
                    .map_err(redis_error),
            }
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(StudioError::QueryCancelled),
            reply = reply => reply,
        }
    }

    /// Run a pipeline, racing Cancel and the connection's command timeout.
    pub async fn run_pipeline(&self, pipe: &redis::Pipeline) -> StudioResult<Vec<Value>> {
        let mut conn = self.conn.lock().clone();
        let limit = self.response_timeout;
        let reply = async {
            match limit {
                Some(limit) => {
                    tokio::time::timeout(limit, pipe.query_async::<Vec<Value>>(&mut conn))
                        .await
                        .map_err(|_| {
                            timeout::timeout_error(
                                limit,
                                &format!("A pipeline of {} commands", pipe.len()),
                            )
                        })?
                        .map_err(redis_error)
                }
                None => pipe
                    .query_async::<Vec<Value>>(&mut conn)
                    .await
                    .map_err(redis_error),
            }
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(StudioError::QueryCancelled),
            reply = reply => reply,
        }
    }

    /// `SELECT db` on the connection, keeping the tracker in step.
    pub async fn select_database(&self, db: i64) -> StudioResult<()> {
        let mut cmd = redis::cmd("SELECT");
        cmd.arg(db);
        self.run(&cmd).await?;
        *self.current_database.lock() = db;
        Ok(())
    }

    /// Entra sign-in: re-authenticate the live connection with `token` when
    /// it is newer than the one the connection is on (`AUTH <oid> <token>`
    /// is allowed mid-session). Returns whether it re-authenticated; a
    /// connection without Entra sign-in ignores the call.
    pub async fn refresh_entra(&self, token: Option<&str>) -> StudioResult<bool> {
        let (Some(session), Some(token)) = (&self.entra, token) else {
            return Ok(false);
        };
        let user = {
            let current = session.lock();
            if !entra::is_newer_token(&current, token) {
                return Ok(false);
            }
            current.user.clone()
        };
        let Some(profile) = self.entra_profile.as_deref() else {
            return Ok(false);
        };
        reauthenticate(&self.conn, session, profile, &user, token).await?;
        Ok(true)
    }

    /// After a Sentinel failover moved `fresh`'s connection into this
    /// client: take over the Entra login it signed in with.
    pub(crate) fn adopt_entra_login(&self, fresh: &RedisClient) {
        if let (Some(mine), Some(theirs)) = (&self.entra, &fresh.entra) {
            let login = theirs.lock().clone();
            *mine.lock() = login;
        }
    }

    /// The Entra token the connection is signed in with now (`None` without
    /// Entra sign-in).
    pub fn entra_token(&self) -> Option<String> {
        self.entra.as_ref().map(|s| s.lock().token.clone())
    }

    /// `profile` carrying the newest Entra token this client knows (its own
    /// login, the app's published token, or the profile's), for opening a
    /// further connection for it: a Sentinel failover's new master or a
    /// replica. Unchanged without Entra sign-in.
    pub(crate) fn entra_fresh_profile(&self, profile: &ConnectionProfile) -> ConnectionProfile {
        entra::freshest_profile(profile, self.entra_token().as_deref())
    }

    /// Run a parsed command's raw arguments.
    pub async fn run_args(&self, args: &[Vec<u8>]) -> StudioResult<Value> {
        let Some((name, rest)) = args.split_first() else {
            return Err(StudioError::InvalidInputMessage(
                sqlly_core::DisplayMessage::new(
                    "services.redis.errors.empty_command",
                    "empty command",
                ),
            ));
        };
        let mut cmd = redis::cmd(&String::from_utf8_lossy(name));
        for arg in rest {
            cmd.arg(arg.as_slice());
        }
        self.run(&cmd).await
    }
}

/// Re-authenticate the connection the client holds now with `token` and
/// record the new login. A standalone, Sentinel master (also after a
/// failover swapped it) or replica connection sends `AUTH user token`
/// mid-session. A cluster connection is rebuilt with the fresh token and
/// swapped in (see [`cluster::rebuild_with_token`]): the client library
/// signs every node connection in with the password the routed client was
/// built with, and reconnects to a node reuse it, so an `AUTH` sent to the
/// nodes would be undone by the next reconnect.
async fn reauthenticate(
    conn: &parking_lot::Mutex<cluster::RedisConn>,
    session: &parking_lot::Mutex<entra::EntraLogin>,
    profile: &ConnectionProfile,
    user: &str,
    token: &str,
) -> StudioResult<()> {
    let current = conn.lock().clone();
    match current {
        cluster::RedisConn::Single(mut single) => {
            let mut auth = redis::cmd("AUTH");
            auth.arg(user).arg(token.trim());
            auth.query_async::<Value>(&mut single)
                .await
                .map_err(redis_error)?;
        }
        cluster::RedisConn::Cluster(handle) => {
            let fresh = cluster::rebuild_with_token(&handle, profile, token.trim()).await?;
            *conn.lock() = cluster::RedisConn::Cluster(fresh);
        }
    }
    let mut current = session.lock();
    current.token = token.trim().to_string();
    current.expires_at = entra::token_claims(token).exp;
    Ok(())
}

/// Keep an Entra-signed connection signed in: shortly before its token
/// expires, re-authenticate with the newest token the app published for
/// the profile (the app re-mints on every connect and run). Ends when the
/// client is dropped.
fn spawn_entra_keeper(
    conn: Arc<parking_lot::Mutex<cluster::RedisConn>>,
    session: Arc<parking_lot::Mutex<entra::EntraLogin>>,
    profile: ConnectionProfile,
) -> tokio_util::sync::DropGuard {
    let profile_id = profile.id.0;
    let stop = CancellationToken::new();
    let guard = stop.clone().drop_guard();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
            let (user, due) = {
                let current = session.lock();
                (
                    current.user.clone(),
                    entra::reauth_due(current.expires_at, entra::now_unix()),
                )
            };
            if !due {
                continue;
            }
            let Some(token) = entra::published_token(profile_id) else {
                continue;
            };
            if !entra::is_newer_token(&session.lock(), &token) {
                continue;
            }
            if let Err(error) = reauthenticate(&conn, &session, &profile, &user, &token).await {
                tracing::warn!(%error, "Redis Entra re-authentication failed");
            }
        }
    });
    guard
}

/// Attach the Entra session (and its re-authentication task) to a client
/// opened for an Entra profile; other profiles pass through unchanged.
pub(crate) fn with_entra_session(
    mut client: RedisClient,
    profile: &ConnectionProfile,
) -> StudioResult<RedisClient> {
    if !entra::uses_entra(profile) {
        return Ok(client);
    }
    let session = Arc::new(parking_lot::Mutex::new(entra::entra_login(profile)?));
    client._entra_keeper = Some(spawn_entra_keeper(
        client.conn.clone(),
        session.clone(),
        profile.clone(),
    ));
    client.entra_profile = Some(Arc::new(profile.clone()));
    client.entra = Some(session);
    Ok(client)
}

/// Cancellation handle: releases the driver loop; the producer then drops
/// the client, which closes the connection.
#[derive(Clone)]
pub struct RedisCancelHandle {
    cancel: CancellationToken,
}

impl RedisCancelHandle {
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

/// Obtain a cancellation handle for the client.
pub fn cancel_handle(client: &RedisClient) -> RedisCancelHandle {
    RedisCancelHandle {
        cancel: client.cancel.clone(),
    }
}

/// Map the crate's error onto the studio's: AUTH failures, connection
/// trouble, and the server's own `-ERR` text otherwise.
pub(crate) fn redis_error(error: redis::RedisError) -> StudioError {
    let text = match (error.code(), error.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        (Some(code), None) => code.to_string(),
        _ => error.to_string(),
    };
    if error.kind() == redis::ErrorKind::AuthenticationFailed
        || matches!(error.code(), Some("NOAUTH" | "WRONGPASS"))
    {
        return StudioError::Authentication(text);
    }
    if error.is_connection_refusal()
        || error.is_connection_dropped()
        || error.is_io_error()
        || error.is_timeout()
    {
        return StudioError::Connection(text);
    }
    if error.code() == Some("NOPERM") {
        return StudioError::PermissionDenied(text);
    }
    StudioError::QueryExecution(text)
}

/// The ACL user (blank means the `default` user, sent as no user at all)
/// and password (blank means none) the driver authenticates with.
fn credentials(profile: &ConnectionProfile) -> StudioResult<(Option<String>, Option<String>)> {
    match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => {
            let user = Some(username.trim().to_string()).filter(|u| !u.is_empty());
            let password =
                if secret_ref.key.is_empty() && secret_ref.provider == SecretProvider::InMemory {
                    None
                } else {
                    Some(secret_ref.resolve()?).filter(|p| !p.is_empty())
                };
            Ok((user, password))
        }
        AuthenticationType::Integrated => Err(StudioError::NotImplemented(
            "Integrated authentication is not supported for Redis connections",
        )),
        // Azure Managed Redis / Azure Cache for Redis: the identity's object
        // id signs in with an Entra access token as the password.
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            let login = entra::entra_login(profile)?;
            Ok((Some(login.user), Some(login.token)))
        }
        // ElastiCache / MemoryDB for Valkey with IAM authentication: the
        // IAM-enabled user id signs in with a freshly minted SigV4 token as
        // the password (15-minute TTL, minted per connect by the caller).
        AuthenticationType::AwsRdsIam { username, .. } => {
            // ElastiCache / MemoryDB only accept IAM tokens over TLS; refusing
            // here (every connect path — standalone, cluster, sentinel —
            // resolves credentials through this function) also keeps a
            // bearer token from ever crossing the wire in plaintext.
            if profile.effective_tls_mode() == TlsMode::Disable {
                return Err(StudioError::InvalidProfile(
                    "AWS IAM sign-in for ElastiCache / MemoryDB requires TLS; turn on encryption for this connection"
                        .into(),
                ));
            }
            let user = username.trim();
            if user.is_empty() {
                return Err(StudioError::InvalidProfile(
                    "AWS IAM for Redis needs the IAM-enabled user id".into(),
                ));
            }
            let token = profile
                .runtime_cloud_iam_token
                .clone()
                .filter(|t| !t.is_empty())
                .ok_or_else(|| {
                    StudioError::Authentication(
                        "missing AWS IAM token for Redis; the token is minted at connect time — reconnect to mint one"
                            .into(),
                    )
                })?;
            Ok((Some(user.to_string()), Some(token)))
        }
        AuthenticationType::GcpCloudSqlIam { .. } => Err(StudioError::InvalidProfile(
            "Google Cloud IAM sign-in applies to Cloud SQL (PostgreSQL and MySQL), not Redis"
                .into(),
        )),
        AuthenticationType::AuthToken { .. } => Err(StudioError::InvalidProfile(
            "Auth tokens apply to libSQL / Turso; Redis signs in with an ACL user and password"
                .into(),
        )),
    }
}

/// The logical database index a profile's `database` names: blank is 0,
/// anything but digits is refused.
pub fn database_index(profile: &ConnectionProfile) -> StudioResult<i64> {
    let text = profile.database.as_deref().map(str::trim).unwrap_or("");
    if text.is_empty() {
        return Ok(0);
    }
    text.parse::<i64>().ok().filter(|n| *n >= 0).ok_or_else(|| {
        StudioError::InvalidProfile(format!(
            "A Redis database is a number (0, 1, 2, …), not \"{text}\""
        ))
    })
}

fn read_pem(path: &std::path::Path, what: &str) -> StudioResult<Vec<u8>> {
    std::fs::read(path).map_err(|e| {
        StudioError::InvalidProfile(format!(
            "cannot read the {what} file {}: {e}",
            path.display()
        ))
    })
}

/// The connection address for the profile's TLS mode.
fn connection_addr(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
) -> StudioResult<ConnectionAddr> {
    let host = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = if port == 0 { DEFAULT_PORT } else { port };
    let mode = profile.effective_tls_mode();
    if mode == TlsMode::Disable {
        return Ok(ConnectionAddr::Tcp(host, port));
    }
    let mut addr = ConnectionAddr::TcpTls {
        host,
        port,
        insecure: mode == TlsMode::Require,
        tls_params: None,
    };
    if mode == TlsMode::VerifyCa {
        addr.set_danger_accept_invalid_hostnames(true);
    }
    Ok(addr)
}

/// Build the RESP2 `redis::Client` for a profile (the live tools' dedicated
/// connections): address, credentials, database and (when the profile names
/// them) the CA bundle and client certificate.
fn build_client(profile: &ConnectionProfile, host: &str, port: u16) -> StudioResult<redis::Client> {
    build_client_with(profile, host, port, ProtocolVersion::RESP2)
}

/// [`build_client`] for a chosen protocol. RESP3 sends `HELLO 3` with the
/// credentials in place of `AUTH`.
fn build_client_with(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    protocol: ProtocolVersion,
) -> StudioResult<redis::Client> {
    let (user, password) = credentials(profile)?;
    let db = database_index(profile)?;
    let addr = connection_addr(profile, host, port)?;
    let tls = matches!(addr, ConnectionAddr::TcpTls { .. });
    let mut settings = RedisConnectionInfo::default()
        .set_db(db)
        .set_protocol(protocol)
        .set_lib_name("SQLLY", env!("CARGO_PKG_VERSION"));
    if let Some(user) = user {
        settings = settings.set_username(user);
    }
    if let Some(password) = password {
        settings = settings.set_password(password);
    }
    let info = addr
        .into_connection_info()
        .map_err(redis_error)?
        .set_redis_settings(settings);
    // Roots only matter when the mode verifies; see [`trust`].
    let verifying = matches!(
        profile.effective_tls_mode(),
        TlsMode::VerifyCa | TlsMode::VerifyFull
    );
    let root_cert = if tls && (verifying || profile.tls_ca_file.is_some()) {
        trust::root_material(profile)?.0
    } else {
        None
    };
    let needs_material = tls && (root_cert.is_some() || profile.tls_client_cert_file.is_some());
    if needs_material {
        let client_tls = match (&profile.tls_client_cert_file, &profile.tls_client_key_file) {
            (Some(cert), Some(key)) => Some(redis::ClientTlsConfig {
                client_cert: read_pem(cert, "client certificate")?,
                client_key: read_pem(key, "client key")?,
            }),
            (Some(_), None) => {
                return Err(StudioError::InvalidProfile(
                    "a client certificate needs its key file".into(),
                ));
            }
            _ => None,
        };
        redis::Client::build_with_tls(
            info,
            redis::TlsCertificates {
                client_tls,
                root_cert,
            },
        )
        .map_err(redis_error)
    } else {
        redis::Client::open(info).map_err(redis_error)
    }
}

/// Open a connection for the profile and verify it with `PING`. Asks for
/// RESP3 first and falls back to RESP2 without a word when the server
/// refuses `HELLO 3` (see [`hello`]).
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<RedisClient> {
    // A Sentinel profile asks the sentinels for the master first.
    if let Some(config) = profile.redis_sentinel.as_ref() {
        return sentinel::open_client(profile, config).await;
    }
    let tunnels = crate::ssh::TunnelManager::global();
    // A cluster profile (or `Auto` known to be one) opens the routed client.
    let mut client = match cluster::open_if_cluster(profile).await? {
        Some(routed) => routed,
        None => {
            let first_host = cluster::single_host(profile);
            let single = first_host.as_ref();
            let (host, port) = tunnels.endpoint_for(single).await?;
            open_client_at(single, &host, port).await?
        }
    };
    // A tunnel (the connection's own, or a cluster node's) that had dropped
    // and was dialed again for this connection: its runs say so.
    client.tunnel_reestablished = tunnels.take_reestablished(&profile.id);
    Ok(client)
}

/// [`open_client`] against an already-resolved `host:port` (Sentinel uses it
/// for the master and the replica). Asks for RESP3 first and falls back to
/// RESP2 without a word when the server refuses `HELLO 3` (see [`hello`]).
async fn open_client_at(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
) -> StudioResult<RedisClient> {
    let client = match connect(profile, host, port, ProtocolVersion::RESP3).await {
        Err(Refusal::Hello) => connect(profile, host, port, ProtocolVersion::RESP2).await,
        other => other,
    }
    .map_err(Refusal::into_error)?;
    // Entra (Azure Redis): keep the connection signed in as tokens rotate.
    let client = with_entra_session(client, profile)?;
    setname::name_connection(&client, profile).await;
    client.run(&redis::cmd("PING")).await.map_err(|e| match e {
        StudioError::QueryExecution(text) => StudioError::Connection(text),
        other => other,
    })?;
    // `Auto` asks a fresh standalone connection whether it is a cluster
    // node (a Sentinel master never is, so it is not asked).
    if profile.redis_sentinel.is_none()
        && let Some(routed) = cluster::switch_if_cluster(&client, profile).await?
    {
        return Ok(routed);
    }
    apply_startup_script(&client, profile).await?;
    Ok(client)
}

/// The `host:port` a Verify-full certificate must name when the connection
/// dials `host` through the profile's tunnel or proxy: the profile's own
/// server, unless the dialed host already is that server. `None` for every
/// other mode (Require does not verify, Verify CA skips the host name) and
/// for a direct connection.
fn tunneled_tls_name(profile: &ConnectionProfile, host: &str) -> Option<(String, u16)> {
    if profile.effective_tls_mode() != TlsMode::VerifyFull || profile.inline_transport().is_empty()
    {
        return None;
    }
    let server = profile
        .server
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let dialed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if server.is_empty() || server.eq_ignore_ascii_case(dialed) {
        return None;
    }
    let port = if profile.port == 0 {
        DEFAULT_PORT
    } else {
        profile.port
    };
    Some((server.to_string(), port))
}

/// Resolves every name to the tunnel's local end (see
/// [`tunneled_tls_name`]).
struct TunnelDial {
    host: String,
    port: u16,
}

impl redis::io::AsyncDNSResolver for TunnelDial {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        _host: &'b str,
        _port: u16,
    ) -> redis::RedisFuture<'a, Box<dyn Iterator<Item = std::net::SocketAddr> + Send + 'a>> {
        Box::pin(async move {
            let addrs: Vec<std::net::SocketAddr> =
                tokio::net::lookup_host((self.host.as_str(), self.port))
                    .await?
                    .collect();
            Ok(Box::new(addrs.into_iter())
                as Box<
                    dyn Iterator<Item = std::net::SocketAddr> + Send + 'a,
                >)
        })
    }
}

/// Why one connection attempt failed: the server would not speak the
/// protocol asked for, or anything else.
pub(crate) enum Refusal {
    Hello,
    Other(StudioError),
}

impl Refusal {
    pub(crate) fn into_error(self) -> StudioError {
        match self {
            Refusal::Hello => StudioError::ConnectionMessage(sqlly_core::DisplayMessage::new(
                "services.redis.errors.handshakes_refused",
                "Connection error: the server refused both RESP3 and RESP2 handshakes",
            )),
            Refusal::Other(error) => error,
        }
    }
}

impl From<StudioError> for Refusal {
    fn from(error: StudioError) -> Self {
        Refusal::Other(error)
    }
}

/// One connection attempt on `protocol`, without `PING` or startup text.
async fn connect(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    protocol: ProtocolVersion,
) -> Result<RedisClient, Refusal> {
    // Through a tunnel, Verify full checks the certificate against the
    // profile's server, not the tunnel's local end.
    connect_named(
        profile,
        host,
        port,
        protocol,
        tunneled_tls_name(profile, host),
    )
    .await
}

/// [`connect`] with an explicit TLS name: when `tls_name` is set, the
/// address names it (so the handshake sends and verifies it) and a fixed
/// resolver sends the socket to `host:port` instead.
async fn connect_named(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    protocol: ProtocolVersion,
    tls_name: Option<(String, u16)>,
) -> Result<RedisClient, Refusal> {
    let client = match &tls_name {
        Some((name, name_port)) => build_client_with(profile, name, *name_port, protocol)?,
        None => build_client_with(profile, host, port, protocol)?,
    };
    let tls = matches!(
        client.get_connection_info().addr(),
        ConnectionAddr::TcpTls { .. }
    );
    let timeout = profile.timeout();
    let mut config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(timeout))
        .set_response_timeout(None);
    if tls_name.is_some() {
        config = config.set_dns_resolver(TunnelDial {
            host: host
                .trim()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string(),
            port: if port == 0 { DEFAULT_PORT } else { port },
        });
    }
    let endpoint = format!("{host}:{}", if port == 0 { DEFAULT_PORT } else { port });
    let conn = tokio::time::timeout(
        timeout,
        client.get_multiplexed_async_connection_with_config(&config),
    )
    .await
    .map_err(|_| {
        StudioError::Connection(format!(
            "timed out connecting to {endpoint} after {timeout:?}"
        ))
    })?
    .map_err(|e| {
        if protocol.supports_resp3() && hello::hello_refused(&e) {
            return Refusal::Hello;
        }
        Refusal::Other(match redis_error(e) {
            StudioError::QueryExecution(text) => StudioError::Connection(text),
            other => other,
        })
    })?;
    let database = database_index(profile)?;
    Ok(RedisClient {
        conn: Arc::new(parking_lot::Mutex::new(cluster::RedisConn::Single(conn))),
        endpoint: parking_lot::Mutex::new(endpoint),
        sentinel: None,
        database,
        current_database: Arc::new(parking_lot::Mutex::new(database)),
        cancel: CancellationToken::new(),
        analysis_cache: analyze::AnalysisCache::default(),
        protocol,
        tls,
        response_timeout: timeout::response_timeout(profile),
        tunnel_reestablished: false,
        entra: None,
        _entra_keeper: None,
        entra_profile: None,
    })
}

/// The profile's startup text, run as a Redis script (one command per line).
async fn apply_startup_script(
    client: &RedisClient,
    profile: &ConnectionProfile,
) -> StudioResult<()> {
    let Some(text) = profile
        .startup_sql
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty())
    else {
        return Ok(());
    };
    let commands = parse_script(text).map_err(sqlly_core::startup_sql_failed)?;
    for command in commands {
        run_command(client, &command)
            .await
            .map_err(sqlly_core::startup_sql_failed)?;
    }
    Ok(())
}

/// Run one parsed command (refusing blockers) and shape its reply. Tracks
/// `SELECT` so the client knows its current database.
async fn run_command(client: &RedisClient, command: &RedisCommand) -> StudioResult<Shaped> {
    if let Some(reason) = blocking_reason(command) {
        return Err(StudioError::QueryExecution(format!(
            "line {}: {reason}",
            command.line
        )));
    }
    // `SQLLY.ON <target> <command …>` runs on the nodes it names, outside
    // slot routing, so the cluster's routing refusals do not apply to it.
    if let Some((target, inner)) = command.on_node() {
        return on_node::run(client, &target, &inner)
            .await
            .map_err(|e| prefix_line(command.line, e));
    }
    if client.is_cluster()
        && let Some(reason) = cluster::refusal(command)
    {
        return Err(StudioError::QueryExecution(format!(
            "line {}: {reason}",
            command.line
        )));
    }
    let name = command.name();
    if let Some(virtual_name) = name.strip_prefix("SQLLY.") {
        // Every failure names its line, like a real command's does.
        return virtual_command(client, virtual_name, command)
            .await
            .map_err(|e| prefix_line(command.line, e));
    }
    // A real command may change the keyspace: later analyzer reports in the
    // same script must resample rather than reuse a stale sample.
    client.analysis_cache.clear();
    let value = client
        .run_args(&command.args)
        .await
        .map_err(|e| prefix_line(command.line, e))?;
    if name == "SELECT"
        && let Ok(db) = command.arg_text(1).trim().parse::<i64>()
    {
        *client.current_database.lock() = db;
    }
    Ok(shape_reply(&command.args, value))
}

fn prefix_line(line: usize, error: StudioError) -> StudioError {
    match error {
        StudioError::QueryExecution(text) => {
            StudioError::QueryExecution(format!("line {line}: {text}"))
        }
        StudioError::PermissionDenied(text) => {
            StudioError::PermissionDenied(format!("line {line}: {text}"))
        }
        other => other,
    }
}

/// Materialized run of one command line (tools, probes, the key browser).
pub async fn query_rows_on_client(
    client: &RedisClient,
    text: &str,
) -> StudioResult<MaterializedResultSet> {
    let commands = parse_script(text)?;
    client.analysis_cache.clear();
    let mut route = sentinel::route_script(client, &commands).await;
    let mut last = None;
    for command in &commands {
        last = Some(sentinel::run_in_script(client, &mut route, command).await?);
    }
    let shaped = last.unwrap_or_else(|| Shaped::new(&[], Vec::new()));
    Ok(MaterializedResultSet {
        columns: shaped.columns,
        rows: shaped.rows,
    })
}

/// Run a script on an open client, streaming each command's result set
/// through the shared driver contract. Applies the Redis connection policy.
pub async fn execute_on_client(
    client: &RedisClient,
    profile: &ConnectionProfile,
    text: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    let commands = apply_redis_policy(profile, text)?;
    // Entra: a run that carries a newer token re-authenticates first.
    client
        .refresh_entra(profile.runtime_entra_access_token.as_deref())
        .await?;
    // Each run takes its own analyzer sample (shared by its reports).
    client.analysis_cache.clear();
    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    if client.tunnel_reestablished {
        streamer.message(TUNNEL_REESTABLISHED_NOTE)?;
    }
    let started_on = client.current_database();
    // Sentinel profiles: read-only scripts may run on a replica, and a
    // failover is handled per the no-duplicate-writes rule. Every other
    // profile runs each command on `client` exactly as before.
    let mut route = sentinel::route_script(client, &commands).await;
    let redirects_before = client.redirect_count();
    for command in &commands {
        if client.cancel.is_cancelled() {
            return Err(StudioError::QueryCancelled);
        }
        let shaped = sentinel::run_in_script(client, &mut route, command).await?;
        streamer.begin_result_set(shaped.columns).await?;
        for row in shaped.rows {
            if !streamer.push_row(row).await? {
                return streamer.finish().await;
            }
        }
    }
    if let Some(text) = cluster::redirect_message(client.redirect_count() - redirects_before) {
        streamer.message(text)?;
    }
    let now_on = client.current_database();
    if now_on != started_on {
        events.publish(EventPayload::DatabaseChanged {
            session_id: streamer.session_id().clone(),
            name: now_on.to_string(),
        })?;
    }
    streamer.finish().await
}

// ---------------------------------------------------------------------------
// Virtual commands
// ---------------------------------------------------------------------------

async fn virtual_command(
    client: &RedisClient,
    name: &str,
    command: &RedisCommand,
) -> StudioResult<Shaped> {
    match name {
        "KEYS" => {
            let pattern = command.arg_text(1);
            let pattern = if pattern.is_empty() {
                "*".to_string()
            } else {
                pattern
            };
            let limit = command
                .arg_text(2)
                .trim()
                .parse::<usize>()
                .unwrap_or(browse::DEFAULT_KEY_LIMIT);
            // An optional fourth argument continues an earlier walk from the
            // cursor it reported, so a listing extends without rescanning.
            // (A cluster cursor names its master: `host:port@cursor`.)
            let start_cursor = command.arg_text(3);
            let listing = browse::scan_keys_from(client, &pattern, limit, &start_cursor).await?;
            let complete = listing.complete;
            let cursor = listing.cursor;
            let rows = listing
                .keys
                .into_iter()
                .map(|k| {
                    vec![
                        shape::bulk_cell(k.key.clone()),
                        CellValue::String(k.kind),
                        k.ttl_seconds.map(CellValue::I64).unwrap_or(CellValue::Null),
                        CellValue::Bool(complete),
                        CellValue::String(cursor.clone()),
                    ]
                })
                .collect();
            Ok(cluster::with_master_count(
                client,
                Shaped::new(
                    &[
                        ("key", "string"),
                        ("type", "string"),
                        ("ttl_seconds", "integer"),
                        ("complete", "boolean"),
                        ("cursor", "string"),
                    ],
                    rows,
                ),
            ))
        }
        "DATABASES" => {
            let with_memory = command.subcommand() == "MEMORY";
            let sample = command
                .arg_text(2)
                .trim()
                .parse::<usize>()
                .unwrap_or(browse::DEFAULT_MEMORY_SAMPLE);
            databases_rows(client, with_memory, sample).await
        }
        "DASHBOARD" => {
            let group = command.arg_text(1).to_ascii_lowercase();
            let info = browse::info(client).await?;
            let rows = info::dashboard_metrics(&group, &info)
                .into_iter()
                .map(|(m, v)| vec![CellValue::String(m), CellValue::String(v)])
                .collect();
            Ok(Shaped::new(
                &[("metric", "string"), ("value", "string")],
                rows,
            ))
        }
        "WAITS" => {
            let info = browse::info(client).await?;
            let rows = info::wait_rows(&info)
                .into_iter()
                .map(|(k, v, d)| {
                    vec![
                        CellValue::String(k),
                        CellValue::I64(v),
                        CellValue::String(d),
                    ]
                })
                .collect();
            Ok(Shaped::new(
                &[
                    ("wait_type", "string"),
                    ("waiting_tasks_count", "integer"),
                    ("description", "string"),
                ],
                rows,
            ))
        }
        "BLOCKING" => Ok(Shaped::new(
            &[
                ("blocking_session_id", "integer"),
                ("blocked_session_id", "integer"),
                ("database_name", "string"),
                ("blocking_query", "string"),
                ("blocked_query", "string"),
            ],
            Vec::new(),
        )),
        "SERVERINFO" => server_info_row(client).await,
        "COMMANDS" => commands_rows(client).await,
        "MEMBYTYPE" => {
            let sample = command
                .arg_text(1)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_ANALYSIS_SAMPLE);
            let analysis = analyze::analysis_for_run(client, sample).await?;
            let rows = analyze::by_type(&analysis)
                .into_iter()
                .map(|t| {
                    vec![
                        CellValue::String(t.kind),
                        CellValue::I64(t.sampled_keys),
                        t.sampled_bytes
                            .map(CellValue::I64)
                            .unwrap_or(CellValue::Null),
                        t.estimated_bytes
                            .map(CellValue::I64)
                            .unwrap_or(CellValue::Null),
                        CellValue::F64(t.pct_of_sampled_bytes),
                    ]
                })
                .collect();
            Ok(Shaped::new(
                &[
                    ("type", "string"),
                    ("sampled_keys", "integer"),
                    ("sampled_bytes", "integer"),
                    ("estimated_bytes", "integer"),
                    ("pct_of_sampled_bytes", "double"),
                ],
                rows,
            ))
        }
        "TOPKEYS" => {
            let sample = command
                .arg_text(1)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_ANALYSIS_SAMPLE);
            let count = command
                .arg_text(2)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_TOP_KEYS);
            // No MEMORY USAGE (unknown command, NOPERM): an empty ranking,
            // not an error — the other reports still run, and
            // SQLLY.MEMSUMMARY carries the reason.
            let analysis = analyze::analysis_for_run(client, sample).await?;
            let rows = analyze::top_keys(&analysis, count)
                .into_iter()
                .enumerate()
                .map(|(i, k)| {
                    vec![
                        CellValue::I64((i + 1) as i64),
                        shape::bulk_cell(k.key.into_bytes()),
                        CellValue::String(k.kind),
                        k.ttl_seconds.map(CellValue::I64).unwrap_or(CellValue::Null),
                        CellValue::I64(k.bytes),
                    ]
                })
                .collect();
            Ok(Shaped::new(
                &[
                    ("rank", "integer"),
                    ("key", "string"),
                    ("type", "string"),
                    ("ttl_seconds", "integer"),
                    ("bytes", "integer"),
                ],
                rows,
            ))
        }
        "MEMBYPREFIX" => {
            // SQLLY.MEMBYPREFIX [sample [depth [delimiters]]]: memory and key
            // counts per key namespace over the run's shared sample.
            let sample = command
                .arg_text(1)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_ANALYSIS_SAMPLE);
            let depth = command.arg_text(2).trim().parse::<usize>().unwrap_or(1);
            let delimiters = command.arg_text(3);
            let analysis = analyze::analysis_for_run(client, sample).await?;
            let rows = analyze::by_prefix(&analysis, depth, &delimiters)
                .into_iter()
                .map(|p| {
                    let bytes = |v: Option<i64>| v.map(CellValue::I64).unwrap_or(CellValue::Null);
                    vec![
                        p.prefix.map(CellValue::String).unwrap_or(CellValue::Null),
                        CellValue::I64(p.sampled_keys),
                        CellValue::I64(p.estimated_keys),
                        bytes(p.sampled_bytes),
                        bytes(p.estimated_bytes),
                        CellValue::F64(p.pct_of_sampled_bytes),
                    ]
                })
                .collect();
            Ok(Shaped::new(
                &[
                    ("prefix", "string"),
                    ("sampled_keys", "integer"),
                    ("estimated_keys", "integer"),
                    ("sampled_bytes", "integer"),
                    ("estimated_bytes", "integer"),
                    ("pct_of_sampled_bytes", "double"),
                ],
                rows,
            ))
        }
        "TTLBUCKETS" => {
            let sample = command
                .arg_text(1)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_ANALYSIS_SAMPLE);
            let analysis = analyze::analysis_for_run(client, sample).await?;
            let rows = analyze::ttl_buckets(&analysis)
                .into_iter()
                .map(|b| {
                    vec![
                        CellValue::String(b.bucket.to_string()),
                        CellValue::I64(b.sampled_keys),
                        CellValue::I64(b.estimated_keys),
                        CellValue::F64(b.pct_of_sample),
                    ]
                })
                .collect();
            Ok(Shaped::new(
                &[
                    ("bucket", "string"),
                    ("sampled_keys", "integer"),
                    ("estimated_keys", "integer"),
                    ("pct_of_sample", "double"),
                ],
                rows,
            ))
        }
        "MEMSUMMARY" => {
            let sample = command
                .arg_text(1)
                .trim()
                .parse::<usize>()
                .unwrap_or(analyze::DEFAULT_ANALYSIS_SAMPLE);
            let analysis = analyze::analysis_for_run(client, sample).await?;
            Ok(Shaped::new(
                &[
                    ("database", "integer"),
                    ("sampled_keys", "integer"),
                    ("total_keys", "integer"),
                    ("complete", "boolean"),
                    ("vanished_keys", "integer"),
                    ("memory_measured", "boolean"),
                    ("memory_note", "string"),
                ],
                vec![vec![
                    CellValue::I64(client.current_database()),
                    CellValue::I64(analysis.keys.len() as i64),
                    CellValue::I64(analysis.total_keys),
                    CellValue::Bool(analysis.complete),
                    CellValue::I64(analysis.vanished as i64),
                    CellValue::Bool(!analysis.memory_unsupported()),
                    analysis
                        .memory_unavailable
                        .clone()
                        .map(|message| CellValue::String(message.into_fallback()))
                        .unwrap_or(CellValue::Null),
                ]],
            ))
        }
        "KEYINFO" => {
            let key = key_argument(command, "SQLLY.KEYINFO")?;
            let row = keyinfo::key_info(client, &key).await?;
            Ok(keyinfo::shape(&row))
        }
        "HPAGE" | "SPAGE" => {
            let key = key_argument(command, &format!("SQLLY.{name}"))?;
            let cursor = command.arg_text(2);
            let cursor = if cursor.trim().is_empty() {
                "0"
            } else {
                cursor.trim()
            };
            let count = page::page_size(command.arg_text(3).trim().parse().ok());
            if name == "HPAGE" {
                page::hash_page(client, &key, cursor, count).await
            } else {
                page::set_page(client, &key, cursor, count).await
            }
        }
        "LPAGE" | "ZPAGE" => {
            let key = key_argument(command, &format!("SQLLY.{name}"))?;
            let start = command.arg_text(2).trim().parse::<i64>().unwrap_or(0);
            let count = page::page_size(command.arg_text(3).trim().parse().ok());
            if name == "LPAGE" {
                page::list_page(client, &key, start, count).await
            } else {
                page::zset_page(client, &key, start, count).await
            }
        }
        "XPAGE" => {
            let key = key_argument(command, "SQLLY.XPAGE")?;
            let from = command.arg_text(2);
            let count = page::page_size(command.arg_text(3).trim().parse().ok());
            page::stream_page(client, &key, from.trim(), count).await
        }
        "STRPAGE" => {
            let key = key_argument(command, "SQLLY.STRPAGE")?;
            let offset = command.arg_text(2).trim().parse::<i64>().unwrap_or(0);
            let window = command
                .arg_text(3)
                .trim()
                .parse::<usize>()
                .unwrap_or(page::STRING_WINDOW_BYTES);
            page::string_page(client, &key, offset, window).await
        }
        "INDEXES" => indexes_rows(client).await,
        "XGROUPS" => {
            let key = key_argument(command, "SQLLY.XGROUPS")?;
            streams::groups(client, &key).await
        }
        "XPENDING" => {
            let key = key_argument(command, "SQLLY.XPENDING")?;
            let group = command.args.get(2).cloned().ok_or_else(|| {
                StudioError::QueryExecutionMessage(sqlly_core::DisplayMessage::new(
                    "services.redis.errors.xpending_arguments",
                    "Query execution error: SQLLY.XPENDING needs a key and a group",
                ))
            })?;
            let from = command.arg_text(3);
            let count = page::page_size(command.arg_text(4).trim().parse().ok());
            streams::pending(client, &key, &group, from.trim(), count).await
        }
        "TOPOLOGY" => cluster::topology_rows(client).await,
        "TRACE" => cluster::trace_rows(client).await,
        // A complete `SQLLY.ON <target> <command …>` never gets here (see
        // [`run_command`]); this is the line without a command to run.
        "ON" => Err(StudioError::QueryExecution(
            "SQLLY.ON needs a target and a command, such as SQLLY.ON masters DBSIZE or SQLLY.ON 10.0.0.1:7000 INFO replication".into(),
        )),
        // The export command file (see [`dumpscript`]): read-only.
        "DUMPSCRIPT" => {
            let pattern = command.arg_text(1);
            let pattern = if pattern.is_empty() {
                "*".to_string()
            } else {
                pattern
            };
            let limit = command
                .arg_text(2)
                .trim()
                .parse::<usize>()
                .unwrap_or(dumpscript::DEFAULT_KEY_LIMIT);
            dumpscript::dump_script(client, &pattern, limit).await
        }
        other => Err(StudioError::QueryExecution(format!(
            "unknown SQLLY.{other} command"
        ))),
    }
}

/// The key argument of a key-view virtual command, byte-exact.
fn key_argument(command: &RedisCommand, what: &str) -> StudioResult<Vec<u8>> {
    command
        .args
        .get(1)
        .cloned()
        .ok_or_else(|| StudioError::QueryExecution(format!("{what} needs a key")))
}

/// Indexes whose `FT.INFO` details `SQLLY.INDEXES` reads; later ones list
/// with empty detail columns so a server with thousands stays quick.
const INDEX_DETAIL_LIMIT: usize = 200;

/// `SQLLY.INDEXES`: the search module's indexes (`FT._LIST`) as rows of
/// `index, on (HASH | JSON), prefixes, num_docs`, the details read with one
/// `FT.INFO` per index. Fails exactly like `FT._LIST` on a server without
/// the module, so the key browser can treat the error as "no indexes here".
async fn indexes_rows(client: &RedisClient) -> StudioResult<Shaped> {
    let listed = client.run(&redis::cmd("FT._LIST")).await?;
    let names: Vec<Vec<u8>> = match listed {
        Value::Array(items) | Value::Set(items) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::BulkString(bytes) => Some(bytes),
                Value::SimpleString(text) => Some(text.into_bytes()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut rows = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        let (on, prefixes, docs) = if i < INDEX_DETAIL_LIMIT {
            let mut cmd = redis::cmd("FT.INFO");
            cmd.arg(name.as_slice());
            match client.run(&cmd).await {
                Ok(info) => index_details(&info),
                Err(_) => (CellValue::Null, CellValue::Null, CellValue::Null),
            }
        } else {
            (CellValue::Null, CellValue::Null, CellValue::Null)
        };
        rows.push(vec![shape::bulk_cell(name), on, prefixes, docs]);
    }
    Ok(Shaped::new(
        &[
            ("index", "string"),
            ("on", "string"),
            ("prefixes", "string"),
            ("num_docs", "integer"),
        ],
        rows,
    ))
}

/// Key type, space-joined prefixes and document count from an `FT.INFO`
/// reply (flat pairs under RESP2, a map under RESP3).
fn index_details(info: &Value) -> (CellValue, CellValue, CellValue) {
    let definition = shape::find_field(info, "index_definition");
    let on = definition
        .as_ref()
        .and_then(|d| shape::find_field(d, "key_type"))
        .map(shape::cell)
        .unwrap_or(CellValue::Null);
    let prefixes = definition
        .as_ref()
        .and_then(|d| shape::find_field(d, "prefixes"))
        .map(|p| match p {
            Value::Array(items) | Value::Set(items) => CellValue::String(
                items
                    .iter()
                    .filter_map(|item| match shape::cell(item.clone()) {
                        CellValue::String(s) => Some(s),
                        CellValue::Bytes(b) => Some(String::from_utf8_lossy(&b).into_owned()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            other => shape::cell(other),
        })
        .unwrap_or(CellValue::Null);
    let docs = match shape::find_field(info, "num_docs") {
        Some(Value::Int(n)) => CellValue::I64(n),
        Some(other) => match shape::cell(other) {
            CellValue::String(s) => s
                .parse::<i64>()
                .map(CellValue::I64)
                .unwrap_or(CellValue::Null),
            cell => cell,
        },
        None => CellValue::Null,
    };
    (on, prefixes, docs)
}

async fn databases_rows(
    client: &RedisClient,
    with_memory: bool,
    sample: usize,
) -> StudioResult<Shaped> {
    let browse::Databases {
        lines: databases,
        counts_known,
    } = browse::databases(client).await?;
    let mut columns = vec![
        ("database_name", "string"),
        ("keys", "integer"),
        ("expires", "integer"),
        ("avg_ttl_ms", "integer"),
    ];
    if with_memory {
        columns.extend([
            ("sampled_keys", "integer"),
            ("sampled_mb", "double"),
            ("estimated_mb", "double"),
            ("largest_key", "string"),
            ("largest_key_kb", "double"),
        ]);
    }
    let started_on = client.current_database();
    let mut rows = Vec::with_capacity(databases.len());
    for db in &databases {
        // Without INFO the counts are unknown: empty cells, not zeros.
        let count = |n: i64| {
            if counts_known {
                CellValue::I64(n)
            } else {
                CellValue::Null
            }
        };
        let mut row = vec![
            CellValue::String(db.db.to_string()),
            count(db.keys),
            count(db.expires),
            count(db.avg_ttl_ms),
        ];
        if with_memory {
            if !counts_known {
                row.extend(std::iter::repeat_n(CellValue::Null, 5));
            } else if db.keys > 0 {
                browse::select_database(client, db.db).await?;
                let memory = browse::sample_memory(client, sample).await?;
                if memory.unsupported || memory.sampled_keys == 0 {
                    row.extend([
                        CellValue::I64(0),
                        CellValue::Null,
                        CellValue::Null,
                        CellValue::Null,
                        CellValue::Null,
                    ]);
                } else {
                    let per_key = memory.sampled_bytes as f64 / memory.sampled_keys as f64;
                    row.extend([
                        CellValue::I64(memory.sampled_keys),
                        CellValue::F64(round2(memory.sampled_bytes as f64 / 1048576.0)),
                        CellValue::F64(round2(per_key * db.keys as f64 / 1048576.0)),
                        memory
                            .largest_key
                            .map(CellValue::String)
                            .unwrap_or(CellValue::Null),
                        CellValue::F64(round2(memory.largest_bytes as f64 / 1024.0)),
                    ]);
                }
            } else {
                row.extend([
                    CellValue::I64(0),
                    CellValue::F64(0.0),
                    CellValue::F64(0.0),
                    CellValue::Null,
                    CellValue::Null,
                ]);
            }
        }
        rows.push(row);
    }
    if with_memory && client.current_database() != started_on {
        browse::select_database(client, started_on).await?;
    }
    Ok(Shaped::new(&columns, rows))
}

fn round2(n: f64) -> f64 {
    (n * 100.0).round() / 100.0
}

/// The `server_info::probe_sql` row: product, version, charset, collation,
/// case flag (keys are case-sensitive), ACL user, database, TLS (whether
/// this connection is encrypted), and the negotiated protocol (`RESP3` /
/// `RESP2`) as a trailing column the shared probe parser ignores.
async fn server_info_row(client: &RedisClient) -> StudioResult<Shaped> {
    let info = browse::info(client).await?;
    let (product, version) = info::product_and_version(&info);
    let mut whoami = redis::cmd("ACL");
    whoami.arg("WHOAMI");
    let user = match client.run(&whoami).await {
        Ok(Value::BulkString(b)) => CellValue::String(String::from_utf8_lossy(&b).into_owned()),
        Ok(Value::SimpleString(s)) => CellValue::String(s),
        _ => CellValue::Null,
    };
    Ok(Shaped::new(
        &[
            ("product", "string"),
            ("version", "string"),
            ("charset", "string"),
            ("collation", "string"),
            ("case_flag", "integer"),
            ("current_user_name", "string"),
            ("database_name", "string"),
            ("tls", "integer"),
            ("protocol", "string"),
        ],
        vec![vec![
            CellValue::String(product),
            CellValue::String(version),
            CellValue::String("binary".to_string()),
            CellValue::Null,
            CellValue::I64(1),
            user,
            CellValue::String(client.current_database().to_string()),
            CellValue::I64(i64::from(client.tls)),
            CellValue::String(hello::protocol_label(client.protocol).to_string()),
        ]],
    ))
}

/// `SQLLY.COMMANDS`: the server's `COMMAND DOCS` and `COMMAND INFO` as one
/// row per command (`command`, `docs`, `info`; the last two as JSON in the
/// connection's own spelling), which feeds command IntelliSense. A refused
/// `COMMAND` (ACL) or a server without `COMMAND DOCS` (before 7.0) surfaces
/// the server's error, so the caller falls back to the bundled vocabulary.
async fn commands_rows(client: &RedisClient) -> StudioResult<Shaped> {
    let mut docs_cmd = redis::cmd("COMMAND");
    docs_cmd.arg("DOCS");
    let docs = client.run(&docs_cmd).await?;
    let mut info_cmd = redis::cmd("COMMAND");
    info_cmd.arg("INFO");
    let info = client.run(&info_cmd).await?;
    Ok(shape::command_docs_rows(docs, info))
}

/// Verify a profile connects, without keeping the client: the staged trace
/// of [`diagnose`] (resolve → tunnel → TCP → TLS → AUTH → HELLO / PING →
/// SELECT → CLUSTER INFO → session), failing with the failing stage as the
/// headline of the error.
pub async fn test_connection(profile: &ConnectionProfile) -> StudioResult<()> {
    diagnose::test_connection(profile).await
}

/// `SqlTransport` implementation for Redis, usable with the shared
/// [`ConnectionService`]. `list_databases` answers the logical databases
/// `0..databases` (from `CONFIG GET databases`, 16 when refused) with each
/// database's key count in the `state` column.
#[derive(Debug, Clone, Default)]
pub struct RedisTransport;

#[async_trait::async_trait]
impl SqlTransport for RedisTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        test_connection(profile).await
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        Ok(browse::databases(&client)
            .await?
            .lines
            .into_iter()
            .map(|db| DatabaseSummary {
                name: db.db.to_string(),
                state: "ONLINE".to_string(),
                compatibility_level: None,
            })
            .collect())
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let client = open_client(profile).await?;
        execute_on_client(&client, profile, sql, events, rows_tx).await
    }
}

/// Build a Redis profile from `redis://[user[:password]@]host[:port][/db]`
/// (or `rediss://` for TLS with the server certificate trusted, and the
/// `valkey`/`valkeys` spellings) — the shape `SQLLY_REDIS_URL` uses.
pub fn profile_from_url(name: &str, url: &str) -> StudioResult<ConnectionProfile> {
    if url
        .trim()
        .split_once("://")
        .is_some_and(|(scheme, _)| scheme.to_ascii_lowercase().ends_with("+sentinel"))
    {
        return sentinel_profile_from_url(name, url);
    }
    let parsed = url::Url::parse(url.trim())
        .map_err(|e| StudioError::InvalidProfile(format!("invalid Redis URL: {e}")))?;
    let tls = match parsed.scheme() {
        "redis" | "valkey" => false,
        "rediss" | "valkeys" => true,
        other => {
            return Err(StudioError::InvalidProfile(format!(
                "unsupported Redis URL scheme '{other}'"
            )));
        }
    };
    let host = parsed.host_str().ok_or_else(|| {
        StudioError::InvalidProfileMessage(sqlly_core::DisplayMessage::new(
            "services.redis.errors.no_host",
            "Redis URL has no host",
        ))
    })?;
    let username = percent_decode(parsed.username());
    let password = parsed.password().map(percent_decode).unwrap_or_default();
    let mut profile = ConnectionProfile::sql_login(
        name,
        host,
        username,
        SecretRef {
            provider: SecretProvider::InMemory,
            key: password,
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Redis;
    profile.port = parsed.port().unwrap_or(DEFAULT_PORT);
    let db = parsed.path().trim_matches('/').to_string();
    let db = if db.is_empty() {
        parsed
            .query_pairs()
            .find(|(k, _)| k == "db")
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default()
    } else {
        db
    };
    profile.database = Some(db).filter(|s| !s.is_empty());
    profile.encrypt = tls;
    profile.trust_server_certificate = tls;
    profile.timeout_seconds = profile.timeout_seconds.max(30);
    Ok(profile)
}

/// `redis+sentinel://[user:pass@]s1:26379,s2/master[/db][?sentinelPassword=…]`
/// (and `rediss+sentinel`, `valkey+sentinel`, `valkeys+sentinel`): a profile
/// that finds its master through the listed sentinels. Like the plain form,
/// the passwords travel as in-memory secrets.
fn sentinel_profile_from_url(name: &str, url: &str) -> StudioResult<ConnectionProfile> {
    let parsed = sqlly_core::parse_connection_url(url)
        .map_err(|e| StudioError::InvalidProfile(format!("invalid Redis Sentinel URL: {e}")))?;
    let Some(mut sentinel) = parsed.redis_sentinel.map(|config| *config) else {
        return Err(StudioError::InvalidProfile(
            "invalid Redis Sentinel URL: not a Redis or Valkey Sentinel URL".to_string(),
        ));
    };
    if let Some(problem) = sentinel.problem() {
        return Err(StudioError::InvalidProfile(problem));
    }
    sentinel.sentinel_password_ref = parsed.sentinel_password.map(|key| SecretRef {
        provider: SecretProvider::InMemory,
        key,
    });
    let mut profile = ConnectionProfile::sql_login(
        name,
        &parsed.server,
        parsed.username.unwrap_or_default(),
        SecretRef {
            provider: SecretProvider::InMemory,
            key: parsed.password.unwrap_or_default(),
        },
    );
    profile.database_type = parsed.engine;
    profile.port = parsed
        .port
        .unwrap_or(sqlly_core::redis_sentinel::DEFAULT_SENTINEL_PORT);
    profile.database = parsed.database.filter(|db| !db.is_empty());
    let tls = parsed.encrypt.unwrap_or(false);
    profile.encrypt = tls;
    profile.trust_server_certificate = tls;
    profile.redis_sentinel = Some(sentinel);
    profile.timeout_seconds = profile.timeout_seconds.max(30);
    Ok(profile)
}

fn percent_decode(value: &str) -> String {
    let mut out = Vec::with_capacity(value.len());
    let bytes = value.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The live-test profile from `SQLLY_REDIS_URL`, or `None` so tests
/// self-skip on a checkout without a server.
pub fn live_profile_from_env(name: &str) -> Option<ConnectionProfile> {
    let url = std::env::var("SQLLY_REDIS_URL").ok()?;
    match profile_from_url(name, &url) {
        Ok(profile) => Some(profile),
        Err(error) => {
            eprintln!("SQLLY_REDIS_URL is not usable: {error}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;
    use std::time::Duration;

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "r",
            "cache.example.com",
            "analyst",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        p.database_type = DatabaseType::Redis;
        p.port = 6379;
        p.encrypt = false;
        p
    }

    #[test]
    fn credentials_map_blank_user_to_default_and_reject_foreign_auth() {
        assert_eq!(
            credentials(&profile()).unwrap(),
            (Some("analyst".to_string()), Some("pw".to_string()))
        );
        let mut blank = profile();
        blank.authentication = AuthenticationType::SqlLogin {
            username: String::new(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        };
        assert_eq!(credentials(&blank).unwrap(), (None, None));
        let mut entra = profile();
        entra.authentication = AuthenticationType::EntraBrowser;
        entra.runtime_entra_access_token = Some(entra_jwt("oid-42", 4_000_000_000));
        let (user, password) = credentials(&entra).unwrap();
        assert_eq!(user.as_deref(), Some("oid-42"));
        assert_eq!(password, entra.runtime_entra_access_token);
        let mut iam = profile();
        iam.authentication = AuthenticationType::GcpCloudSqlIam {
            username: "x".into(),
        };
        assert!(matches!(
            credentials(&iam),
            Err(StudioError::InvalidProfile(_))
        ));
    }

    #[test]
    fn aws_iam_credentials_pair_the_user_id_with_the_minted_token() {
        let mut iam = profile();
        iam.authentication = AuthenticationType::AwsRdsIam {
            username: "iam-user".into(),
            region: "us-east-1".into(),
            profile: None,
        };
        // IAM tokens are never sent in plaintext: no TLS, no credentials.
        iam.encrypt = false;
        iam.runtime_cloud_iam_token = Some("signed-token".into());
        assert!(matches!(
            credentials(&iam),
            Err(StudioError::InvalidProfile(msg)) if msg.contains("TLS")
        ));
        iam.encrypt = true;
        // No token minted yet: the error must say so, not fail AUTH at the server.
        iam.runtime_cloud_iam_token = None;
        assert!(matches!(
            credentials(&iam),
            Err(StudioError::Authentication(_))
        ));
        iam.runtime_cloud_iam_token = Some(
            "clustercfg.use1.cache.amazonaws.com:6379/?Action=connect&User=iam-user&X-Amz-Signature=abc"
                .into(),
        );
        let (user, password) = credentials(&iam).unwrap();
        assert_eq!(user.as_deref(), Some("iam-user"));
        assert_eq!(password, iam.runtime_cloud_iam_token);
        // A blank user id can never authenticate; refuse it up front.
        let mut blank = iam.clone();
        blank.authentication = AuthenticationType::AwsRdsIam {
            username: "   ".into(),
            region: "us-east-1".into(),
            profile: None,
        };
        assert!(matches!(
            credentials(&blank),
            Err(StudioError::InvalidProfile(_))
        ));
    }

    #[test]
    fn aws_iam_profile_builds_a_client_with_user_and_token() {
        let mut p = profile();
        p.server = "clustercfg.xxxx.use1.cache.amazonaws.com".into();
        p.authentication = AuthenticationType::AwsRdsIam {
            username: "iam-user".into(),
            region: "us-east-1".into(),
            profile: None,
        };
        p.encrypt = true;
        p.tls_mode = TlsMode::VerifyFull;
        p.runtime_cloud_iam_token = Some("signed-token".into());
        let host = p.server.clone();
        let client = build_client(&p, &host, 6379).unwrap();
        let settings = client.get_connection_info().redis_settings();
        assert_eq!(settings.username(), Some("iam-user"));
        assert_eq!(settings.password(), Some("signed-token"));
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

    #[test]
    fn entra_profile_builds_a_client_with_oid_and_token() {
        let mut p = profile();
        p.server = "contoso.redis.cache.windows.net".into();
        p.authentication = AuthenticationType::EntraBrowser;
        p.encrypt = true;
        p.tls_mode = TlsMode::VerifyFull;
        p.runtime_entra_access_token = Some(entra_jwt("oid-7", 4_000_000_000));
        let host = p.server.clone();
        let client = build_client(&p, &host, 10000).unwrap();
        let settings = client.get_connection_info().redis_settings();
        assert_eq!(settings.username(), Some("oid-7"));
        assert_eq!(settings.password(), p.runtime_entra_access_token.as_deref());
    }

    #[test]
    fn database_index_is_a_number() {
        let mut p = profile();
        assert_eq!(database_index(&p).unwrap(), 0);
        p.database = Some(" 3 ".into());
        assert_eq!(database_index(&p).unwrap(), 3);
        p.database = Some("shop".into());
        assert!(matches!(
            database_index(&p),
            Err(StudioError::InvalidProfile(_))
        ));
    }

    fn proxied(mut p: ConnectionProfile) -> ConnectionProfile {
        p.transport = sqlly_core::connection::TransportChain {
            proxy: Some(sqlly_core::connection::ProxyHop {
                kind: sqlly_core::connection::ProxyKind::Socks5,
                host: "proxy.example.com".into(),
                port: 1080,
                username: None,
                password: None,
            }),
            ssh: None,
            kube: None,
            ssm: None,
        };
        p
    }

    #[test]
    fn verify_full_through_a_tunnel_names_the_profile_server() {
        let mut p = proxied(profile());
        p.tls_mode = TlsMode::VerifyFull;
        assert_eq!(
            tunneled_tls_name(&p, "127.0.0.1"),
            Some(("cache.example.com".to_string(), 6379))
        );
        // Dialing the server itself (a proxy that resolves the name) needs
        // nothing extra.
        assert_eq!(tunneled_tls_name(&p, " cache.example.com "), None);
        // Verify CA skips the host name and Require verifies nothing.
        p.tls_mode = TlsMode::VerifyCa;
        assert_eq!(tunneled_tls_name(&p, "127.0.0.1"), None);
        p.tls_mode = TlsMode::Require;
        assert_eq!(tunneled_tls_name(&p, "127.0.0.1"), None);
        // A direct connection dials the name it verifies.
        let mut direct = profile();
        direct.tls_mode = TlsMode::VerifyFull;
        assert_eq!(tunneled_tls_name(&direct, "127.0.0.1"), None);
    }

    /// Live (`SQLLY_REDIS_TLS_URL` + `SQLLY_REDIS_TLS_CA_FILE`; the fixture's
    /// certificate names `localhost` and `127.0.0.1` only): a Verify-full
    /// profile for `localhost` that reaches the server at `::1` — standing
    /// in for a tunnel's local end, which the certificate does not name —
    /// verifies against `localhost` and connects; the same dial without a
    /// tunnel checks `::1` and is refused.
    #[tokio::test]
    async fn live_verify_full_through_a_tunnel_checks_the_profile_host() {
        let (Ok(url), Ok(ca)) = (
            std::env::var("SQLLY_REDIS_TLS_URL"),
            std::env::var("SQLLY_REDIS_TLS_CA_FILE"),
        ) else {
            eprintln!("skipping: SQLLY_REDIS_TLS_URL / SQLLY_REDIS_TLS_CA_FILE not set");
            return;
        };
        let _ = rustls::crypto::ring::default_provider().install_default();
        let mut p = profile_from_url("tls name", &url).unwrap();
        p.server = "localhost".into();
        p.tls_mode = TlsMode::VerifyFull;
        p.tls_ca_file = Some(ca.into());
        let port = p.port;
        let tunneled = proxied(p.clone());
        let client = match connect(&tunneled, "::1", port, ProtocolVersion::RESP2).await {
            Ok(client) => client,
            Err(refusal) => panic!("{}", refusal.into_error()),
        };
        assert!(client.run(&redis::cmd("PING")).await.is_ok());
        assert_eq!(client.endpoint(), format!("::1:{port}"));
        let mut direct = p;
        direct.server = "::1".into();
        assert!(
            connect(&direct, "::1", port, ProtocolVersion::RESP2)
                .await
                .is_err(),
            "the certificate does not name ::1"
        );
    }

    #[test]
    fn connection_addr_follows_tls_mode() {
        let mut p = profile();
        assert!(matches!(
            connection_addr(&p, "h", 0).unwrap(),
            ConnectionAddr::Tcp(host, 6379) if host == "h"
        ));
        p.encrypt = true;
        p.trust_server_certificate = true;
        assert!(matches!(
            connection_addr(&p, "[::1]", 6380).unwrap(),
            ConnectionAddr::TcpTls { host, port: 6380, insecure: true, .. } if host == "::1"
        ));
        p.tls_mode = TlsMode::VerifyFull;
        assert!(matches!(
            connection_addr(&p, "h", 1).unwrap(),
            ConnectionAddr::TcpTls {
                insecure: false,
                tls_params: None,
                ..
            }
        ));
        p.tls_mode = TlsMode::VerifyCa;
        assert!(matches!(
            connection_addr(&p, "h", 1).unwrap(),
            ConnectionAddr::TcpTls {
                insecure: false,
                tls_params: Some(_),
                ..
            }
        ));
        assert!(build_client(&p, "h", 1).is_ok());
    }

    #[test]
    fn profile_from_url_fills_every_field() {
        let p = profile_from_url("live", "redis://app:s%40lly@127.0.0.1:6380/3").unwrap();
        assert_eq!(p.database_type, DatabaseType::Redis);
        assert_eq!(p.server, "127.0.0.1");
        assert_eq!(p.port, 6380);
        assert_eq!(p.database.as_deref(), Some("3"));
        assert!(!p.encrypt);
        match &p.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "app");
                assert_eq!(secret_ref.key, "s@lly");
            }
            other => panic!("{other:?}"),
        }
        let bare = profile_from_url("live", "rediss://cache.example.com?db=2").unwrap();
        assert_eq!(bare.port, DEFAULT_PORT);
        assert_eq!(bare.database.as_deref(), Some("2"));
        assert!(bare.encrypt && bare.trust_server_certificate);
        let valkey = profile_from_url("v", "valkey://:pw@h").unwrap();
        assert!(valkey.database.is_none());
        assert!(profile_from_url("x", "mysql://h").is_err());
    }

    #[test]
    fn profile_from_sentinel_url_names_master_sentinels_and_secrets() {
        let p = profile_from_url(
            "s",
            "redis+sentinel://app:pw@10.0.0.1:26379,10.0.0.2/mymaster/2?sentinelPassword=s%40fe",
        )
        .unwrap();
        assert_eq!(p.database_type, DatabaseType::Redis);
        assert_eq!(p.database.as_deref(), Some("2"));
        assert!(!p.encrypt);
        let sentinel = p.redis_sentinel.as_ref().unwrap();
        assert_eq!(sentinel.master_name, "mymaster");
        assert_eq!(
            sentinel.sentinel_addresses().unwrap(),
            vec![
                ("10.0.0.1".to_string(), 26379),
                ("10.0.0.2".to_string(), 26379)
            ]
        );
        assert_eq!(sentinel.sentinel_password_ref.as_ref().unwrap().key, "s@fe");
        match &p.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "app");
                assert_eq!(secret_ref.key, "pw");
            }
            other => panic!("{other:?}"),
        }
        let tls = profile_from_url("t", "rediss+sentinel://s1/cache").unwrap();
        assert!(tls.encrypt);
        assert!(tls.redis_sentinel.unwrap().sentinel_password_ref.is_none());
        let valkey = profile_from_url("v", "valkey+sentinel://s1/m").unwrap();
        assert_eq!(valkey.database_type, DatabaseType::Valkey);
        assert!(profile_from_url("x", "redis+sentinel://s1").is_err());
    }

    fn drain(events: &mut sqlly_core::EventSubscription) -> Vec<EventPayload> {
        let mut out = Vec::new();
        while let Ok(event) = events.try_recv() {
            out.push(event.payload);
        }
        out
    }

    async fn run_collect(
        client: &RedisClient,
        profile: &ConnectionProfile,
        text: &str,
    ) -> (
        Vec<Vec<sqlly_core::ResultColumn>>,
        Vec<Vec<Vec<CellValue>>>,
        Vec<EventPayload>,
    ) {
        let events = EventBus::new(1024);
        let mut sub = events.subscribe();
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
            .unwrap();
        drop(tx);
        let mut by_index = drain_rows.await.unwrap();
        let mut columns = Vec::new();
        let payloads = drain(&mut sub);
        for payload in &payloads {
            if let EventPayload::ResultSetStarted { columns: c, .. } = payload {
                columns.push(c.clone());
            }
        }
        let rows = (0..columns.len())
            .map(|i| by_index.remove(&i).unwrap_or_default())
            .collect();
        (columns, rows, payloads)
    }

    fn col_names(columns: &[sqlly_core::ResultColumn]) -> Vec<&str> {
        columns.iter().map(|c| c.name.as_str()).collect()
    }

    /// Live: every result shape, SELECT switching, a 5k-key fixture walked
    /// through SQLLY.KEYS with types and TTLs, INFO / CLIENT LIST / SLOWLOG,
    /// blocking-command refusal, read-only policy, server errors, cancel and
    /// the tool virtual commands. Opt-in through `SQLLY_REDIS_URL`
    /// (`redis://127.0.0.1:6380/0`).
    /// The key browser's own truncation path on a keyspace three times its
    /// budget: `SQLLY.KEYS wsi:* 10000` stops at the budget with a live
    /// cursor, and continuing from that cursor twice covers every key once.
    /// Opt-in through `SQLLY_REDIS_URL`; works in logical database 6 and
    /// removes only its own `wsi:` keys afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_capped_listing_continues_from_its_cursor_over_thirty_thousand_keys() {
        let Some(mut profile) = live_profile_from_env("redis-wsi") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.auto_wrap_rollback = false;
        profile.database = Some("6".into());
        let client = open_client(&profile).await.expect("client");
        const TOTAL: usize = 30_000;
        for chunk in (0..TOTAL).collect::<Vec<_>>().chunks(5_000) {
            let mut pipe = redis::pipe();
            for i in chunk {
                pipe.cmd("SET").arg(format!("wsi:{i}")).arg("v");
            }
            client.run_pipeline(&pipe).await.expect("seed");
        }
        let key_text = |cell: &CellValue| match cell {
            CellValue::String(s) => s.clone(),
            CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        };
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut cursor = "0".to_string();
        let mut walks = 0;
        loop {
            let command = format!("SQLLY.KEYS wsi:* {} {cursor}", browse::DEFAULT_KEY_LIMIT);
            let (columns, rows, _) = run_collect(&client, &profile, &command).await;
            assert_eq!(
                col_names(&columns[0]),
                vec!["key", "type", "ttl_seconds", "complete", "cursor"]
            );
            walks += 1;
            let rows = &rows[0];
            assert!(!rows.is_empty(), "walk {walks} listed nothing");
            for row in rows {
                assert!(
                    seen.insert(key_text(&row[0])),
                    "walk {walks} re-listed {}",
                    key_text(&row[0])
                );
            }
            let complete = rows[0][3] == CellValue::Bool(true);
            let CellValue::String(next) = &rows[0][4] else {
                panic!("cursor cell");
            };
            if complete {
                assert_eq!(next, "0");
                break;
            }
            assert!(
                rows.len() >= browse::DEFAULT_KEY_LIMIT,
                "a capped walk lists at least its budget, got {}",
                rows.len()
            );
            assert_ne!(next, "0", "a capped walk leaves a live cursor");
            cursor = next.clone();
            assert!(walks < 10, "the walk never completed");
        }
        assert_eq!(seen.len(), TOTAL, "every seeded key listed exactly once");
        assert!(
            walks >= 3,
            "30,000 keys take at least three 10,000-key walks"
        );

        // Clean up only our own keys.
        for chunk in (0..TOTAL).collect::<Vec<_>>().chunks(5_000) {
            let mut pipe = redis::pipe();
            for i in chunk {
                pipe.cmd("UNLINK").arg(format!("wsi:{i}"));
            }
            client.run_pipeline(&pipe).await.expect("cleanup");
        }
        let (_, rows, _) = run_collect(&client, &profile, "SQLLY.KEYS wsi:* 100").await;
        assert!(rows[0].is_empty(), "cleanup left wsi: keys behind");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_shapes_select_scan_and_tools() {
        let Some(mut profile) = live_profile_from_env("redis-live") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        profile.auto_wrap_rollback = false;
        // Work in logical database 9 so a developer's own keys are untouched.
        profile.database = Some("9".into());
        let transport = RedisTransport;
        transport.test_connection(&profile).await.unwrap();
        let dbs = transport.list_databases(&profile).await.unwrap();
        assert!(
            dbs.iter().any(|d| d.name == "0") && dbs.iter().any(|d| d.name == "9"),
            "{dbs:?}"
        );

        // A user the server does not know is refused as an authentication error.
        let mut wrong = profile.clone();
        wrong.authentication = AuthenticationType::SqlLogin {
            username: "nobody-sqlly".into(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "wrong".into(),
            },
        };
        assert!(
            matches!(
                open_client(&wrong).await,
                Err(StudioError::Authentication(_))
            ),
            "a wrong login must not connect"
        );

        let client = open_client(&profile).await.unwrap();
        assert_eq!(client.current_database(), 9);
        client.run(&redis::cmd("FLUSHDB")).await.unwrap();

        // Every reply shape, one result set per line, in order.
        let script = "# fixture\nSET greeting \"hello world\"\nGET greeting\nGET missing\nINCR counter\nRPUSH l a b c\nLRANGE l 0 -1\nHSET h name ann age 3\nHGETALL h\nZADD z 1.5 a 2 b\nZRANGE z 0 -1 WITHSCORES\nSADD s x\nSMEMBERS s\nXADD st 1-0 f v\nXRANGE st - +\n[\"SET\", \"json key\", \"two words\"]\nGET \"json key\"\nCONFIG GET databases\nSCAN 0 MATCH greeting COUNT 10\nDBSIZE\n";
        let (columns, rows, _) = run_collect(&client, &profile, script).await;
        assert_eq!(columns.len(), 19, "{columns:?}");
        assert_eq!(rows[0], vec![vec![CellValue::String("OK".into())]]);
        assert_eq!(rows[1], vec![vec![CellValue::String("hello world".into())]]);
        assert_eq!(rows[2], vec![vec![CellValue::Null]]);
        assert_eq!(rows[3], vec![vec![CellValue::I64(1)]]);
        assert_eq!(col_names(&columns[5]), vec!["index", "value"]);
        assert_eq!(rows[5].len(), 3);
        assert_eq!(col_names(&columns[7]), vec!["field", "value"]);
        assert_eq!(rows[7].len(), 2);
        assert_eq!(col_names(&columns[9]), vec!["member", "score"]);
        assert_eq!(
            rows[9][0],
            vec![
                CellValue::String("a".into()),
                CellValue::String("1.5".into())
            ]
        );
        assert_eq!(
            rows[11],
            vec![vec![CellValue::I64(0), CellValue::String("x".into())]]
        );
        assert_eq!(col_names(&columns[13]), vec!["id", "fields"]);
        assert_eq!(rows[13][0][1], CellValue::String("{\"f\":\"v\"}".into()));
        assert_eq!(rows[15], vec![vec![CellValue::String("two words".into())]]);
        assert_eq!(col_names(&columns[16]), vec!["field", "value"]);
        assert_eq!(col_names(&columns[17]), vec!["cursor", "key"]);
        assert_eq!(rows[17][0][1], CellValue::String("greeting".into()));
        assert_eq!(rows[18], vec![vec![CellValue::I64(8)]]);

        // INFO, CLIENT LIST and SLOWLOG are tables.
        let (columns, rows, _) =
            run_collect(&client, &profile, "INFO server\nCLIENT LIST\nSLOWLOG GET 5").await;
        assert_eq!(col_names(&columns[0]), vec!["section", "key", "value"]);
        assert!(
            rows[0]
                .iter()
                .any(|r| r[1] == CellValue::String("redis_version".into()))
        );
        assert_eq!(&col_names(&columns[1])[..2], &["session_id", "status"]);
        assert!(rows[1].iter().any(|r| matches!(r[0], CellValue::I64(_))));
        assert_eq!(col_names(&columns[2])[4], "query_text");

        // SELECT switches the connection's database and reports it.
        let (_, rows, payloads) =
            run_collect(&client, &profile, "SELECT 10\nDBSIZE\nSELECT 9").await;
        assert_eq!(rows[1], vec![vec![CellValue::I64(0)]]);
        assert!(
            !payloads
                .iter()
                .any(|p| matches!(p, EventPayload::DatabaseChanged { .. }))
        );
        let (_, _, payloads) = run_collect(&client, &profile, "SELECT 10").await;
        assert!(
            payloads
                .iter()
                .any(|p| matches!(p, EventPayload::DatabaseChanged { name, .. } if name == "10"))
        );
        assert_eq!(client.current_database(), 10);
        browse::select_database(&client, 9).await.unwrap();

        // Server errors carry Redis's text and the line.
        let bus = EventBus::new(8);
        let _keep = bus.subscribe();
        let err = execute_on_client(
            &client,
            &profile,
            "GET greeting\nLPUSH greeting x",
            &bus,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("line 2: WRONGTYPE"), "{err}");
        // The exact rendered shape the bulk import's per-line accounting
        // parses (`redis_bulk::split_line_error` in the GPUI app): the error
        // kind's prefix, then the `line N:` marker.
        assert!(
            err.to_string()
                .starts_with("Query execution error: line 2: WRONGTYPE"),
            "{err}"
        );

        // Blocking commands are refused before they are sent.
        for text in [
            "SUBSCRIBE news",
            "MONITOR",
            "BLPOP l 0",
            "XREAD BLOCK 0 STREAMS st $",
        ] {
            let err = execute_on_client(&client, &profile, text, &bus, None)
                .await
                .unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("line 1:")
                    && (message.contains("forever")
                        || message.contains("timeout")
                        || message.contains("redis-cli")
                        || message.contains("Profiler")
                        || message.contains("Pub/Sub")),
                "{text}: {err}"
            );
        }
        assert!(
            execute_on_client(&client, &profile, "BLPOP l 1", &bus, None)
                .await
                .is_ok()
        );

        // The SELECT-only lock means read-only commands only.
        let mut locked = profile.clone();
        locked.block_all_but_select = true;
        assert!(
            execute_on_client(&client, &locked, "GET greeting\nHGETALL h", &bus, None)
                .await
                .is_ok()
        );
        let err = execute_on_client(&client, &locked, "GET greeting\nDEL greeting", &bus, None)
            .await
            .unwrap_err();
        assert!(matches!(err, StudioError::PermissionDenied(_)), "{err}");

        // 5k-key fixture: SQLLY.KEYS walks it with SCAN, types and TTLs.
        let mut pipe = redis::pipe();
        for i in 0..5000 {
            let key = match i % 4 {
                0 => format!("user:{i}:profile"),
                1 => format!("user:{i}:session"),
                2 => format!("order:{i}"),
                _ => format!("cache/{i}"),
            };
            match i % 4 {
                0 => {
                    pipe.cmd("HSET").arg(&key).arg("f").arg("v");
                }
                1 => {
                    pipe.cmd("SET").arg(&key).arg("s").arg("EX").arg(3600);
                }
                2 => {
                    pipe.cmd("RPUSH").arg(&key).arg("x");
                }
                _ => {
                    pipe.cmd("SADD").arg(&key).arg("m");
                }
            }
        }
        client.run_pipeline(&pipe).await.unwrap();
        let (columns, rows, _) = run_collect(&client, &profile, "SQLLY.KEYS * 100000").await;
        assert_eq!(
            col_names(&columns[0]),
            vec!["key", "type", "ttl_seconds", "complete", "cursor"]
        );
        assert_eq!(
            rows[0].len(),
            5008,
            "every key of the fixture plus the earlier ones"
        );
        assert!(rows[0].iter().all(|r| r[3] == CellValue::Bool(true)));
        assert!(
            rows[0]
                .iter()
                .all(|r| r[4] == CellValue::String("0".into())),
            "a complete walk reports the zero cursor"
        );
        // A capped walk reports the cursor to continue from, and continuing
        // from it walks on past the keys already listed instead of over them.
        let (_, first, _) = run_collect(&client, &profile, "SQLLY.KEYS * 2000").await;
        assert!(first[0].len() >= 2000 && first[0].len() < 2000 + browse::SCAN_COUNT);
        assert!(first[0].iter().all(|r| r[3] == CellValue::Bool(false)));
        let CellValue::String(cursor) = &first[0][0][4] else {
            panic!("cursor cell");
        };
        assert_ne!(cursor, "0", "a capped walk leaves a live cursor");
        let (_, rest, _) =
            run_collect(&client, &profile, &format!("SQLLY.KEYS * 100000 {cursor}")).await;
        assert!(rest[0].iter().all(|r| r[3] == CellValue::Bool(true)));
        let key_text = |cell: &CellValue| match cell {
            CellValue::String(s) => s.clone(),
            CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        };
        let mut seen: std::collections::HashSet<String> =
            first[0].iter().map(|r| key_text(&r[0])).collect();
        let overlap = rest[0]
            .iter()
            .filter(|r| !seen.insert(key_text(&r[0])))
            .count();
        assert_eq!(
            overlap, 0,
            "the continued walk never re-lists the first page's keys"
        );
        assert_eq!(
            seen.len(),
            5008,
            "first page plus continuation cover the whole keyspace"
        );
        let session = rows[0]
            .iter()
            .find(|r| r[0] == CellValue::String("user:1:session".into()))
            .unwrap();
        assert_eq!(session[1], CellValue::String("string".into()));
        assert!(matches!(session[2], CellValue::I64(ttl) if ttl > 0 && ttl <= 3600));
        let profile_key = rows[0]
            .iter()
            .find(|r| r[0] == CellValue::String("user:0:profile".into()))
            .unwrap();
        assert_eq!(profile_key[1], CellValue::String("hash".into()));
        assert_eq!(profile_key[2], CellValue::Null);
        let (_, rows, _) = run_collect(&client, &profile, "SQLLY.KEYS user:* 100").await;
        // The budget stops the walk at its page boundary: the last page is
        // kept whole so the reported cursor continues exactly after it.
        assert!(rows[0].len() >= 100 && rows[0].len() < 100 + browse::SCAN_COUNT);
        assert!(rows[0].iter().all(|r| r[3] == CellValue::Bool(false)));
        let (_, rows, _) = run_collect(&client, &profile, "SQLLY.KEYS order:*").await;
        assert_eq!(rows[0].len(), 1250);
        assert!(
            rows[0]
                .iter()
                .all(|r| r[1] == CellValue::String("list".into()))
        );

        // The tool virtual commands.
        let (columns, rows, _) = run_collect(&client, &profile, "SQLLY.DATABASES").await;
        assert_eq!(
            col_names(&columns[0]),
            vec!["database_name", "keys", "expires", "avg_ttl_ms"]
        );
        let nine = rows[0]
            .iter()
            .find(|r| r[0] == CellValue::String("9".into()))
            .unwrap();
        assert_eq!(nine[1], CellValue::I64(5008));
        assert_eq!(nine[2], CellValue::I64(1250));
        let (columns, rows, _) = run_collect(&client, &profile, "SQLLY.DATABASES MEMORY 50").await;
        assert_eq!(columns[0].len(), 9);
        let nine = rows[0]
            .iter()
            .find(|r| r[0] == CellValue::String("9".into()))
            .unwrap();
        assert_eq!(nine[4], CellValue::I64(50));
        assert!(matches!(nine[6], CellValue::F64(mb) if mb > 0.0));
        assert_eq!(
            client.current_database(),
            9,
            "memory sampling restores the database"
        );
        for group in info::DASHBOARD_GROUPS {
            let (columns, rows, _) =
                run_collect(&client, &profile, &format!("SQLLY.DASHBOARD {group}")).await;
            assert_eq!(col_names(&columns[0]), vec!["metric", "value"]);
            assert!(!rows[0].is_empty(), "{group}");
        }
        let (columns, rows, _) = run_collect(
            &client,
            &profile,
            "SQLLY.WAITS\nSQLLY.BLOCKING\nSQLLY.SERVERINFO",
        )
        .await;
        assert_eq!(col_names(&columns[0])[0], "wait_type");
        assert!(!rows[0].is_empty());
        assert!(rows[1].is_empty());
        assert_eq!(col_names(&columns[1])[0], "blocking_session_id");
        assert_eq!(
            col_names(&columns[2]),
            vec![
                "product",
                "version",
                "charset",
                "collation",
                "case_flag",
                "current_user_name",
                "database_name",
                "tls",
                "protocol"
            ]
        );
        // The fixture may be a fork: the product is whatever INFO names.
        assert!(
            matches!(
                &rows[2][0][0],
                CellValue::String(p) if ["Redis", "Valkey", "KeyDB", "DragonflyDB"].contains(&p.as_str())
            ),
            "{:?}",
            rows[2][0][0]
        );
        assert_eq!(rows[2][0][6], CellValue::String("9".into()));

        // SQLLY.COMMANDS: the server's own command docs, one row per
        // command, parseable by the IntelliSense vocabulary.
        let (columns, rows, _) = run_collect(&client, &profile, "SQLLY.COMMANDS").await;
        assert_eq!(col_names(&columns[0]), vec!["command", "docs", "info"]);
        assert!(rows[0].len() > 200, "{}", rows[0].len());
        let get = rows[0]
            .iter()
            .find(|r| r[0] == CellValue::String("GET".into()))
            .expect("GET documented");
        let (CellValue::String(docs), CellValue::String(info)) = (&get[1], &get[2]) else {
            panic!("{get:?}");
        };
        assert!(docs.contains("Returns the string value of a key"), "{docs}");
        assert!(info.contains("readonly"), "{info}");
        let vocabulary = sqlly_intel::redis_complete::CommandVocabulary::from_server_rows(
            rows[0].iter().filter_map(|r| match (&r[0], &r[1], &r[2]) {
                (CellValue::String(n), CellValue::String(d), CellValue::String(i)) => {
                    Some((n.as_str(), d.as_str(), i.as_str()))
                }
                _ => None,
            }),
        )
        .expect("rows parse");
        assert!(
            vocabulary
                .get("ZRANGE")
                .unwrap()
                .syntax()
                .contains("WITHSCORES")
        );
        assert!(vocabulary.get("CONFIG").unwrap().is_container());

        // Cancel releases a long-running script quickly.
        let slow = open_client(&profile).await.unwrap();
        let handle = cancel_handle(&slow);
        let started = std::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            handle.cancel();
        });
        // 200 one-second blocking pops on a list nobody fills: well over a
        // minute if it ran to the end.
        let mut long = String::new();
        for _ in 0..200 {
            long.push_str("BLPOP never-filled 1\n");
        }
        let result = execute_on_client(&slow, &profile, &long, &bus, None).await;
        assert!(
            matches!(result, Err(StudioError::QueryCancelled)),
            "{result:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));

        client.run(&redis::cmd("FLUSHDB")).await.unwrap();
    }

    /// Live: the analyzer's reports in one run share ONE sample (their key
    /// counts agree), and a user whose ACL denies MEMORY still gets counts
    /// and TTLs — an empty top-keys list and a stated reason, not an error
    /// that stops the script. Opt-in through `SQLLY_REDIS_URL` (the ACL
    /// half needs an admin connection); uses database 13.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_analyzer_shares_one_sample_and_survives_noperm_memory() {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let mut profile = profile_from_url("redis-analyzer", &url).expect("profile");
        profile.database = Some("13".to_string());
        let client = open_client(&profile).await.expect("client");
        let prefix = format!("sqlly-analyzer-{}", std::process::id());
        let mut seed = redis::pipe();
        for i in 0..30 {
            seed.cmd("SET")
                .arg(format!("{prefix}:s{i}"))
                .arg("x".repeat(50 + i));
        }
        seed.cmd("HSET")
            .arg(format!("{prefix}:h"))
            .arg("f")
            .arg("v");
        seed.cmd("SET")
            .arg(format!("{prefix}:t"))
            .arg("1")
            .arg("EX")
            .arg(600);
        client.run_pipeline(&seed).await.expect("seed");

        async fn run(client: &RedisClient, line: &str) -> Shaped {
            let commands = parse_script(line).expect("parse");
            run_command(client, &commands[0]).await.expect(line)
        }
        let column = |shaped: &Shaped, name: &str| {
            shaped
                .columns
                .iter()
                .position(|c| c.name == name)
                .unwrap_or_else(|| panic!("no column {name}"))
        };
        let int = |cell: &CellValue| match cell {
            CellValue::I64(n) => *n,
            other => panic!("not an integer: {other:?}"),
        };

        client.analysis_cache.clear();
        let summary = run(&client, "SQLLY.MEMSUMMARY 20").await;
        let sampled = int(&summary.rows[0][column(&summary, "sampled_keys")]);
        let cached = client
            .analysis_cache
            .get(13, 20)
            .expect("the run's sample is memoized");
        let by_type = run(&client, "SQLLY.MEMBYTYPE 20").await;
        let ttl = run(&client, "SQLLY.TTLBUCKETS 20").await;
        assert!(
            std::sync::Arc::ptr_eq(&cached, &client.analysis_cache.get(13, 20).unwrap()),
            "later reports reuse the first report's sample"
        );
        let by_type_keys: i64 = by_type
            .rows
            .iter()
            .map(|r| int(&r[column(&by_type, "sampled_keys")]))
            .sum();
        let ttl_keys: i64 = ttl
            .rows
            .iter()
            .map(|r| int(&r[column(&ttl, "sampled_keys")]))
            .sum();
        assert_eq!(by_type_keys, sampled, "by-type counts the same sample");
        assert_eq!(ttl_keys, sampled, "TTL buckets count the same sample");
        let by_prefix = run(&client, "SQLLY.MEMBYPREFIX 20 1 \": |\"").await;
        let prefix_keys: i64 = by_prefix
            .rows
            .iter()
            .map(|r| int(&r[column(&by_prefix, "sampled_keys")]))
            .sum();
        assert_eq!(prefix_keys, sampled, "by-prefix counts the same sample");
        // A real command in between invalidates the sample.
        run(&client, "PING").await;
        assert!(client.analysis_cache.get(13, 20).is_none());

        // A user whose ACL denies MEMORY.
        let user = format!("sqlly-nomem-{}", std::process::id());
        let mut setuser = redis::cmd("ACL");
        setuser
            .arg("SETUSER")
            .arg(&user)
            .arg("on")
            .arg(">sqlly-test-pw")
            .arg("~*")
            .arg("+@all")
            .arg("-memory");
        if client.run(&setuser).await.is_err() {
            eprintln!("skipping the NOPERM half: the test user cannot run ACL SETUSER");
        } else {
            let mut limited = profile.clone();
            if let AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } = &mut limited.authentication
            {
                *username = user.clone();
                *secret_ref = SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "sqlly-test-pw".to_string(),
                };
            }
            let limited_client = open_client(&limited).await.expect("limited client");
            let summary = run(&limited_client, "SQLLY.MEMSUMMARY 20").await;
            assert_eq!(
                summary.rows[0][column(&summary, "memory_measured")],
                CellValue::Bool(false)
            );
            match &summary.rows[0][column(&summary, "memory_note")] {
                CellValue::String(note) => assert!(note.contains("not permitted"), "{note}"),
                other => panic!("no memory note: {other:?}"),
            }
            let top = run(&limited_client, "SQLLY.TOPKEYS 20 5").await;
            assert!(
                top.rows.is_empty(),
                "no ranking without MEMORY, and no error"
            );
            let by_type = run(&limited_client, "SQLLY.MEMBYTYPE 20").await;
            assert!(!by_type.rows.is_empty(), "counts survive");
            assert!(
                by_type
                    .rows
                    .iter()
                    .all(|r| r[column(&by_type, "sampled_bytes")] == CellValue::Null)
            );
            let mut deluser = redis::cmd("ACL");
            deluser.arg("DELUSER").arg(&user);
            client.run(&deluser).await.expect("cleanup user");
        }

        let mut cleanup = redis::cmd("DEL");
        for i in 0..30 {
            cleanup.arg(format!("{prefix}:s{i}"));
        }
        cleanup
            .arg(format!("{prefix}:h"))
            .arg(format!("{prefix}:t"));
        client.run(&cleanup).await.expect("cleanup");
    }

    /// Live: a user whose ACL denies INFO (as on ElastiCache with a narrow
    /// user) still gets the database list — the server node and the
    /// Databases view list every database, with the key counts left empty
    /// instead of failing. Opt-in through `SQLLY_REDIS_URL`; uses database 7.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_database_list_survives_noperm_info() {
        let Some(url) = std::env::var("SQLLY_REDIS_URL").ok() else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let mut profile = profile_from_url("redis-noinfo", &url).expect("profile");
        profile.database = Some("7".to_string());
        let client = open_client(&profile).await.expect("client");
        let user = format!("noinfo-{}", std::process::id());
        let mut setuser = redis::cmd("ACL");
        setuser
            .arg("SETUSER")
            .arg(&user)
            .arg("on")
            .arg(">noinfo-test-pw")
            .arg("~*")
            .arg("+@all")
            .arg("-info");
        if client.run(&setuser).await.is_err() {
            eprintln!("skipping: the test user cannot run ACL SETUSER");
            return;
        }
        let mut limited = profile.clone();
        limited.authentication = sqlly_core::AuthenticationType::SqlLogin {
            username: user.clone(),
            secret_ref: sqlly_core::SecretRef {
                provider: sqlly_core::SecretProvider::InMemory,
                key: "noinfo-test-pw".to_string(),
            },
        };
        let limited_client = open_client(&limited).await.expect("limited client");
        let listed = browse::databases(&limited_client).await;
        let rows = query_rows_on_client(&limited_client, "SQLLY.DATABASES").await;
        let mut deluser = redis::cmd("ACL");
        deluser.arg("DELUSER").arg(&user);
        client.run(&deluser).await.expect("cleanup user");

        let listed = listed.expect("databases without INFO");
        assert!(!listed.counts_known);
        assert!(listed.lines.iter().any(|l| l.db == 7));
        let rows = rows.expect("SQLLY.DATABASES without INFO");
        assert!(rows.rows.len() >= 8, "every database is listed");
        assert_eq!(rows.rows[7][0], CellValue::String("7".into()));
        assert_eq!(rows.rows[7][1], CellValue::Null, "unknown, not zero");
    }
}
