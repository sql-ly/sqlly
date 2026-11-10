//! # Redis Cluster
//!
//! A Redis profile reaches either one server or a Redis Cluster (self-hosted,
//! ElastiCache cluster mode, MemoryDB). [`RedisConn`] is the connection the
//! [`RedisClient`] holds: `Single` is the plain multiplexed connection every
//! standalone path has always used, `Cluster` the `redis` crate's slot-aware
//! async cluster connection. Everything else in the driver talks to the
//! client through `run` / `run_pipeline`, so the one dispatch point is
//! [`RedisConn`]'s `ConnectionLike` implementation:
//!
//! - single commands go to the crate, which routes them by key slot and
//!   follows `MOVED` / `ASK` itself;
//! - pipelines are split by slot (the crate refuses a cross-slot pipeline),
//!   each part sent to its slot's master, and the replies put back in order;
//! - node connections are made by [`CountingConnection`], which counts the
//!   `MOVED` / `ASK` replies the crate followed so a script can report its
//!   redirect churn;
//! - every node address the cluster names (seeds included) is dialed through
//!   [`NodeDialer`]: the profile's node map rewrites the advertised address,
//!   and a tunneled profile gets one local forward per node from
//!   [`crate::ssh::TunnelManager::endpoint_for_node`]. That is the fix for
//!   clusters whose nodes advertise addresses only the far side of a NAT or
//!   SSH bastion can reach: the advertised address is what the slot map and
//!   TLS name use, the dial address is what the socket connects to.
//!
//! The mode comes from the profile ([`sqlly_core::redis_cluster`]): `Cluster`
//! always routes, `Standalone` never does, and `Auto` asks the server with
//! `INFO cluster` after the first standalone connect and remembers the answer
//! for the session (per connection id), so a standalone server pays for one
//! probe per session and nothing else changes for it.
//!
//! Commands the editor cannot run on a cluster are refused before they are
//! sent, naming the rule ([`refusal`]): `KEYS` and `SCAN` (a cursor belongs to
//! one node; the key browser's `SQLLY.KEYS` walks every master), `SELECT` of
//! anything but 0 (a cluster has one logical database), transactions, and
//! multi-key commands whose keys hash to different slots.
//!
//! Fan-out helpers here back the tools that must see the whole keyspace:
//! [`scan_keys_across`] (the key browser and the bulk dry-run, with a
//! per-node cursor `host:port@cursor` so a listing continues where it
//! stopped), [`keyspace_across`] (`SQLLY.DATABASES`), [`sample_across`] (the
//! memory analyzer, proportional to each master's `DBSIZE`), and the
//! topology / trace virtual commands ([`topology_rows`], [`trace_rows`]).

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use futures::FutureExt;
use redis::aio::{ConnectionLike, MultiplexedConnection};
use redis::cluster::ClusterClientBuilder;
use redis::cluster_async::{ClusterConnection, Connect};
use redis::cluster_routing::{Route, RoutingInfo, SingleNodeRoutingInfo, SlotAddr};
use redis::{
    AsyncConnectionConfig, Cmd, IntoConnectionInfo, Pipeline, ProtocolVersion, RedisFuture,
    RedisResult, Value,
};
use sqlly_core::connection::ConnectionId;
use sqlly_core::redis_cluster::{
    RedisClusterConfig, RedisClusterMode, cluster_mode, has_host_list, seed_nodes,
};
use sqlly_core::{CellValue, ConnectionProfile, StudioError, StudioResult, TlsMode};
use sqlly_intel::query::safety::redis::RedisCommand;
use tokio_util::sync::CancellationToken;

use super::Refusal;
use super::browse::{KeyInfo, KeyListing, SCAN_COUNT};
use super::shape::{self, Shaped};
use super::{RedisClient, redis_error};

/// Number of hash slots in a Redis Cluster.
pub const SLOT_COUNT: u16 = 16384;

/// The hash slot of a key: CRC16 (XMODEM) of the key, or of its `{tag}`
/// when the key has a non-empty hash tag, modulo 16384 — exactly what
/// `CLUSTER KEYSLOT` answers.
pub fn key_slot(key: &[u8]) -> u16 {
    crc16_xmodem(hash_tag(key).unwrap_or(key)) % SLOT_COUNT
}

fn hash_tag(key: &[u8]) -> Option<&[u8]> {
    let open = key.iter().position(|&b| b == b'{')?;
    let close = key[open + 1..].iter().position(|&b| b == b'}')?;
    (close > 0).then(|| &key[open + 1..open + 1 + close])
}

fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in bytes {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

// ---------------------------------------------------------------------------
// Node connections that count redirects
// ---------------------------------------------------------------------------

tokio::task_local! {
    /// Set by [`NodeDialer::resolve`] while a node connection is being made,
    /// read back by [`CountingConnection::connect_with_config`]: the only
    /// way the crate's static connection factory learns which cluster (and
    /// so which redirect counter and client name) the connection belongs to.
    static DIAL_COUNTER: RefCell<Option<DialContext>>;
}

/// What a node connection learns from its cluster's [`NodeDialer`].
#[derive(Clone)]
struct DialContext {
    redirects: Arc<AtomicU64>,
    /// `CLIENT SETNAME` for the node connection (see [`super::setname`]).
    client_name: String,
}

/// One node connection of a cluster: a multiplexed connection that counts
/// every `MOVED` / `ASK` reply passing through it (the crate follows the
/// redirect; the count tells the user how much slot churn a script saw).
#[derive(Clone)]
pub struct CountingConnection {
    inner: MultiplexedConnection,
    redirects: Arc<AtomicU64>,
}

fn is_redirect_code(code: Option<&str>) -> bool {
    matches!(code, Some("MOVED" | "ASK"))
}

fn count_value(counter: &AtomicU64, value: &Value) {
    if let Value::ServerError(error) = value
        && is_redirect_code(Some(error.code()))
    {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn count_result<T>(counter: &AtomicU64, result: &RedisResult<T>) {
    if let Err(error) = result
        && is_redirect_code(error.code())
    {
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

impl ConnectionLike for CountingConnection {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        async move {
            let result = self.inner.req_packed_command(cmd).await;
            count_result(&self.redirects, &result);
            if let Ok(value) = &result {
                count_value(&self.redirects, value);
            }
            result
        }
        .boxed()
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        async move {
            let result = self
                .inner
                .req_packed_commands(pipeline, offset, count)
                .await;
            count_result(&self.redirects, &result);
            if let Ok(values) = &result {
                for value in values {
                    count_value(&self.redirects, value);
                }
            }
            result
        }
        .boxed()
    }

    fn get_db(&self) -> i64 {
        0
    }
}

impl Connect for CountingConnection {
    fn connect_with_config<'a, T>(info: T, config: AsyncConnectionConfig) -> RedisFuture<'a, Self>
    where
        T: IntoConnectionInfo + Send + 'a,
    {
        DIAL_COUNTER
            .scope(RefCell::new(None), async move {
                let info = info.into_connection_info()?;
                let client = redis::Client::open(info)?;
                let inner = client
                    .get_multiplexed_async_connection_with_config(&config)
                    .await?;
                let context = DIAL_COUNTER
                    .try_with(|slot| slot.borrow().clone())
                    .ok()
                    .flatten();
                let mut inner = inner;
                let redirects = match context {
                    Some(context) => {
                        // Every node connection names itself like a
                        // standalone one; a refusal leaves it unnamed.
                        let mut setname = redis::cmd("CLIENT");
                        setname.arg("SETNAME").arg(&context.client_name);
                        if let Err(error) = setname.query_async::<Value>(&mut inner).await {
                            tracing::debug!(%error, "CLIENT SETNAME refused on a cluster node connection");
                        }
                        context.redirects
                    }
                    None => Arc::default(),
                };
                Ok(CountingConnection { inner, redirects })
            })
            .boxed()
    }
}

// ---------------------------------------------------------------------------
// The connection factory: node map and per-node tunnels
// ---------------------------------------------------------------------------

/// Where the client dials each node the cluster names. Plugged into the
/// crate as its DNS resolver, so the advertised `host:port` keeps naming the
/// node in the slot map, in redirects and as the TLS server name, while the
/// socket goes wherever the node map and the profile's tunnel say.
pub struct NodeDialer {
    profile: ConnectionProfile,
    config: RedisClusterConfig,
    redirects: Arc<AtomicU64>,
    /// Advertised `host:port` → the `host:port` actually dialed, for the
    /// topology view.
    dialed: parking_lot::Mutex<BTreeMap<String, String>>,
}

impl NodeDialer {
    fn new(profile: &ConnectionProfile, redirects: Arc<AtomicU64>) -> Self {
        NodeDialer {
            profile: profile.clone(),
            config: profile.redis_cluster.clone().unwrap_or_default(),
            redirects,
            dialed: parking_lot::Mutex::new(BTreeMap::new()),
        }
    }

    /// The node map's rewrite of an advertised address (unchanged when the
    /// map does not name it).
    pub fn mapped(&self, host: &str, port: u16) -> (String, u16) {
        self.config
            .dial_for(host, port)
            .unwrap_or_else(|| (host.to_string(), port))
    }

    /// The `host:port` to open a socket to for an advertised node address:
    /// the node map first, then (for a tunneled profile) the node's own
    /// local forward.
    pub async fn dial_target(&self, host: &str, port: u16) -> StudioResult<(String, u16)> {
        let (host, port) = self.mapped(host, port);
        crate::ssh::TunnelManager::global()
            .endpoint_for_node(&self.profile, &host, port)
            .await
    }

    /// The profile the cluster connection was opened with.
    pub(crate) fn profile(&self) -> &ConnectionProfile {
        &self.profile
    }

    /// Advertised → dialed addresses seen so far.
    pub fn dialed(&self) -> BTreeMap<String, String> {
        self.dialed.lock().clone()
    }
}

impl redis::io::AsyncDNSResolver for NodeDialer {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        host: &'b str,
        port: u16,
    ) -> RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        async move {
            let _ = DIAL_COUNTER.try_with(|slot| {
                *slot.borrow_mut() = Some(DialContext {
                    redirects: self.redirects.clone(),
                    client_name: super::setname::client_name(&self.profile),
                })
            });
            let (dial_host, dial_port) = self.dial_target(host, port).await.map_err(|e| {
                redis::RedisError::from(std::io::Error::other(format!(
                    "cannot reach cluster node {host}:{port}: {e}"
                )))
            })?;
            self.dialed
                .lock()
                .insert(format!("{host}:{port}"), format!("{dial_host}:{dial_port}"));
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((dial_host.as_str(), dial_port))
                .await?
                .collect();
            Ok(Box::new(addrs.into_iter()) as Box<dyn Iterator<Item = SocketAddr> + Send + 'a>)
        }
        .boxed()
    }
}

// ---------------------------------------------------------------------------
// The client's connection: Single or Cluster
// ---------------------------------------------------------------------------

/// A cluster master as the slot map names it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeAddr {
    pub host: String,
    pub port: u16,
}

impl NodeAddr {
    pub fn label(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

/// The cluster half of [`RedisConn`].
#[derive(Clone)]
pub struct ClusterHandle {
    conn: ClusterConnection<CountingConnection>,
    redirects: Arc<AtomicU64>,
    dialer: Arc<NodeDialer>,
    /// The first seed's host, standing in for a node that advertises a
    /// wildcard or empty address (the crate does the same).
    seed_host: String,
    masters: Arc<parking_lot::Mutex<Vec<NodeAddr>>>,
}

impl ClusterHandle {
    /// `MOVED` / `ASK` redirects followed since the connection opened.
    pub fn redirects(&self) -> u64 {
        self.redirects.load(Ordering::Relaxed)
    }

    pub fn dialer(&self) -> &NodeDialer {
        &self.dialer
    }

    /// The first seed's host, which stands in for a node that advertises a
    /// wildcard or empty address.
    pub(crate) fn seed_host(&self) -> &str {
        &self.seed_host
    }

    /// The masters seen by the last [`RedisClient::cluster_masters`] call.
    pub fn known_master_count(&self) -> usize {
        self.masters.lock().len()
    }
}

/// The connection a [`RedisClient`] holds.
#[derive(Clone)]
pub enum RedisConn {
    Single(MultiplexedConnection),
    Cluster(ClusterHandle),
}

/// The first key of an internal pipeline command (for slot grouping):
/// the third argument for `MEMORY USAGE` / `OBJECT …` / `XINFO …`, none for
/// keyless commands, the second argument otherwise.
fn command_key(cmd: &Cmd) -> Option<Vec<u8>> {
    let args: Vec<&[u8]> = cmd
        .args_iter()
        .filter_map(|arg| match arg {
            redis::Arg::Simple(bytes) => Some(bytes),
            _ => None,
        })
        .collect();
    let name = String::from_utf8_lossy(args.first()?).to_ascii_uppercase();
    let index = match name.as_str() {
        "MEMORY" | "OBJECT" | "XINFO" | "XGROUP" => 2,
        "PING" | "INFO" | "DBSIZE" | "SCAN" | "CONFIG" | "CLIENT" | "CLUSTER" | "COMMAND"
        | "SLOWLOG" | "ACL" | "SELECT" | "ECHO" | "TIME" | "LASTSAVE" | "RANDOMKEY" | "KEYS"
        | "FLUSHDB" | "FLUSHALL" | "FT._LIST" | "MODULE" | "LATENCY" | "PUBLISH" | "SPUBLISH"
        | "EVAL" | "EVALSHA" | "FCALL" | "SCRIPT" | "FUNCTION" => return None,
        _ => 1,
    };
    args.get(index).map(|k| k.to_vec())
}

async fn split_pipeline(
    conn: ClusterConnection<CountingConnection>,
    pipeline: &Pipeline,
    offset: usize,
    count: usize,
) -> RedisResult<Vec<Value>> {
    if pipeline.is_transaction() {
        let mut conn = conn;
        return conn.req_packed_commands(pipeline, offset, count).await;
    }
    let commands: Vec<&Cmd> = pipeline.cmd_iter().collect();
    let mut by_slot: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    let mut keyless: Vec<usize> = Vec::new();
    for (i, cmd) in commands.iter().enumerate() {
        match command_key(cmd) {
            Some(key) => by_slot.entry(key_slot(&key)).or_default().push(i),
            None => keyless.push(i),
        }
    }
    let mut parts = Vec::with_capacity(by_slot.len() + keyless.len());
    for (slot, indexes) in by_slot {
        let mut part = redis::pipe();
        for &i in &indexes {
            part.add_command(commands[i].clone());
        }
        let mut conn = conn.clone();
        parts.push(
            async move {
                let n = indexes.len();
                let values = conn
                    .route_pipeline(
                        part,
                        0,
                        n,
                        SingleNodeRoutingInfo::SpecificNode(Route::new(slot, SlotAddr::Master)),
                    )
                    .await?;
                Ok::<_, redis::RedisError>((indexes, values))
            }
            .boxed(),
        );
    }
    for i in keyless {
        let mut conn = conn.clone();
        let cmd = commands[i].clone();
        parts.push(
            async move {
                let value = conn.req_packed_command(&cmd).await?;
                Ok((vec![i], vec![value]))
            }
            .boxed(),
        );
    }
    let mut out: Vec<Value> = vec![Value::Nil; commands.len()];
    for (indexes, values) in futures::future::try_join_all(parts).await? {
        for (i, value) in indexes.into_iter().zip(values) {
            out[i] = value;
        }
    }
    let end = (offset + count).min(out.len());
    Ok(out.drain(offset.min(end)..end).collect())
}

impl ConnectionLike for RedisConn {
    fn req_packed_command<'a>(&'a mut self, cmd: &'a Cmd) -> RedisFuture<'a, Value> {
        match self {
            RedisConn::Single(conn) => conn.req_packed_command(cmd),
            RedisConn::Cluster(handle) => handle.conn.req_packed_command(cmd),
        }
    }

    fn req_packed_commands<'a>(
        &'a mut self,
        pipeline: &'a Pipeline,
        offset: usize,
        count: usize,
    ) -> RedisFuture<'a, Vec<Value>> {
        match self {
            RedisConn::Single(conn) => conn.req_packed_commands(pipeline, offset, count),
            RedisConn::Cluster(handle) => {
                split_pipeline(handle.conn.clone(), pipeline, offset, count).boxed()
            }
        }
    }

    fn get_db(&self) -> i64 {
        match self {
            RedisConn::Single(conn) => conn.get_db(),
            RedisConn::Cluster(_) => 0,
        }
    }
}

impl RedisClient {
    /// The cluster half of the connection, `None` on a standalone server.
    /// (A cheap clone: the handle shares the routed connection.)
    pub fn cluster(&self) -> Option<ClusterHandle> {
        match &*self.conn.lock() {
            RedisConn::Single(_) => None,
            RedisConn::Cluster(handle) => Some(handle.clone()),
        }
    }

    /// True when commands are routed by slot.
    pub fn is_cluster(&self) -> bool {
        self.cluster().is_some()
    }

    /// `MOVED` / `ASK` redirects followed so far (always 0 standalone).
    pub fn redirect_count(&self) -> u64 {
        self.cluster().map(|h| h.redirects()).unwrap_or(0)
    }

    /// Run one command on one cluster node (by the address the slot map
    /// names), racing Cancel and the command timeout (see
    /// [`super::timeout`]). On a standalone client it is [`Self::run`].
    pub async fn run_on(&self, node: &NodeAddr, cmd: &Cmd) -> StudioResult<Value> {
        let Some(handle) = self.cluster() else {
            return self.run(cmd).await;
        };
        let mut conn = handle.conn.clone();
        let routing = RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
            host: node.host.clone(),
            port: node.port,
        });
        let args: Vec<Vec<u8>> = cmd
            .args_iter()
            .filter_map(|arg| match arg {
                redis::Arg::Simple(bytes) => Some(bytes.to_vec()),
                _ => None,
            })
            .collect();
        let budget = super::timeout::budget(self.response_timeout, &args);
        let reply = async {
            let routed = conn.route_command(cmd.clone(), routing);
            match budget {
                Some(limit) => tokio::time::timeout(limit, routed)
                    .await
                    .map_err(|_| {
                        super::timeout::timeout_error(limit, &super::timeout::command_label(&args))
                    })?
                    .map_err(redis_error),
                None => routed.await.map_err(redis_error),
            }
        };
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(StudioError::QueryCancelled),
            reply = reply => reply,
        }
    }

    /// The cluster's masters (from `CLUSTER SLOTS`, the source the crate's
    /// slot map uses), sorted by address. Empty on a standalone client.
    pub async fn cluster_masters(&self) -> StudioResult<Vec<NodeAddr>> {
        let Some(handle) = self.cluster() else {
            return Ok(Vec::new());
        };
        let mut cmd = redis::cmd("CLUSTER");
        cmd.arg("SLOTS");
        let mut conn = handle.conn.clone();
        let reply = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(StudioError::QueryCancelled),
            reply = conn.route_command(cmd, RoutingInfo::SingleNode(SingleNodeRoutingInfo::RandomPrimary)) => reply.map_err(redis_error)?,
        };
        let masters = parse_slot_masters(&reply, &handle.seed_host);
        *handle.masters.lock() = masters.clone();
        Ok(masters)
    }
}

/// The distinct masters a `CLUSTER SLOTS` reply names, sorted. A wildcard
/// or empty address stands for the answering node, for which `fallback`
/// (the seed host) is used, like the crate does.
pub fn parse_slot_masters(reply: &Value, fallback: &str) -> Vec<NodeAddr> {
    let mut out: Vec<NodeAddr> = Vec::new();
    let Value::Array(ranges) = reply else {
        return out;
    };
    for range in ranges {
        let Value::Array(items) = range else { continue };
        let Some(Value::Array(master)) = items.get(2) else {
            continue;
        };
        let host = match master.first() {
            Some(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
            Some(Value::SimpleString(s)) => s.clone(),
            _ => String::new(),
        };
        let host = if host.is_empty() || host == "0.0.0.0" || host == "::" {
            fallback.to_string()
        } else if host == "?" {
            continue;
        } else {
            host
        };
        let port = match master.get(1) {
            Some(Value::Int(n)) => *n as u16,
            _ => continue,
        };
        let node = NodeAddr { host, port };
        if !out.contains(&node) {
            out.push(node);
        }
    }
    out.sort();
    out
}

// ---------------------------------------------------------------------------
// Opening: the mode decision
// ---------------------------------------------------------------------------

fn mode_memo() -> &'static parking_lot::Mutex<HashMap<ConnectionId, bool>> {
    static MEMO: OnceLock<parking_lot::Mutex<HashMap<ConnectionId, bool>>> = OnceLock::new();
    MEMO.get_or_init(Default::default)
}

/// What `Auto` learned about a profile this session (`Some(true)` = a
/// cluster).
pub fn remembered_mode(profile: &ConnectionProfile) -> Option<bool> {
    mode_memo().lock().get(&profile.id).copied()
}

fn remember_mode(profile: &ConnectionProfile, cluster: bool) {
    mode_memo().lock().insert(profile.id, cluster);
}

/// True when a profile is set to `Cluster`, or is `Auto` and already known
/// to be a cluster this session. The live tools use it to decide whether to
/// fan out without opening a probe connection on a standalone server.
pub fn is_known_cluster(profile: &ConnectionProfile) -> bool {
    match cluster_mode(profile) {
        RedisClusterMode::Cluster => true,
        RedisClusterMode::Standalone => false,
        RedisClusterMode::Auto => remembered_mode(profile) == Some(true),
    }
}

/// Forget what `Auto` learned (the editor calls it after a profile edit).
pub fn forget_mode(profile: &ConnectionProfile) {
    mode_memo().lock().remove(&profile.id);
}

/// The profile a standalone connection uses: a server field listing several
/// seed hosts is cut to its first, so the tunnel and the client see one
/// host. Unchanged (borrowed) otherwise.
pub fn single_host(profile: &ConnectionProfile) -> Cow<'_, ConnectionProfile> {
    if !has_host_list(profile) {
        return Cow::Borrowed(profile);
    }
    let mut first = profile.clone();
    if let Some((host, port)) = seed_nodes(profile).into_iter().next() {
        first.server = host;
        first.port = port;
    }
    Cow::Owned(first)
}

/// The open-path hook before any standalone connect: a profile set to
/// `Cluster` (or `Auto` already known to be a cluster this session) opens
/// the routed client; everything else answers `None` and connects as
/// before.
pub(super) async fn open_if_cluster(
    profile: &ConnectionProfile,
) -> StudioResult<Option<RedisClient>> {
    let routed = match cluster_mode(profile) {
        RedisClusterMode::Cluster => true,
        RedisClusterMode::Standalone => false,
        RedisClusterMode::Auto => remembered_mode(profile) == Some(true),
    };
    if routed {
        open_cluster(profile).await.map(Some)
    } else {
        Ok(None)
    }
}

/// The open-path hook after a standalone connect answered `PING`: in `Auto`
/// mode, the first connect of the session asks `INFO cluster`; a cluster
/// node switches to the routed client, anything else (including a server
/// that refuses `INFO`) stays standalone and is never asked again.
pub(super) async fn switch_if_cluster(
    client: &RedisClient,
    profile: &ConnectionProfile,
) -> StudioResult<Option<RedisClient>> {
    if cluster_mode(profile) != RedisClusterMode::Auto || remembered_mode(profile).is_some() {
        return Ok(None);
    }
    let mut cmd = redis::cmd("INFO");
    cmd.arg("cluster");
    let enabled = match client.run(&cmd).await {
        Ok(Value::BulkString(b)) => cluster_enabled(&String::from_utf8_lossy(&b)),
        Ok(Value::VerbatimString { text, .. }) | Ok(Value::SimpleString(text)) => {
            cluster_enabled(&text)
        }
        _ => false,
    };
    remember_mode(profile, enabled);
    if enabled {
        open_cluster(profile).await.map(Some)
    } else {
        Ok(None)
    }
}

/// `cluster_enabled:1` in an `INFO cluster` reply.
pub fn cluster_enabled(info: &str) -> bool {
    info.lines()
        .any(|line| line.trim().eq_ignore_ascii_case("cluster_enabled:1"))
}

fn cluster_tls(profile: &ConnectionProfile) -> Option<(redis::TlsMode, bool)> {
    match profile.effective_tls_mode() {
        TlsMode::Disable => None,
        TlsMode::Require => Some((redis::TlsMode::Insecure, false)),
        TlsMode::VerifyCa => Some((redis::TlsMode::Secure, true)),
        // Like the standalone address: anything else verifies fully.
        _ => Some((redis::TlsMode::Secure, false)),
    }
}

/// Open the routed cluster client for a profile: every seed from the
/// server field, the profile's ACL user, TLS mode and certificates, and the
/// [`NodeDialer`] for every node connection. Verifies the cluster answers
/// before returning, and runs the profile's startup script.
///
/// Node connections ask for RESP3 like a standalone connection does, and
/// the whole cluster falls back to RESP2 when a node refuses `HELLO 3`.
pub async fn open_cluster(profile: &ConnectionProfile) -> StudioResult<RedisClient> {
    let client = connect_cluster_any(profile, None).await?;
    // Entra (Azure Redis): keep every node signed in as tokens rotate (see
    // [`rebuild_with_token`]).
    let client = super::with_entra_session(client, profile)?;
    let masters = client.cluster_masters().await.map_err(|e| match e {
        StudioError::QueryExecution(text) => StudioError::Connection(text),
        other => other,
    })?;
    if masters.is_empty() {
        return Err(StudioError::Connection(
            "the server answered as a cluster node but CLUSTER SLOTS names no masters".into(),
        ));
    }
    remember_mode(profile, true);
    super::apply_startup_script(&client, profile).await?;
    Ok(client)
}

/// The routed client on RESP3, or RESP2 when a node refuses `HELLO 3`.
async fn connect_cluster_any(
    profile: &ConnectionProfile,
    redirects: Option<Arc<AtomicU64>>,
) -> StudioResult<RedisClient> {
    match connect_cluster(profile, ProtocolVersion::RESP3, redirects.clone()).await {
        Err(Refusal::Hello) => connect_cluster(profile, ProtocolVersion::RESP2, redirects).await,
        other => other,
    }
    .map_err(Refusal::into_error)
}

/// A fresh routed connection for `handle`'s cluster, signed in with the
/// Entra `token`, keeping the redirect count. The client library signs each
/// node connection in with the password the routed client was built with
/// and reuses it when it reconnects to a node, so re-authenticating the
/// open node connections alone would not survive the next reconnect: the
/// Entra keeper rebuilds the routed connection with the new token and swaps
/// it in instead. Commands already running finish on the old connection.
pub(super) async fn rebuild_with_token(
    handle: &ClusterHandle,
    profile: &ConnectionProfile,
    token: &str,
) -> StudioResult<ClusterHandle> {
    let mut fresh = profile.clone();
    fresh.runtime_entra_access_token = Some(token.to_string());
    let client = connect_cluster_any(&fresh, Some(handle.redirects.clone())).await?;
    client.cluster().ok_or_else(|| {
        StudioError::Connection("the rebuilt cluster connection is not routed".into())
    })
}

/// One routed-client attempt on `protocol`, without the `CLUSTER SLOTS`
/// check or startup text. `redirects` carries an existing count over.
async fn connect_cluster(
    profile: &ConnectionProfile,
    protocol: ProtocolVersion,
    redirects: Option<Arc<AtomicU64>>,
) -> Result<RedisClient, Refusal> {
    if super::database_index(profile)? != 0 {
        return Err(StudioError::InvalidProfile(
            "A Redis Cluster has one logical database (0); clear the connection's database or set it to 0".into(),
        )
        .into());
    }
    let (user, password) = super::credentials(profile)?;
    let seeds = seed_nodes(profile);
    let Some((seed_host, seed_port)) = seeds.first().cloned() else {
        return Err(StudioError::InvalidProfile(
            "a Redis Cluster connection needs at least one seed host".into(),
        )
        .into());
    };
    let mut infos = Vec::with_capacity(seeds.len());
    for (host, port) in &seeds {
        infos.push(
            super::connection_addr(profile, host, *port)?
                .into_connection_info()
                .map_err(redis_error)?,
        );
    }
    let timeout = profile.timeout();
    let redirects = redirects.unwrap_or_default();
    let dialer = Arc::new(NodeDialer::new(profile, redirects.clone()));
    let mut builder = ClusterClientBuilder::new(infos)
        .connection_timeout(timeout)
        .retries(4)
        .use_protocol(protocol)
        .async_dns_resolver(SharedDialer(dialer.clone()));
    if let Some(user) = user {
        builder = builder.username(user);
    }
    if let Some(password) = password {
        builder = builder.password(password);
    }
    let tls = cluster_tls(profile);
    if let Some((mode, accept_any_host)) = tls {
        builder = builder.tls(mode);
        if accept_any_host {
            builder = builder.danger_accept_invalid_hostnames(true);
        }
        let certs = tls_certificates(profile, mode)?;
        if certs.root_cert.is_some() || certs.client_tls.is_some() {
            builder = builder.certs(certs);
        }
    }
    let client = builder.build().map_err(redis_error)?;
    let endpoint = format!("{seed_host}:{seed_port}");
    let conn = tokio::time::timeout(
        timeout * 2,
        client.get_async_generic_connection::<CountingConnection>(),
    )
    .await
    .map_err(|_| {
        StudioError::Connection(format!(
            "timed out connecting to the Redis Cluster at {endpoint} after {:?}",
            timeout * 2
        ))
    })?
    .map_err(|e| {
        if protocol.supports_resp3() && super::hello::hello_refused(&e) {
            return Refusal::Hello;
        }
        Refusal::Other(match redis_error(e) {
            StudioError::QueryExecution(text) => StudioError::Connection(text),
            other => other,
        })
    })?;
    Ok(RedisClient {
        conn: Arc::new(parking_lot::Mutex::new(RedisConn::Cluster(ClusterHandle {
            conn,
            redirects,
            dialer,
            seed_host,
            masters: Default::default(),
        }))),
        endpoint: parking_lot::Mutex::new(endpoint),
        sentinel: None,
        database: 0,
        current_database: Arc::new(parking_lot::Mutex::new(0)),
        cancel: CancellationToken::new(),
        analysis_cache: super::analyze::AnalysisCache::default(),
        protocol,
        tls: tls.is_some(),
        response_timeout: super::timeout::response_timeout(profile),
        tunnel_reestablished: false,
        entra: None,
        _entra_keeper: None,
        entra_profile: None,
    })
}

/// The root set (see [`super::trust`]) and client certificate a profile
/// names, for the cluster builder (the standalone client uses the same).
fn tls_certificates(
    profile: &ConnectionProfile,
    mode: redis::TlsMode,
) -> StudioResult<redis::TlsCertificates> {
    // Roots only matter when the mode verifies, as on a standalone server.
    let root_cert = if matches!(mode, redis::TlsMode::Secure) || profile.tls_ca_file.is_some() {
        super::trust::root_material(profile)?.0
    } else {
        None
    };
    let client_tls = match (&profile.tls_client_cert_file, &profile.tls_client_key_file) {
        (Some(cert), Some(key)) => Some(redis::ClientTlsConfig {
            client_cert: super::read_pem(cert, "client certificate")?,
            client_key: super::read_pem(key, "client key")?,
        }),
        (Some(_), None) => {
            return Err(StudioError::InvalidProfile(
                "a client certificate needs its key file".into(),
            ));
        }
        _ => None,
    };
    Ok(redis::TlsCertificates {
        client_tls,
        root_cert,
    })
}

/// The crate takes its resolver by value; this shares one [`NodeDialer`]
/// between the crate and the client so the topology view can read what
/// was dialed.
struct SharedDialer(Arc<NodeDialer>);

impl redis::io::AsyncDNSResolver for SharedDialer {
    fn resolve<'a, 'b: 'a>(
        &'a self,
        host: &'b str,
        port: u16,
    ) -> RedisFuture<'a, Box<dyn Iterator<Item = SocketAddr> + Send + 'a>> {
        self.0.resolve(host, port)
    }
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Byte-exact key arguments of a multi-key command, `None` when the command
/// takes at most one key (or is not known to take several).
pub fn multi_key_arguments(command: &RedisCommand) -> Option<Vec<&[u8]>> {
    let args = &command.args;
    let name = command.name();
    let all_from = |start: usize| -> Vec<&[u8]> {
        args.iter()
            .skip(start)
            .map(Vec::as_slice)
            .collect::<Vec<_>>()
    };
    let numkeys_at = |at: usize| -> Vec<&[u8]> {
        let n = command.arg_text(at).trim().parse::<usize>().unwrap_or(0);
        args.iter()
            .skip(at + 1)
            .take(n)
            .map(Vec::as_slice)
            .collect()
    };
    let keys: Vec<&[u8]> = match name.as_str() {
        "MGET" | "DEL" | "UNLINK" | "EXISTS" | "TOUCH" | "WATCH" | "SDIFF" | "SINTER"
        | "SUNION" | "SDIFFSTORE" | "SINTERSTORE" | "SUNIONSTORE" | "PFCOUNT" | "PFMERGE" => {
            all_from(1)
        }
        "RENAME" | "RENAMENX" | "SMOVE" | "LMOVE" | "RPOPLPUSH" | "BLMOVE" | "BRPOPLPUSH"
        | "COPY" | "GEOSEARCHSTORE" | "ZRANGESTORE" => {
            args.iter().skip(1).take(2).map(Vec::as_slice).collect()
        }
        "BLPOP" | "BRPOP" | "BZPOPMIN" | "BZPOPMAX" => args
            .iter()
            .skip(1)
            .take(args.len().saturating_sub(2))
            .map(Vec::as_slice)
            .collect(),
        "MSET" | "MSETNX" => args.iter().skip(1).step_by(2).map(Vec::as_slice).collect(),
        "EVAL" | "EVALSHA" | "EVAL_RO" | "EVALSHA_RO" | "FCALL" | "FCALL_RO" => numkeys_at(2),
        "ZUNION" | "ZINTER" | "ZDIFF" | "SINTERCARD" | "ZINTERCARD" | "LMPOP" | "ZMPOP" => {
            numkeys_at(1)
        }
        "BLMPOP" | "BZMPOP" => numkeys_at(2),
        "ZUNIONSTORE" | "ZINTERSTORE" | "ZDIFFSTORE" => {
            let mut keys = vec![args.get(1).map(Vec::as_slice).unwrap_or_default()];
            keys.extend(numkeys_at(2));
            keys
        }
        "BITOP" => all_from(2),
        "JSON.MGET" => args
            .iter()
            .skip(1)
            .take(args.len().saturating_sub(2))
            .map(Vec::as_slice)
            .collect(),
        "XREAD" | "XREADGROUP" => {
            let at = args
                .iter()
                .position(|a| a.eq_ignore_ascii_case(b"STREAMS"))?;
            let rest = &args[at + 1..];
            rest.iter()
                .take(rest.len() / 2)
                .map(Vec::as_slice)
                .collect()
        }
        _ => return None,
    };
    (keys.len() > 1).then_some(keys)
}

pub fn refusal_message(command: &RedisCommand) -> Option<sqlly_core::DisplayMessage> {
    let name = command.name();
    let fallback = refusal(command)?;
    let key = match name.as_str() {
        "KEYS" => "services.redis.cluster.refusal_keys",
        "SCAN" => "services.redis.cluster.refusal_scan",
        "SELECT" => "services.redis.cluster.refusal_select",
        "COPY" => "services.redis.cluster.refusal_copy",
        "MOVE" => "services.redis.cluster.refusal_move",
        "MULTI" | "EXEC" | "DISCARD" => "services.redis.cluster.refusal_transaction",
        _ => "services.redis.cluster.refusal_cross_slot",
    };
    let message = sqlly_core::DisplayMessage::new(key, fallback);
    Some(match key {
        "services.redis.cluster.refusal_select" => {
            message.with_arg("database_name", command.arg_text(1))
        }
        "services.redis.cluster.refusal_transaction" => message.with_arg("command_name", name),
        "services.redis.cluster.refusal_cross_slot" => {
            let keys = multi_key_arguments(command)?;
            let mut slots: Vec<u16> = keys.iter().map(|key| key_slot(key)).collect();
            slots.sort_unstable();
            slots.dedup();
            message
                .with_arg("command_name", name)
                .with_arg("slot_count", slots.len().to_string())
                .with_arg(
                    "slot_list",
                    slots
                        .iter()
                        .take(4)
                        .map(u16::to_string)
                        .collect::<Vec<_>>()
                        .join(", "),
                )
        }
        _ => message,
    })
}

/// English compatibility fallback; localized callers use [`refusal_message`].
pub fn refusal(command: &RedisCommand) -> Option<String> {
    let name = command.name();
    match name.as_str() {
        "KEYS" => {
            return Some(
                "KEYS would block every master while it walks the whole keyspace; on a cluster use the key browser or SQLLY.KEYS <pattern>, which scans each master in pages"
                    .into(),
            );
        }
        "SCAN" => {
            return Some(
                "a SCAN cursor belongs to one node, so on a cluster it cannot be continued from the editor; use SQLLY.KEYS <pattern> [limit [cursor]], which walks every master and returns a cursor that continues across them"
                    .into(),
            );
        }
        "SELECT" => {
            let db = command.arg_text(1);
            if db.trim() != "0" {
                return Some(format!(
                    "SELECT {} is refused: a cluster has one logical database (0)",
                    db.trim()
                ));
            }
            return None;
        }
        // The explorer's Copy to Database… writes `COPY key key DB n`.
        "COPY" => {
            let args = &command.args;
            if let Some(at) = args.iter().position(|a| a.eq_ignore_ascii_case(b"DB"))
                && args
                    .get(at + 1)
                    .is_some_and(|db| String::from_utf8_lossy(db).trim() != "0")
            {
                return Some(
                    "COPY … DB is refused: a cluster has one logical database (0); use Copy to Connection… to copy a key to another server"
                        .into(),
                );
            }
        }
        "MOVE" => {
            return Some(
                "MOVE is refused: a cluster has one logical database (0), so there is no other database to move a key to"
                    .into(),
            );
        }
        "MULTI" | "EXEC" | "DISCARD" => {
            return Some(format!(
                "{name} is not available on a cluster connection: each command is routed to its own slot's master, so a transaction cannot stay on one node; run the transaction with redis-cli -c against the node that owns its hash-tagged keys"
            ));
        }
        _ => {}
    }
    let keys = multi_key_arguments(command)?;
    let mut slots: Vec<u16> = keys.iter().map(|k| key_slot(k)).collect();
    slots.sort_unstable();
    slots.dedup();
    if slots.len() <= 1 {
        return None;
    }
    let shown: Vec<String> = slots.iter().take(4).map(u16::to_string).collect();
    let more = if slots.len() > 4 { ", …" } else { "" };
    Some(format!(
        "{name} names keys in {} different hash slots ({}{more}); a cluster runs a multi-key command only when every key maps to the same slot — give the keys a common {{hash tag}} such as user:{{42}}:a and user:{{42}}:b",
        slots.len(),
        shown.join(", ")
    ))
}

pub fn redirect_display_message(redirects: u64) -> Option<sqlly_core::DisplayMessage> {
    let key = if redirects == 1 {
        "services.redis.cluster.redirect_one"
    } else {
        "services.redis.cluster.redirect_many"
    };
    redirect_message(redirects).map(|fallback| {
        sqlly_core::DisplayMessage::new(key, fallback)
            .with_arg("redirect_count", redirects.to_string())
    })
}

/// English compatibility fallback; localized callers use [`redirect_display_message`].
pub fn redirect_message(redirects: u64) -> Option<String> {
    (redirects > 0).then(|| {
        format!(
            "Cluster: {redirects} MOVED/ASK redirect{} followed while this script ran (slots moved between nodes; the client's slot map is now up to date)",
            if redirects == 1 { "" } else { "s" }
        )
    })
}

// ---------------------------------------------------------------------------
// Fan-out
// ---------------------------------------------------------------------------

/// A cursor across masters: `host:port@cursor` names the master the walk
/// stopped on and that node's own `SCAN` cursor. `"0"` (or anything that
/// does not parse) starts at the first master.
pub fn encode_cluster_cursor(node: &NodeAddr, cursor: &str) -> String {
    format!("{}@{cursor}", node.label())
}

/// The master and node cursor a cluster cursor names.
pub fn decode_cluster_cursor(text: &str) -> Option<(NodeAddr, String)> {
    let (address, cursor) = text.trim().rsplit_once('@')?;
    let (host, port) = sqlly_core::redis_cluster::split_host_port(address, 0)?;
    if port == 0 {
        return None;
    }
    Some((
        NodeAddr {
            host: host.to_string(),
            port,
        },
        super::browse::normalize_cursor(cursor),
    ))
}

/// One master's raw `SCAN` walk from `cursor` until it returns to 0 or
/// `budget` keys were seen. Returns the keys and the cursor (`"0"` done).
async fn scan_node(
    client: &RedisClient,
    node: &NodeAddr,
    pattern: &str,
    budget: usize,
    cursor: &str,
) -> StudioResult<(Vec<Vec<u8>>, String)> {
    let mut cursor = cursor.to_string();
    let mut raw: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut cmd = redis::cmd("SCAN");
        cmd.arg(cursor.as_str());
        if !pattern.is_empty() && pattern != "*" {
            cmd.arg("MATCH").arg(pattern);
        }
        cmd.arg("COUNT")
            .arg(SCAN_COUNT.min(budget.saturating_sub(raw.len()).max(10)));
        let (next, keys) = super::browse::scan_page(client.run_on(node, &cmd).await?);
        raw.extend(keys);
        cursor = next;
        if cursor == "0" || raw.len() >= budget {
            return Ok((raw, cursor));
        }
    }
}

/// [`super::browse::scan_keys_from`] on a cluster: walk the masters in
/// address order, each from its own cursor, until `limit` keys were seen;
/// the listing's cursor is a cluster cursor ([`encode_cluster_cursor`]) a
/// further walk continues from, `"0"` once every master was walked. Keys
/// are probed for type and TTL through the slot-split pipeline.
pub async fn scan_keys_across(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
    start_cursor: &str,
) -> StudioResult<KeyListing> {
    let limit = limit.max(1);
    let masters = client.cluster_masters().await?;
    let (mut index, mut cursor) = match decode_cluster_cursor(start_cursor) {
        Some((node, cursor)) => match masters.iter().position(|m| *m == node) {
            Some(i) => (i, cursor),
            // The master is gone (failover, resharding): start over rather
            // than skip a node's keys.
            None => (0, "0".to_string()),
        },
        None => (0, "0".to_string()),
    };
    let mut raw: Vec<Vec<u8>> = Vec::new();
    let mut next_cursor = "0".to_string();
    while index < masters.len() {
        let (keys, node_cursor) = scan_node(
            client,
            &masters[index],
            pattern,
            limit - raw.len().min(limit - 1),
            &cursor,
        )
        .await?;
        raw.extend(keys);
        if node_cursor != "0" {
            next_cursor = encode_cluster_cursor(&masters[index], &node_cursor);
            break;
        }
        index += 1;
        cursor = "0".to_string();
        if raw.len() >= limit {
            if let Some(next) = masters.get(index) {
                next_cursor = encode_cluster_cursor(next, "0");
            }
            break;
        }
    }
    let complete = next_cursor == "0";
    let keys: Vec<KeyInfo> = super::browse::probe_keys(client, raw).await?;
    Ok(KeyListing {
        keys,
        complete,
        cursor: next_cursor,
    })
}

/// Keys only, up to `limit`, across masters (the memory sample's walk).
pub async fn scan_raw_across(
    client: &RedisClient,
    pattern: &str,
    limit: usize,
) -> StudioResult<Vec<Vec<u8>>> {
    let mut raw = Vec::new();
    for node in client.cluster_masters().await? {
        if raw.len() >= limit {
            break;
        }
        let (keys, _) = scan_node(client, &node, pattern, limit - raw.len(), "0").await?;
        raw.extend(keys);
    }
    raw.truncate(limit);
    Ok(raw)
}

/// Each master's `DBSIZE`, in master order.
pub async fn dbsize_per_master(client: &RedisClient) -> StudioResult<Vec<(NodeAddr, i64)>> {
    let mut out = Vec::new();
    for node in client.cluster_masters().await? {
        let n = match client.run_on(&node, &redis::cmd("DBSIZE")).await? {
            Value::Int(n) => n,
            _ => 0,
        };
        out.push((node, n));
    }
    Ok(out)
}

/// How many keys to sample on each master: `sample` split in proportion to
/// each master's key count, at least one key on every non-empty master
/// (while the budget lasts), never more than a master holds.
pub fn proportional_quotas(sizes: &[i64], sample: usize) -> Vec<usize> {
    let total: i64 = sizes.iter().map(|n| (*n).max(0)).sum();
    if total == 0 || sample == 0 {
        return vec![0; sizes.len()];
    }
    let mut quotas: Vec<usize> = sizes
        .iter()
        .map(|&n| {
            let n = n.max(0);
            if n == 0 {
                0
            } else {
                ((sample as f64 * n as f64 / total as f64).floor() as usize)
                    .max(1)
                    .min(n as usize)
            }
        })
        .collect();
    // Hand any rounding remainder to the largest masters.
    let mut used: usize = quotas.iter().sum();
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(sizes[i]));
    while used < sample {
        let mut grew = false;
        for &i in &order {
            if used >= sample {
                break;
            }
            if (quotas[i] as i64) < sizes[i].max(0) {
                quotas[i] += 1;
                used += 1;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    quotas
}

/// The memory analyzer's walk on a cluster: each master's share of
/// `sample` by `DBSIZE`, scanned on that master. Returns the listing (types
/// and TTLs probed), the summed key count, and whether every master's walk
/// covered it.
pub async fn sample_across(client: &RedisClient, sample: usize) -> StudioResult<(KeyListing, i64)> {
    let sizes = dbsize_per_master(client).await?;
    let counts: Vec<i64> = sizes.iter().map(|(_, n)| *n).collect();
    let quotas = proportional_quotas(&counts, sample);
    let mut raw = Vec::new();
    let mut complete = true;
    for ((node, _), quota) in sizes.iter().zip(quotas) {
        if quota == 0 {
            continue;
        }
        let (mut keys, cursor) = scan_node(client, node, "*", quota, "0").await?;
        complete &= cursor == "0" && keys.len() <= quota;
        keys.truncate(quota);
        raw.extend(keys);
    }
    let keys = super::browse::probe_keys(client, raw).await?;
    Ok((
        KeyListing {
            keys,
            complete,
            cursor: "0".to_string(),
        },
        counts.iter().sum(),
    ))
}

/// `INFO keyspace` summed over every master: database 0's keys, expiring
/// keys, and the key-weighted average TTL.
pub async fn keyspace_across(client: &RedisClient) -> StudioResult<super::info::KeyspaceLine> {
    let mut total = super::info::KeyspaceLine {
        db: 0,
        keys: 0,
        expires: 0,
        avg_ttl_ms: 0,
    };
    let mut weighted_ttl: i128 = 0;
    for node in client.cluster_masters().await? {
        let mut cmd = redis::cmd("INFO");
        cmd.arg("keyspace");
        let text = match client.run_on(&node, &cmd).await? {
            Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
            Value::VerbatimString { text, .. } | Value::SimpleString(text) => text,
            _ => String::new(),
        };
        for line in super::info::parse_keyspace(&text) {
            if line.db == 0 {
                total.keys += line.keys;
                total.expires += line.expires;
                weighted_ttl += line.avg_ttl_ms as i128 * line.expires.max(0) as i128;
            }
        }
    }
    if total.expires > 0 {
        total.avg_ttl_ms = (weighted_ttl / total.expires as i128) as i64;
    }
    Ok(total)
}

/// Append the `masters` column a cluster listing carries (standalone
/// listings keep their columns exactly).
pub fn with_master_count(client: &RedisClient, mut shaped: Shaped) -> Shaped {
    let Some(handle) = client.cluster() else {
        return shaped;
    };
    let n = handle.known_master_count() as i64;
    shaped.columns.push(shape::column("masters", "integer"));
    for row in &mut shaped.rows {
        row.push(CellValue::I64(n));
    }
    shaped
}

// ---------------------------------------------------------------------------
// Topology
// ---------------------------------------------------------------------------

/// One cluster node as the topology view shows it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TopologyNode {
    pub id: String,
    /// `master` or `replica`.
    pub role: String,
    /// `ip:port` the node announces.
    pub address: String,
    /// The endpoint clients are told to use (`cluster-preferred-endpoint-type`):
    /// the IP, the announced hostname, or `?`.
    pub endpoint: String,
    /// `cluster-announce-hostname`, when the node announces one.
    pub hostname: Option<String>,
    pub tls_port: Option<u16>,
    /// A replica's master id.
    pub master_id: Option<String>,
    pub slots: Vec<(u16, u16)>,
    /// `online`, `failed`, `loading`, or `suspected` (a `pfail` flag).
    pub health: String,
    pub replication_offset: Option<i64>,
    /// `connected` / `disconnected` (from `CLUSTER NODES`).
    pub link_state: Option<String>,
    pub flags: Option<String>,
}

impl TopologyNode {
    pub fn slot_count(&self) -> i64 {
        self.slots
            .iter()
            .map(|(a, b)| (*b as i64) - (*a as i64) + 1)
            .sum()
    }

    pub fn slots_text(&self) -> String {
        self.slots
            .iter()
            .map(|(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub(crate) fn port(&self) -> Option<u16> {
        self.address
            .rsplit_once(':')
            .and_then(|(_, p)| p.parse().ok())
    }

    pub(crate) fn host(&self) -> String {
        self.address
            .rsplit_once(':')
            .map(|(h, _)| h.trim_start_matches('[').trim_end_matches(']').to_string())
            .unwrap_or_default()
    }
}

/// Parse `CLUSTER NODES` text: one node per line, `id ip:port@cport[,hostname]
/// flags master ping pong epoch link slot…`. Importing/migrating slot
/// markers (`[…]`) are skipped.
pub fn parse_cluster_nodes(text: &str) -> Vec<TopologyNode> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let (address_part, hostname) = match fields[1].split_once(',') {
            Some((a, h)) => (a, Some(h.to_string()).filter(|h| !h.is_empty())),
            None => (fields[1], None),
        };
        let address = address_part
            .split_once('@')
            .map(|(a, _)| a)
            .unwrap_or(address_part)
            .to_string();
        let flags = fields[2];
        let flag_list: Vec<&str> = flags.split(',').collect();
        let role = if flag_list.contains(&"master") {
            "master"
        } else {
            "replica"
        };
        let health = if flag_list.contains(&"fail") {
            "failed"
        } else if flag_list.contains(&"pfail") {
            "suspected"
        } else {
            "online"
        };
        let master_id = Some(fields[3].to_string()).filter(|m| m != "-");
        let mut slots = Vec::new();
        for slot in &fields[8..] {
            if slot.starts_with('[') {
                continue;
            }
            let range = match slot.split_once('-') {
                Some((a, b)) => a.parse().ok().zip(b.parse().ok()),
                None => slot.parse().ok().map(|s| (s, s)),
            };
            if let Some(range) = range {
                slots.push(range);
            }
        }
        let endpoint = address
            .rsplit_once(':')
            .map(|(h, _)| h.to_string())
            .unwrap_or_default();
        out.push(TopologyNode {
            id: fields[0].to_string(),
            role: role.to_string(),
            address,
            endpoint,
            hostname,
            tls_port: None,
            master_id,
            slots,
            health: health.to_string(),
            replication_offset: None,
            link_state: Some(fields[7].to_string()),
            flags: Some(flags.to_string()),
        });
    }
    sort_topology(&mut out);
    out
}

fn text_value(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Some(s.clone()),
        Value::VerbatimString { text, .. } => Some(text.clone()),
        Value::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

fn int_value(value: &Value) -> Option<i64> {
    match value {
        Value::Int(n) => Some(*n),
        other => text_value(other).and_then(|t| t.parse().ok()),
    }
}

fn list_items(value: &Value) -> Vec<Value> {
    match value {
        Value::Array(items) | Value::Set(items) => items.clone(),
        _ => Vec::new(),
    }
}

/// Parse a `CLUSTER SHARDS` reply (Redis 7.0+; flat pairs under RESP2, maps
/// under RESP3): each shard's slot ranges go to its master; each node keeps
/// its id, role, ip/endpoint/hostname, ports, replication offset and health.
pub fn parse_cluster_shards(reply: &Value) -> Vec<TopologyNode> {
    let mut out = Vec::new();
    for shard in list_items(reply) {
        let slot_values: Vec<i64> = shape::find_field(&shard, "slots")
            .map(|v| list_items(&v).iter().filter_map(int_value).collect())
            .unwrap_or_default();
        let slots: Vec<(u16, u16)> = slot_values
            .chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| (c[0] as u16, c[1] as u16))
            .collect();
        let nodes = shape::find_field(&shard, "nodes")
            .map(|v| list_items(&v))
            .unwrap_or_default();
        let master_id = nodes.iter().find_map(|n| {
            (shape::find_field(n, "role")
                .and_then(|r| text_value(&r))
                .as_deref()
                == Some("master"))
            .then(|| shape::find_field(n, "id").and_then(|v| text_value(&v)))
            .flatten()
        });
        for node in &nodes {
            let field = |name: &str| shape::find_field(node, name);
            let role = field("role")
                .and_then(|v| text_value(&v))
                .unwrap_or_default();
            let role = if role == "master" {
                "master"
            } else {
                "replica"
            };
            let ip = field("ip").and_then(|v| text_value(&v)).unwrap_or_default();
            let port = field("port")
                .and_then(|v| int_value(&v))
                .or_else(|| field("tls-port").and_then(|v| int_value(&v)))
                .unwrap_or(0);
            out.push(TopologyNode {
                id: field("id").and_then(|v| text_value(&v)).unwrap_or_default(),
                role: role.to_string(),
                address: if ip.contains(':') {
                    format!("[{ip}]:{port}")
                } else {
                    format!("{ip}:{port}")
                },
                endpoint: field("endpoint")
                    .and_then(|v| text_value(&v))
                    .unwrap_or_else(|| ip.clone()),
                hostname: field("hostname")
                    .and_then(|v| text_value(&v))
                    .filter(|h| !h.is_empty()),
                tls_port: field("tls-port")
                    .and_then(|v| int_value(&v))
                    .map(|p| p as u16),
                master_id: if role == "master" {
                    None
                } else {
                    master_id.clone()
                },
                slots: if role == "master" {
                    slots.clone()
                } else {
                    Vec::new()
                },
                health: field("health")
                    .and_then(|v| text_value(&v))
                    .unwrap_or_else(|| "online".to_string()),
                replication_offset: field("replication-offset").and_then(|v| int_value(&v)),
                link_state: None,
                flags: None,
            });
        }
    }
    sort_topology(&mut out);
    out
}

/// Masters first (by first slot), each followed by its replicas.
fn sort_topology(nodes: &mut [TopologyNode]) {
    let first_slot: HashMap<String, u16> = nodes
        .iter()
        .filter(|n| n.role == "master")
        .map(|n| {
            (
                n.id.clone(),
                n.slots.first().map(|s| s.0).unwrap_or(SLOT_COUNT),
            )
        })
        .collect();
    nodes.sort_by(|a, b| {
        let group = |n: &TopologyNode| {
            let owner = if n.role == "master" {
                &n.id
            } else {
                n.master_id.as_ref().unwrap_or(&n.id)
            };
            first_slot.get(owner).copied().unwrap_or(SLOT_COUNT)
        };
        (group(a), a.role != "master", &a.address).cmp(&(group(b), b.role != "master", &b.address))
    });
}

/// `CLUSTER SHARDS` with link states merged in from `CLUSTER NODES`, or
/// `CLUSTER NODES` alone on servers before 7.0.
pub async fn topology(client: &RedisClient) -> StudioResult<Vec<TopologyNode>> {
    let mut nodes_cmd = redis::cmd("CLUSTER");
    nodes_cmd.arg("NODES");
    let nodes_text = match client.run(&nodes_cmd).await? {
        Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
        Value::VerbatimString { text, .. } | Value::SimpleString(text) => text,
        _ => String::new(),
    };
    let from_nodes = parse_cluster_nodes(&nodes_text);
    let mut shards_cmd = redis::cmd("CLUSTER");
    shards_cmd.arg("SHARDS");
    let mut nodes = match client.run(&shards_cmd).await {
        Ok(reply) => {
            let shards = parse_cluster_shards(&reply);
            if shards.is_empty() {
                from_nodes.clone()
            } else {
                shards
            }
        }
        Err(_) => from_nodes.clone(),
    };
    for node in &mut nodes {
        if let Some(known) = from_nodes.iter().find(|n| n.id == node.id) {
            node.link_state = known.link_state.clone();
            node.flags = known.flags.clone();
            if node.hostname.is_none() {
                node.hostname = known.hostname.clone();
            }
        }
    }
    Ok(nodes)
}

/// The dial address the client uses (or would use) for a node.
fn dial_text(client: &RedisClient, node: &TopologyNode) -> String {
    let Some(handle) = client.cluster() else {
        return node.address.clone();
    };
    let dialed = handle.dialer.dialed();
    let endpoint_key = node
        .port()
        .map(|p| format!("{}:{p}", node.endpoint))
        .unwrap_or_default();
    if let Some(dial) = dialed
        .get(&node.address)
        .or_else(|| dialed.get(&endpoint_key))
    {
        return dial.clone();
    }
    match node.port() {
        Some(port) => {
            let (h, p) = handle.dialer.mapped(&node.host(), port);
            format!("{h}:{p}")
        }
        None => node.address.clone(),
    }
}

/// `SQLLY.TOPOLOGY`: one row per node.
pub async fn topology_rows(client: &RedisClient) -> StudioResult<Shaped> {
    let Some(handle) = client.cluster() else {
        return Err(StudioError::QueryExecution(
            "SQLLY.TOPOLOGY needs a Redis Cluster connection; this server is standalone".into(),
        ));
    };
    let tls = cluster_tls(&handle.dialer.profile).is_some();
    let nodes = topology(client).await?;
    let rows = nodes
        .iter()
        .map(|n| {
            let opt =
                |v: &Option<String>| v.clone().map(CellValue::String).unwrap_or(CellValue::Null);
            // The name a TLS client sends: the endpoint the cluster tells
            // clients to use (a hostname when it announces one).
            let tls_name = tls.then(|| {
                n.hostname
                    .clone()
                    .filter(|_| n.endpoint == "?" || n.endpoint.is_empty())
                    .unwrap_or_else(|| n.endpoint.clone())
            });
            vec![
                CellValue::String(n.id.clone()),
                CellValue::String(n.role.clone()),
                CellValue::String(n.address.clone()),
                CellValue::String(n.endpoint.clone()),
                opt(&n.hostname),
                CellValue::String(dial_text(client, n)),
                opt(&tls_name),
                CellValue::String(n.slots_text()),
                CellValue::I64(n.slot_count()),
                CellValue::String(n.health.clone()),
                n.replication_offset
                    .map(CellValue::I64)
                    .unwrap_or(CellValue::Null),
                opt(&n.link_state),
                opt(&n.master_id),
                opt(&n.flags),
            ]
        })
        .collect();
    Ok(Shaped::new(&TOPOLOGY_COLUMNS, rows))
}

/// The columns `SQLLY.TOPOLOGY` answers with.
pub const TOPOLOGY_COLUMNS: [(&str, &str); 14] = [
    ("node_id", "string"),
    ("role", "string"),
    ("address", "string"),
    ("endpoint", "string"),
    ("hostname", "string"),
    ("dial_address", "string"),
    ("tls_name", "string"),
    ("slots", "string"),
    ("slot_count", "integer"),
    ("health", "string"),
    ("replication_offset", "integer"),
    ("link_state", "string"),
    ("master_id", "string"),
    ("flags", "string"),
];

/// `SQLLY.TRACE`: for every node, the staged connection trace a Test
/// Connection runs ([`super::diagnose`]) — resolve, tunnel / proxy (with the
/// node map), TCP, TLS (checked against the address the node advertises),
/// AUTH, HELLO / PING, SELECT and `CLUSTER INFO` — one row per stage,
/// stopping at a node's first failure so the row that failed names the
/// stage to fix.
pub async fn trace_rows(client: &RedisClient) -> StudioResult<Shaped> {
    let Some(handle) = client.cluster() else {
        return Err(StudioError::QueryExecution(
            "SQLLY.TRACE needs a Redis Cluster connection; this server is standalone".into(),
        ));
    };
    let profile = &handle.dialer.profile;
    let mut node_profile = single_host(profile).into_owned();
    node_profile.database = None;
    let nodes = topology(client).await?;
    let mut rows = Vec::new();
    for node in &nodes {
        let mut push = |stage: &str, outcome: super::diagnose::Outcome, detail: String| {
            let label = match outcome {
                super::diagnose::Outcome::Ok => "ok",
                super::diagnose::Outcome::Skipped => "skipped",
                super::diagnose::Outcome::Note => "note",
                super::diagnose::Outcome::Failed => "failed",
            };
            rows.push(vec![
                CellValue::String(node.id.clone()),
                CellValue::String(node.address.clone()),
                CellValue::String(stage.to_string()),
                CellValue::Bool(outcome != super::diagnose::Outcome::Failed),
                CellValue::String(label.to_string()),
                CellValue::String(detail),
            ]);
        };
        let tunnel_label = super::diagnose::Stage::Tunnel.label();
        let Some(port) = node.port() else {
            push(
                tunnel_label,
                super::diagnose::Outcome::Failed,
                "the node announces no port".into(),
            );
            continue;
        };
        let host = if node.endpoint.is_empty() || node.endpoint == "?" {
            node.host()
        } else {
            node.endpoint.clone()
        };
        let (mapped_host, mapped_port) = handle.dialer.mapped(&host, port);
        let (dial_host, dial_port) = match handle.dialer.dial_target(&host, port).await {
            Ok(target) => target,
            Err(e) => {
                push(
                    tunnel_label,
                    super::diagnose::Outcome::Failed,
                    super::diagnose::plain_text(&e),
                );
                continue;
            }
        };
        let mut route = format!("{host}:{port}");
        if (mapped_host.as_str(), mapped_port) != (host.as_str(), port) {
            route.push_str(&format!(" → node map → {mapped_host}:{mapped_port}"));
        }
        if (dial_host.as_str(), dial_port) != (mapped_host.as_str(), mapped_port) {
            route.push_str(&format!(" → tunnel → {dial_host}:{dial_port}"));
        }
        if route == format!("{host}:{port}") {
            route.push_str(" dialed directly");
        }
        let mut sink = |report: &super::diagnose::StageReport| {
            tracing::debug!(target: "sqlly::redis::trace", node = %node.address, "{}", report.line());
        };
        let trace = super::diagnose::trace_cluster_node(
            &node_profile,
            (&host, port),
            (&dial_host, dial_port),
            route,
            &mut sink,
        )
        .await;
        for report in &trace.stages {
            push(report.stage.label(), report.outcome, report.detail.clone());
        }
    }
    Ok(Shaped::new(
        &[
            ("node_id", "string"),
            ("address", "string"),
            ("stage", "string"),
            ("ok", "boolean"),
            ("outcome", "string"),
            ("detail", "string"),
        ],
        rows,
    ))
}

/// Every master of a profile's cluster, as `(advertised label, dial host,
/// dial port)`, for the live tools that need one dedicated connection per
/// master (Profiler, Pub/Sub). Empty for a standalone profile.
pub async fn master_dial_targets(
    profile: &ConnectionProfile,
) -> StudioResult<Vec<(String, String, u16)>> {
    let client = super::open_client(profile).await?;
    let Some(handle) = client.cluster() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for master in client.cluster_masters().await? {
        let (host, port) = handle.dialer.dial_target(&master.host, master.port).await?;
        out.push((master.label(), host, port));
    }
    Ok(out)
}

/// The master owning a key's slot, as `(advertised label, dial host, dial
/// port)` (the sharded Pub/Sub listener subscribes there).
pub async fn slot_owner_dial_target(
    profile: &ConnectionProfile,
    key: &[u8],
) -> StudioResult<Option<(String, String, u16)>> {
    let client = super::open_client(profile).await?;
    let Some(handle) = client.cluster() else {
        return Ok(None);
    };
    let mut cmd = redis::cmd("CLUSTER");
    cmd.arg("SLOTS");
    let reply = client.run(&cmd).await?;
    let slot = key_slot(key) as i64;
    let Value::Array(ranges) = &reply else {
        return Ok(None);
    };
    for range in ranges {
        let Value::Array(items) = range else { continue };
        let (Some(Value::Int(start)), Some(Value::Int(end))) = (items.first(), items.get(1)) else {
            continue;
        };
        if slot < *start || slot > *end {
            continue;
        }
        let masters = parse_slot_masters(&Value::Array(vec![range.clone()]), &handle.seed_host);
        if let Some(master) = masters.first() {
            let (host, port) = handle.dialer.dial_target(&master.host, master.port).await?;
            return Ok(Some((master.label(), host, port)));
        }
    }
    Ok(None)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};
    use sqlly_intel::query::safety::redis::parse_script;

    fn cmd(text: &str) -> RedisCommand {
        parse_script(text).unwrap().remove(0)
    }

    #[test]
    fn cluster_key_slots_match_cluster_keyslot() {
        // Values from `CLUSTER KEYSLOT` on a Redis 7 cluster.
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
        assert_eq!(key_slot(b"123456789"), 12739);
        assert_eq!(
            key_slot(b"{user1000}.following"),
            key_slot(b"{user1000}.followers")
        );
        assert_eq!(key_slot(b"{user1000}.x"), key_slot(b"user1000"));
        // An empty tag hashes the whole key.
        assert_eq!(
            key_slot(b"foo{}bar"),
            crc16_xmodem(b"foo{}bar") % SLOT_COUNT
        );
        assert_eq!(key_slot(b""), 0);
    }

    #[test]
    fn cluster_refuses_keys_scan_select_and_cross_slot_commands_by_rule() {
        assert!(refusal(&cmd("KEYS *")).unwrap().contains("SQLLY.KEYS"));
        assert!(refusal(&cmd("SCAN 0")).unwrap().contains("SQLLY.KEYS"));
        assert!(
            refusal(&cmd("SELECT 1"))
                .unwrap()
                .contains("cluster has one logical database")
        );
        assert!(refusal(&cmd("SELECT 0")).is_none());
        let mget = refusal(&cmd("MGET foo bar")).unwrap();
        assert!(mget.contains("12182") && mget.contains("5061"), "{mget}");
        assert!(mget.contains("hash tag"), "{mget}");
        assert!(refusal(&cmd("MGET {u}:a {u}:b")).is_none());
        assert!(refusal(&cmd("MSET {u}:a 1 {u}:b 2")).is_none());
        assert!(refusal(&cmd("MSET foo 1 bar 2")).is_some());
        assert!(refusal(&cmd("DEL foo")).is_none());
        assert!(refusal(&cmd("GET foo")).is_none());
        // The explorer's Copy to Database… and MOVE need a second database.
        assert!(
            refusal(&cmd("COPY k k DB 3"))
                .unwrap()
                .contains("Copy to Connection")
        );
        assert!(refusal(&cmd("COPY {k}a {k}b")).is_none());
        assert!(refusal(&cmd("COPY k k DB 0")).is_none());
        assert!(refusal(&cmd("MOVE k 2")).is_some());
        assert!(refusal(&cmd("EVAL \"return 1\" 2 foo bar")).is_some());
        assert!(refusal(&cmd("EVAL \"return 1\" 0")).is_none());
        assert!(refusal(&cmd("ZUNIONSTORE dst 2 foo bar")).is_some());
        assert!(refusal(&cmd("XREAD COUNT 1 STREAMS foo bar 0 0")).is_some());
        assert!(refusal(&cmd("XREAD COUNT 1 STREAMS foo 0")).is_none());
        assert!(refusal(&cmd("MULTI")).is_some());
    }

    #[test]
    fn cluster_cursors_round_trip_and_reject_plain_cursors() {
        let node = NodeAddr {
            host: "10.0.0.5".into(),
            port: 7001,
        };
        let text = encode_cluster_cursor(&node, "1234");
        assert_eq!(text, "10.0.0.5:7001@1234");
        assert_eq!(decode_cluster_cursor(&text), Some((node, "1234".into())));
        let v6 = NodeAddr {
            host: "fd00::1".into(),
            port: 7002,
        };
        assert_eq!(
            decode_cluster_cursor(&encode_cluster_cursor(&v6, "0")),
            Some((v6, "0".into()))
        );
        assert_eq!(decode_cluster_cursor("0"), None);
        assert_eq!(decode_cluster_cursor("42"), None);
        assert_eq!(decode_cluster_cursor(""), None);
    }

    #[test]
    fn cluster_sample_quotas_follow_each_masters_dbsize() {
        assert_eq!(proportional_quotas(&[100, 100, 200], 100), vec![25, 25, 50]);
        assert_eq!(proportional_quotas(&[0, 10, 0], 100), vec![0, 10, 0]);
        assert_eq!(proportional_quotas(&[1, 1_000_000], 10), vec![1, 9]);
        assert_eq!(proportional_quotas(&[0, 0], 10), vec![0, 0]);
        let q = proportional_quotas(&[333, 333, 334], 1000);
        assert_eq!(q.iter().sum::<usize>(), 1000);
    }

    #[test]
    fn cluster_enabled_reads_info_cluster() {
        assert!(cluster_enabled("# Cluster\r\ncluster_enabled:1\r\n"));
        assert!(!cluster_enabled("# Cluster\r\ncluster_enabled:0\r\n"));
        assert!(!cluster_enabled(""));
    }

    #[test]
    fn cluster_slot_masters_dedupe_sort_and_replace_wildcards() {
        let node = |host: &str, port: i64| {
            Value::Array(vec![
                Value::BulkString(host.as_bytes().to_vec()),
                Value::Int(port),
                Value::BulkString(b"id".to_vec()),
            ])
        };
        let reply = Value::Array(vec![
            Value::Array(vec![
                Value::Int(5461),
                Value::Int(10922),
                node("10.0.0.2", 7001),
            ]),
            Value::Array(vec![Value::Int(0), Value::Int(100), node("0.0.0.0", 7000)]),
            Value::Array(vec![
                Value::Int(101),
                Value::Int(5460),
                node("0.0.0.0", 7000),
            ]),
            Value::Array(vec![Value::Int(10923), Value::Int(16383), node("?", 7002)]),
        ]);
        assert_eq!(
            parse_slot_masters(&reply, "seed.example.com"),
            vec![
                NodeAddr {
                    host: "10.0.0.2".into(),
                    port: 7001
                },
                NodeAddr {
                    host: "seed.example.com".into(),
                    port: 7000
                },
            ]
        );
    }

    fn profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "c",
            "127.0.0.1",
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p.port = 7100;
        p
    }

    #[tokio::test]
    async fn cluster_dialer_applies_the_node_map_before_dialing() {
        let mut p = profile();
        p.redis_cluster = Some(RedisClusterConfig {
            mode: RedisClusterMode::Cluster,
            node_map: sqlly_core::redis_cluster::parse_node_map("10.0.0.5:7001 => 127.0.0.1:17001")
                .unwrap(),
        });
        let dialer = NodeDialer::new(&p, Arc::default());
        assert_eq!(
            dialer.dial_target("10.0.0.5", 7001).await.unwrap(),
            ("127.0.0.1".to_string(), 17001)
        );
        assert_eq!(
            dialer.dial_target("10.0.0.6", 7002).await.unwrap(),
            ("10.0.0.6".to_string(), 7002),
            "unmapped nodes dial as advertised on a direct profile"
        );
    }

    #[test]
    fn single_host_cuts_a_seed_list_to_its_first_host() {
        let mut p = profile();
        p.server = "10.0.0.1:7000,10.0.0.2:7001".into();
        let first = single_host(&p);
        assert_eq!(first.server, "10.0.0.1");
        assert_eq!(first.port, 7000);
        let plain = profile();
        assert!(matches!(single_host(&plain), Cow::Borrowed(_)));
    }

    #[test]
    fn redirect_message_only_when_something_moved() {
        assert_eq!(redirect_message(0), None);
        assert!(
            redirect_message(1)
                .unwrap()
                .contains("1 MOVED/ASK redirect followed")
        );
        assert!(
            redirect_message(3)
                .unwrap()
                .contains("3 MOVED/ASK redirects")
        );
    }

    #[test]
    fn internal_pipeline_keys_pick_the_key_argument() {
        let mut c = redis::cmd("MEMORY");
        c.arg("USAGE").arg("k1").arg("SAMPLES").arg(5);
        assert_eq!(command_key(&c), Some(b"k1".to_vec()));
        let mut t = redis::cmd("TYPE");
        t.arg("k2");
        assert_eq!(command_key(&t), Some(b"k2".to_vec()));
        assert_eq!(command_key(&redis::cmd("DBSIZE")), None);
    }

    // -----------------------------------------------------------------------
    // Live: the cluster fixture (`SQLLY_REDIS_CLUSTER_URL`, 3 masters + 3
    // replicas on 7100-7105). Keys are prefixed `wsb:` and removed again.
    // The tests share one lock: the failover test moves masters around.
    // -----------------------------------------------------------------------

    use sqlly_core::{EventBus, EventPayload};

    fn live_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
    }

    fn live_cluster_profile(name: &str, mode: RedisClusterMode) -> Option<ConnectionProfile> {
        let url = std::env::var("SQLLY_REDIS_CLUSTER_URL").ok()?;
        let mut profile = super::super::profile_from_url(name, &url).ok()?;
        profile.auto_wrap_rollback = false;
        profile.redis_cluster = Some(RedisClusterConfig {
            mode,
            node_map: Vec::new(),
        });
        Some(profile)
    }

    struct Run {
        sets: Vec<Vec<Vec<CellValue>>>,
        messages: Vec<String>,
        error: Option<String>,
    }

    async fn run_script(client: &RedisClient, profile: &ConnectionProfile, text: &str) -> Run {
        let events = EventBus::new(4096);
        let mut sub = events.subscribe();
        let (tx, mut rx) = sqlly_core::row_channel(64);
        let drain = tokio::spawn(async move {
            let mut by_index: BTreeMap<usize, Vec<Vec<CellValue>>> = BTreeMap::new();
            while let Some(batch) = rx.recv().await {
                by_index
                    .entry(batch.result_set_index)
                    .or_default()
                    .extend(batch.rows);
            }
            by_index
        });
        let result =
            super::super::execute_on_client(client, profile, text, &events, Some(&tx)).await;
        drop(tx);
        let mut by_index = drain.await.unwrap();
        let mut sets = 0;
        let mut messages = Vec::new();
        while let Ok(event) = sub.try_recv() {
            match event.payload {
                EventPayload::ResultSetStarted { .. } => sets += 1,
                EventPayload::Message { text, .. } => messages.push(text),
                _ => {}
            }
        }
        Run {
            sets: (0..sets)
                .map(|i| by_index.remove(&i).unwrap_or_default())
                .collect(),
            messages,
            error: result.err().map(|e| e.to_string()),
        }
    }

    async fn direct(port: u16) -> MultiplexedConnection {
        redis::Client::open(format!("redis://127.0.0.1:{port}"))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap()
    }

    async fn role_of(port: u16) -> String {
        let mut conn = direct(port).await;
        match redis::cmd("ROLE").query_async::<Value>(&mut conn).await {
            Ok(Value::Array(items)) => items.first().and_then(text_value).unwrap_or_default(),
            _ => String::new(),
        }
    }

    async fn wait_for_role(port: u16, role: &str) -> bool {
        for _ in 0..150 {
            if role_of(port).await == role {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        false
    }

    /// A replica whose replication link is up (`ROLE` → `slave … connected`),
    /// which a manual failover needs.
    async fn wait_replica_synced(port: u16) {
        for _ in 0..150 {
            let mut conn = direct(port).await;
            if let Ok(Value::Array(items)) =
                redis::cmd("ROLE").query_async::<Value>(&mut conn).await
                && items.first().and_then(text_value).as_deref() == Some("slave")
                && items.get(3).and_then(text_value).as_deref() == Some("connected")
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    /// Promote the replica on `port` with `CLUSTER FAILOVER`, retrying while
    /// a just-demoted node is still catching up.
    async fn promote(port: u16) -> bool {
        for _ in 0..4 {
            wait_replica_synced(port).await;
            let mut conn = direct(port).await;
            let mut failover = redis::cmd("CLUSTER");
            failover.arg("FAILOVER");
            let _ = failover.query_async::<Value>(&mut conn).await;
            if wait_for_role(port, "master").await {
                return true;
            }
        }
        false
    }

    async fn wait_cluster_ok(port: u16) {
        for _ in 0..150 {
            let mut conn = direct(port).await;
            let mut info = redis::cmd("CLUSTER");
            info.arg("INFO");
            if let Ok(value) = info.query_async::<Value>(&mut conn).await
                && text_value(&value).is_some_and(|t| t.contains("cluster_state:ok"))
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("cluster never returned to cluster_state:ok");
    }

    /// After a failover: wait until every node's own view (`CLUSTER NODES`
    /// on each of the six fixture nodes) shows three masters again, so the
    /// next test does not read a node that still lists the demoted master.
    async fn wait_views_agree() {
        for _ in 0..150 {
            let mut agreed = true;
            for port in 7100..=7105 {
                let mut conn = direct(port).await;
                let mut nodes = redis::cmd("CLUSTER");
                nodes.arg("NODES");
                let masters = match nodes.query_async::<Value>(&mut conn).await {
                    Ok(value) => text_value(&value)
                        .map(|t| {
                            parse_cluster_nodes(&t)
                                .iter()
                                .filter(|n| n.role == "master")
                                .count()
                        })
                        .unwrap_or(0),
                    Err(_) => 0,
                };
                if masters != 3 {
                    agreed = false;
                    break;
                }
            }
            if agreed {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    fn cell_text(cell: &CellValue) -> String {
        match cell {
            CellValue::String(s) => s.clone(),
            CellValue::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        }
    }

    /// Auto mode detects the cluster; keys on all three masters read back
    /// what was written; KEYS / SELECT 1 / a cross-slot MGET are refused by
    /// rule while a hash-tagged MGET runs; the topology lists all six nodes;
    /// the trace passes every stage; SQLLY.DATABASES has one database.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_routes_keys_refuses_by_rule_and_lists_topology() {
        let Some(profile) = live_cluster_profile("wsb-route", RedisClusterMode::Auto) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        forget_mode(&profile);
        let client = super::super::open_client(&profile).await.expect("client");
        assert!(client.is_cluster(), "Auto detects the cluster");
        assert_eq!(remembered_mode(&profile), Some(true));
        let masters = client.cluster_masters().await.unwrap();
        assert_eq!(masters.len(), 3, "{masters:?}");
        // Every node connection negotiated RESP3 and named itself.
        assert!(client.protocol().supports_resp3());
        let mut getname = redis::cmd("CLIENT");
        getname.arg("GETNAME");
        for master in &masters {
            let name = client.run_on(master, &getname).await.unwrap();
            assert_eq!(
                text_value(&name).as_deref(),
                Some(super::super::setname::client_name(&profile).as_str()),
                "{master:?}"
            );
        }
        // One key per master (slots 0-5460, 5461-10922, 10923-16383).
        let mut keys: Vec<String> = Vec::new();
        for want in [0u16, 1, 2] {
            let key = (0..)
                .map(|i| format!("wsb:route:{i}"))
                .find(|k| key_slot(k.as_bytes()) / 5462 == want)
                .unwrap();
            keys.push(key);
        }
        let mut script = String::new();
        for (i, key) in keys.iter().enumerate() {
            script.push_str(&format!("SET {key} v{i}\nGET {key}\n"));
        }
        let run = run_script(&client, &profile, &script).await;
        assert_eq!(run.error, None);
        for i in 0..3 {
            assert_eq!(cell_text(&run.sets[i * 2 + 1][0][0]), format!("v{i}"));
        }
        let cross = run_script(&client, &profile, &format!("MGET {} {}", keys[0], keys[1])).await;
        let error = cross.error.expect("cross-slot MGET refused");
        assert!(error.contains("different hash slots"), "{error}");
        let tagged = run_script(
            &client,
            &profile,
            "SET wsb:{t}:a 1\nSET wsb:{t}:b 2\nMGET wsb:{t}:a wsb:{t}:b",
        )
        .await;
        assert_eq!(tagged.error, None);
        assert_eq!(tagged.sets[2].len(), 2);
        let select = run_script(&client, &profile, "SELECT 1").await;
        assert!(
            select
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("cluster has one logical database"),
            "{:?}",
            select.error
        );
        let keys_cmd = run_script(&client, &profile, "KEYS wsb:*").await;
        assert!(keys_cmd.error.is_some());
        // Server-level tools still work against one master.
        let info = run_script(&client, &profile, "SQLLY.SERVERINFO").await;
        assert_eq!(info.error, None);
        let dbs = run_script(&client, &profile, "SQLLY.DATABASES").await;
        assert_eq!(dbs.error, None);
        assert_eq!(dbs.sets[0].len(), 1, "a cluster has one logical database");
        let topology = run_script(&client, &profile, "SQLLY.TOPOLOGY").await;
        assert_eq!(topology.error, None);
        let nodes = &topology.sets[0];
        assert_eq!(nodes.len(), 6, "{nodes:?}");
        let masters_listed = nodes
            .iter()
            .filter(|r| r[1] == CellValue::String("master".into()))
            .count();
        assert_eq!(masters_listed, 3);
        let slot_total: i64 = nodes
            .iter()
            .map(|r| match r[8] {
                CellValue::I64(n) => n,
                _ => 0,
            })
            .sum();
        assert_eq!(slot_total, SLOT_COUNT as i64, "masters cover every slot");
        let trace = run_script(&client, &profile, "SQLLY.TRACE").await;
        assert_eq!(trace.error, None);
        // The connection test's stages, Resolve through CLUSTER INFO (no
        // Session stage), for each of the six nodes, every one passing.
        assert_eq!(trace.sets[0].len(), 6 * 8, "{:?}", trace.sets[0]);
        assert!(
            trace.sets[0].iter().all(|r| r[3] == CellValue::Bool(true)),
            "{:?}",
            trace.sets[0]
        );
        let stages: Vec<String> = trace.sets[0][..8]
            .iter()
            .map(|r| cell_text(&r[2]))
            .collect();
        assert_eq!(
            stages,
            [
                "Resolve",
                "Tunnel / proxy",
                "TCP",
                "TLS",
                "AUTH",
                "HELLO / PING",
                "SELECT",
                "CLUSTER INFO"
            ]
        );
        assert!(
            trace.sets[0]
                .iter()
                .filter(|r| cell_text(&r[2]) == "CLUSTER INFO")
                .all(|r| cell_text(&r[5]).starts_with("cluster node: state ok")),
            "{:?}",
            trace.sets[0]
        );
        let mut cleanup = "DEL wsb:{t}:a wsb:{t}:b".to_string();
        for key in &keys {
            cleanup.push_str(&format!("\nDEL {key}"));
        }
        assert_eq!(run_script(&client, &profile, &cleanup).await.error, None);
    }

    /// The Redis Insight #6537 case: the client dials something other than
    /// what the nodes advertise. Each node gets a local TCP forward on a
    /// random port, the node map rewrites advertised → forward, and every
    /// master's traffic arrives through its forward.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_node_map_dials_every_node_through_its_mapped_address() {
        let Some(mut profile) = live_cluster_profile("wsb-map", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let mut lines = Vec::new();
        let mut accepted = Vec::new();
        for port in 7100u16..=7105 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let local = listener.local_addr().unwrap().port();
            let count = Arc::new(AtomicU64::new(0));
            accepted.push((port, local, count.clone()));
            tokio::spawn(async move {
                while let Ok((mut inbound, _)) = listener.accept().await {
                    count.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        if let Ok(mut outbound) =
                            tokio::net::TcpStream::connect(("127.0.0.1", port)).await
                        {
                            let _ =
                                tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                        }
                    });
                }
            });
            lines.push(format!("127.0.0.1:{port} => 127.0.0.1:{local}"));
        }
        profile.redis_cluster.as_mut().unwrap().node_map =
            sqlly_core::redis_cluster::parse_node_map(&lines.join("\n")).unwrap();
        let client = super::super::open_client(&profile).await.expect("client");
        let masters = client.cluster_masters().await.unwrap();
        let mut script = String::new();
        for i in 0..60 {
            script.push_str(&format!("SET wsb:map:{i} {i}\n"));
        }
        assert_eq!(run_script(&client, &profile, &script).await.error, None);
        for master in &masters {
            let (_, local, count) = accepted
                .iter()
                .find(|(port, _, _)| *port == master.port)
                .expect("a forward per node");
            assert!(
                count.load(Ordering::Relaxed) > 0,
                "master {} was not dialed through its forward 127.0.0.1:{local}",
                master.label()
            );
        }
        let topology = run_script(&client, &profile, "SQLLY.TOPOLOGY").await;
        let dialed: Vec<String> = topology.sets[0]
            .iter()
            .filter(|r| r[1] == CellValue::String("master".into()))
            .map(|r| cell_text(&r[5]))
            .collect();
        for dial in &dialed {
            assert!(
                accepted
                    .iter()
                    .any(|(_, local, _)| dial.ends_with(&format!(":{local}"))),
                "topology names the mapped dial address, got {dial}"
            );
        }
        let mut cleanup = String::new();
        for i in 0..60 {
            cleanup.push_str(&format!("DEL wsb:map:{i}\n"));
        }
        assert_eq!(run_script(&client, &profile, &cleanup).await.error, None);
    }

    /// A `CLUSTER FAILOVER` on one replica moves its master's slots; the
    /// client's next command for that slot is answered with MOVED, which
    /// the crate follows and the run reports as a redirect. The original
    /// master is failed back at the end so other suites see the fixture's
    /// usual masters.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_follows_moved_after_failover_and_reports_it() {
        let Some(profile) = live_cluster_profile("wsb-moved", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let client = super::super::open_client(&profile).await.expect("client");
        let key = "wsb:moved:k";
        let run = run_script(&client, &profile, &format!("SET {key} before")).await;
        assert_eq!(run.error, None);
        assert!(
            run.messages.is_empty(),
            "no redirects yet: {:?}",
            run.messages
        );
        // Find the key's master and one of its replicas from CLUSTER NODES.
        let mut seed = direct(7100).await;
        let mut nodes_cmd = redis::cmd("CLUSTER");
        nodes_cmd.arg("NODES");
        let text = text_value(&nodes_cmd.query_async::<Value>(&mut seed).await.unwrap()).unwrap();
        let nodes = parse_cluster_nodes(&text);
        let slot = key_slot(key.as_bytes());
        let master = nodes
            .iter()
            .find(|n| n.role == "master" && n.slots.iter().any(|(a, b)| *a <= slot && slot <= *b))
            .expect("slot owner")
            .clone();
        let replica = nodes
            .iter()
            .find(|n| n.master_id.as_deref() == Some(master.id.as_str()))
            .expect("the owner has a replica")
            .clone();
        let master_port = master.port().unwrap();
        let replica_port = replica.port().unwrap();
        assert!(promote(replica_port).await, "the replica took over");
        assert!(wait_for_role(master_port, "slave").await);
        wait_cluster_ok(7100).await;
        let before = client.redirect_count();
        let run = run_script(&client, &profile, &format!("GET {key}")).await;
        let after = client.redirect_count();
        // Fail back before asserting, so a failure cannot leave it moved.
        let restored = promote(master_port).await;
        wait_for_role(replica_port, "slave").await;
        wait_cluster_ok(7100).await;
        wait_views_agree().await;
        assert!(restored, "the original master {master_port} was restored");
        assert_eq!(run.error, None);
        assert_eq!(cell_text(&run.sets[0][0][0]), "before");
        assert!(
            after > before,
            "the MOVED reply was counted ({before} → {after})"
        );
        assert!(
            run.messages
                .iter()
                .any(|m| m.contains("MOVED/ASK redirect")),
            "{:?}",
            run.messages
        );
        assert_eq!(
            run_script(&client, &profile, &format!("DEL {key}"))
                .await
                .error,
            None
        );
    }

    /// 30,000 keys spread over the masters: the browser's walk stops at its
    /// 10,000 budget with a cluster cursor and the `masters` column, the
    /// continuation covers every key exactly once, the memory analyzer's
    /// sample spans the masters, and slot-grouped UNLINK batches delete
    /// everything with no CROSSSLOT failure.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_fans_out_listing_analysis_and_slot_grouped_deletes() {
        let Some(profile) = live_cluster_profile("wsb-fanout", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let client = super::super::open_client(&profile).await.expect("client");
        const TOTAL: usize = 30_000;
        for chunk in (0..TOTAL).collect::<Vec<_>>().chunks(5_000) {
            let mut pipe = redis::pipe();
            for i in chunk {
                pipe.cmd("SET").arg(format!("wsb:fan:{i}")).arg("v");
            }
            client.run_pipeline(&pipe).await.expect("seed");
        }
        let mut seen: std::collections::HashSet<String> = Default::default();
        let mut cursor = "0".to_string();
        let mut walks = 0;
        loop {
            let run = run_script(
                &client,
                &profile,
                &format!(
                    "SQLLY.KEYS wsb:fan:* {} {cursor}",
                    super::super::browse::DEFAULT_KEY_LIMIT
                ),
            )
            .await;
            assert_eq!(run.error, None);
            let rows = &run.sets[0];
            walks += 1;
            assert!(!rows.is_empty());
            assert_eq!(
                rows[0].len(),
                6,
                "a cluster listing carries the masters column"
            );
            assert_eq!(rows[0][5], CellValue::I64(3));
            for row in rows {
                assert!(
                    seen.insert(cell_text(&row[0])),
                    "walk {walks} re-listed a key"
                );
            }
            let next = cell_text(&rows[0][4]);
            if rows[0][3] == CellValue::Bool(true) {
                assert_eq!(next, "0");
                break;
            }
            if walks == 1 {
                assert!(rows.len() >= 10_000 && rows.len() < TOTAL, "{}", rows.len());
            }
            assert!(
                next.contains('@'),
                "a cluster cursor names its master: {next}"
            );
            cursor = next;
            assert!(walks < 10, "the walk never completed");
        }
        assert_eq!(seen.len(), TOTAL, "every key listed exactly once");
        let summary = run_script(
            &client,
            &profile,
            "SQLLY.MEMSUMMARY 3000\nSQLLY.TOPKEYS 3000 5",
        )
        .await;
        assert_eq!(summary.error, None);
        let total = match summary.sets[0][0][2] {
            CellValue::I64(n) => n,
            ref other => panic!("{other:?}"),
        };
        assert!(total >= TOTAL as i64, "DBSIZE summed over masters: {total}");
        assert_eq!(
            summary.sets[0][0][1],
            CellValue::I64(3000),
            "the whole sample was taken"
        );
        // Delete in slot-grouped batches of at most 500 keys.
        let mut keys: Vec<Vec<u8>> = seen.into_iter().map(String::into_bytes).collect();
        keys.sort_by_key(|k| key_slot(k));
        let mut failures = Vec::new();
        let mut deleted = 0;
        let mut start = 0;
        while start < keys.len() {
            let slot = key_slot(&keys[start]);
            let mut end = start;
            while end < keys.len() && end - start < 500 && key_slot(&keys[end]) == slot {
                end += 1;
            }
            let line = std::iter::once("UNLINK".to_string())
                .chain(
                    keys[start..end]
                        .iter()
                        .map(|k| String::from_utf8_lossy(k).into_owned()),
                )
                .collect::<Vec<_>>()
                .join(" ");
            let run = run_script(&client, &profile, &line).await;
            match run.error {
                Some(e) => failures.push(e),
                None => {
                    if let CellValue::I64(n) = run.sets[0][0][0] {
                        deleted += n;
                    }
                }
            }
            start = end;
        }
        assert!(
            failures.iter().all(|f| !f.contains("CROSSSLOT")),
            "{failures:?}"
        );
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(deleted, TOTAL as i64);
    }

    /// The owner label (`host:port`) of a key's slot, from CLUSTER NODES.
    async fn owner_label(key: &[u8]) -> String {
        let mut seed = direct(7100).await;
        let mut nodes_cmd = redis::cmd("CLUSTER");
        nodes_cmd.arg("NODES");
        let text = text_value(&nodes_cmd.query_async::<Value>(&mut seed).await.unwrap()).unwrap();
        let slot = key_slot(key);
        parse_cluster_nodes(&text)
            .into_iter()
            .find(|n| n.role == "master" && n.slots.iter().any(|(a, b)| *a <= slot && slot <= *b))
            .map(|n| n.address)
            .expect("slot owner")
    }

    /// One key per master, named `{prefix}{i}`.
    fn key_per_master(prefix: &str) -> Vec<String> {
        [0u16, 1, 2]
            .into_iter()
            .map(|want| {
                (0..)
                    .map(|i| format!("{prefix}{i}"))
                    .find(|k| key_slot(k.as_bytes()) / 5462 == want)
                    .unwrap()
            })
            .collect()
    }

    /// Sharded channels on three masters: each `SPUBLISH` arrives with the
    /// node that owns the channel's slot.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_sharded_pubsub_tags_messages_with_the_owning_node() {
        let Some(profile) = live_cluster_profile("wsb-spub", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let channels = key_per_master("wsb:ch:");
        let targets: Vec<super::super::live::SubscribeTarget> = channels
            .iter()
            .map(|c| super::super::live::SubscribeTarget::Sharded(c.clone()))
            .chain(std::iter::once(
                super::super::live::SubscribeTarget::Channel("wsb:plain".into()),
            ))
            .collect();
        let mut listener = super::super::live::open_pubsub(&profile, &targets)
            .await
            .expect("listener");
        let client = super::super::open_client(&profile).await.expect("client");
        let mut script = String::new();
        for (i, channel) in channels.iter().enumerate() {
            script.push_str(&format!("SPUBLISH {channel} m{i}\n"));
        }
        script.push_str("PUBLISH wsb:plain hello\n");
        assert_eq!(run_script(&client, &profile, &script).await.error, None);
        let mut got = Vec::new();
        while got.len() < channels.len() + 1 {
            let message =
                tokio::time::timeout(std::time::Duration::from_secs(5), listener.next_message())
                    .await
                    .expect("a message within 5s")
                    .expect("the listener is open");
            got.push(message);
        }
        for channel in &channels {
            let message = got
                .iter()
                .find(|m| &m.channel == channel)
                .expect("each sharded channel delivered");
            assert_eq!(
                message.node.as_deref(),
                Some(owner_label(channel.as_bytes()).await.as_str()),
                "{channel}"
            );
        }
        let plain = got
            .iter()
            .find(|m| m.channel == "wsb:plain")
            .expect("plain");
        assert!(
            plain.node.is_some(),
            "plain channels name the master they came from"
        );
    }

    /// MONITOR on every master: writes to keys on two different masters
    /// show up tagged with each key's own master.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_monitor_interleaves_masters_with_node_tags() {
        let Some(profile) = live_cluster_profile("wsb-mon", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let mut capture = super::super::live::open_monitor(&profile)
            .await
            .expect("monitor");
        let client = super::super::open_client(&profile).await.expect("client");
        let keys = key_per_master("wsb:mon:");
        let mut script = String::new();
        for key in &keys {
            script.push_str(&format!("SET {key} 1\nDEL {key}\n"));
        }
        assert_eq!(run_script(&client, &profile, &script).await.error, None);
        let mut seen: BTreeMap<String, String> = BTreeMap::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while seen.len() < keys.len() && std::time::Instant::now() < deadline {
            let Ok(Some(frame)) =
                tokio::time::timeout(std::time::Duration::from_secs(5), capture.next_frame()).await
            else {
                break;
            };
            if let super::super::live::MonitorFrame::NodeLine { node, line } = frame {
                for key in &keys {
                    if line.contains(&format!("\"SET\" \"{key}\"")) {
                        seen.insert(key.clone(), node.clone());
                    }
                }
            }
        }
        assert_eq!(
            seen.len(),
            keys.len(),
            "every master's SET was captured: {seen:?}"
        );
        let mut nodes: Vec<&String> = seen.values().collect();
        nodes.sort();
        nodes.dedup();
        assert_eq!(nodes.len(), 3, "three different masters: {seen:?}");
        for (key, node) in &seen {
            assert_eq!(node, &owner_label(key.as_bytes()).await, "{key}");
        }
    }

    /// Sharded Pub/Sub on a standalone Redis 7 server: `#name` subscribes
    /// with SSUBSCRIBE and the message carries no node.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_standalone_sharded_pubsub_receives_spublish() {
        let Some(profile) = super::super::live_profile_from_env("wsb-spub-standalone") else {
            eprintln!("skipping: SQLLY_REDIS_URL not set");
            return;
        };
        let targets = super::super::live::parse_subscribe_targets("#wsb:shard");
        let mut listener = super::super::live::open_pubsub(&profile, &targets)
            .await
            .expect("listener");
        let client = super::super::open_client(&profile).await.expect("client");
        assert!(!client.is_cluster());
        client
            .run_args(&[b"SPUBLISH".to_vec(), b"wsb:shard".to_vec(), b"hi".to_vec()])
            .await
            .expect("spublish");
        let message =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.next_message())
                .await
                .expect("a message within 5s")
                .expect("open");
        assert_eq!(message.channel, "wsb:shard");
        assert_eq!(
            message.payload,
            super::super::live::PubSubPayload::Text("hi".into())
        );
        assert_eq!(message.node, None);
    }

    /// `SQLLY.ON` (Redis Insight #2809): DBSIZE on every master and on every
    /// replica answers once per node with node and role columns; one
    /// replica's address answers `INFO replication` as that replica; one
    /// master's address answers exactly what its row in the masters grid
    /// says; KEYS runs on chosen nodes; a replica serves a key read; no
    /// redirect is followed (the masters that do not own the key answer
    /// MOVED in their own rows); and the read-only policy still refuses a
    /// FLUSHALL under the prefix before anything runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_runs_commands_on_chosen_nodes() {
        let Some(profile) = live_cluster_profile("wsb-on", RedisClusterMode::Cluster) else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let client = super::super::open_client(&profile).await.expect("client");
        assert!(client.is_cluster());
        let query = |text: String| {
            let client = &client;
            async move {
                super::super::query_rows_on_client(client, &text)
                    .await
                    .unwrap_or_else(|e| panic!("{text}: {e}"))
            }
        };
        let names = |set: &sqlly_core::MaterializedResultSet| -> Vec<String> {
            set.columns.iter().map(|c| c.name.clone()).collect()
        };

        let masters = query("SQLLY.ON masters DBSIZE".into()).await;
        assert_eq!(names(&masters), vec!["node", "role", "result"]);
        assert_eq!(masters.rows.len(), 3, "{:?}", masters.rows);
        assert!(
            masters
                .rows
                .iter()
                .all(|r| r[1] == CellValue::String("master".into()))
        );
        let mut master_nodes: Vec<String> = masters.rows.iter().map(|r| cell_text(&r[0])).collect();
        master_nodes.sort();
        master_nodes.dedup();
        assert_eq!(master_nodes.len(), 3, "{master_nodes:?}");
        let replicas = query("sqlly.on REPLICAS dbsize".into()).await;
        assert_eq!(replicas.rows.len(), 3, "{:?}", replicas.rows);
        assert!(
            replicas
                .rows
                .iter()
                .all(|r| r[1] == CellValue::String("replica".into()))
        );
        assert_eq!(query("SQLLY.ON all PING".into()).await.rows.len(), 6);

        // One master by address: that node's own answer, no node column.
        let first = &masters.rows[0];
        let single = query(format!("SQLLY.ON {} DBSIZE", cell_text(&first[0]))).await;
        assert_eq!(names(&single), vec!["result"]);
        assert_eq!(single.rows, vec![vec![first[2].clone()]]);

        // One replica by address answers INFO replication as a replica.
        let nodes = topology(&client).await.expect("topology");
        let replica = nodes.iter().find(|n| n.role == "replica").expect("replica");
        let info = query(format!("SQLLY.ON {} INFO replication", replica.address)).await;
        assert!(!names(&info).contains(&"node".to_string()));
        let cells: Vec<String> = info.rows.iter().flatten().map(cell_text).collect();
        assert!(cells.iter().any(|c| c == "slave"), "{cells:?}");
        assert!(cells.iter().any(|c| c == "master_link_status"), "{cells:?}");

        // KEYS is refused by cluster routing but runs on chosen nodes.
        let keys = query("SQLLY.ON masters KEYS wsb:on:*".into()).await;
        assert!(names(&keys).starts_with(&["node".to_string(), "role".to_string()]));

        // A key on its owner: its replica serves the read (READONLY), and
        // the other masters answer MOVED in their own rows.
        let key = "wsb:on:probe";
        let run = run_script(&client, &profile, &format!("SET {key} here")).await;
        assert_eq!(run.error, None);
        let owner = nodes
            .iter()
            .find(|n| {
                n.role == "master"
                    && n.slots
                        .iter()
                        .any(|(a, b)| (*a..=*b).contains(&key_slot(key.as_bytes())))
            })
            .expect("owner");
        let owner_replica = nodes
            .iter()
            .find(|n| n.master_id.as_deref() == Some(owner.id.as_str()))
            .expect("owner's replica");
        let mut replica_read = String::new();
        for _ in 0..50 {
            let read = query(format!("SQLLY.ON {} GET {key}", owner_replica.address)).await;
            replica_read = read
                .rows
                .first()
                .map(|r| cell_text(&r[0]))
                .unwrap_or_default();
            if replica_read == "here" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert_eq!(replica_read, "here", "the replica serves the key read");
        let spread = query(format!("SQLLY.ON masters GET {key}")).await;
        assert_eq!(names(&spread), vec!["node", "role", "result", "error"]);
        for row in &spread.rows {
            if cell_text(&row[0]) == owner.address {
                assert_eq!(cell_text(&row[2]), "here");
                assert_eq!(row[3], CellValue::Null);
            } else {
                assert!(
                    cell_text(&row[3]).to_ascii_uppercase().contains("MOVED"),
                    "{row:?}"
                );
            }
        }

        // The prefix does not hide a write from the connection policy.
        let mut read_only = profile.clone();
        read_only.enforce_read_only = true;
        let refused = run_script(&client, &read_only, "SQLLY.ON masters FLUSHALL").await;
        let error = refused
            .error
            .expect("FLUSHALL refused on a read-only connection");
        assert!(error.contains("FLUSHALL"), "{error}");
        assert_eq!(
            cell_text(&query(format!("GET {key}")).await.rows[0][0]),
            "here"
        );

        let run = run_script(&client, &profile, &format!("DEL {key}")).await;
        assert_eq!(run.error, None);
    }

    /// `CLUSTER NODES` text from a 3-master / 3-replica cluster whose second
    /// master announces a hostname (and has a suspected-failure flag on a
    /// replica), and the same cluster as `CLUSTER SHARDS` (RESP2 flat pairs).
    pub(crate) const NODES_FIXTURE: &str = "\
9e99 10.0.0.2:7001@17001,node-b.example.com master - 0 1792441110000 2 connected 5461-10922
50bb 10.0.0.5:7004@17004 slave,pfail f7dd 0 1792441110771 3 connected
1017 10.0.0.6:7005@17005 slave 5165 0 1792441111274 1 connected
f7dd 10.0.0.3:7002@17002 master - 0 1792441109765 3 connected 10923-16383
1d78 10.0.0.4:7003@17003 slave 9e99 0 1792441110268 2 connected
5165 10.0.0.1:7000@17000 myself,master - 0 1792441111000 1 connected 0-5460 [5461->-9e99]
";

    fn shards_fixture() -> Value {
        let bulk = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let node = |id: &str, ip: &str, port: i64, role: &str, health: &str, host: Option<&str>| {
            let mut items = vec![
                bulk("id"),
                bulk(id),
                bulk("port"),
                Value::Int(port),
                bulk("ip"),
                bulk(ip),
                bulk("endpoint"),
                bulk(ip),
            ];
            if let Some(host) = host {
                items.extend([bulk("hostname"), bulk(host)]);
            }
            items.extend([
                bulk("role"),
                bulk(role),
                bulk("replication-offset"),
                Value::Int(5908),
                bulk("health"),
                bulk(health),
            ]);
            Value::Array(items)
        };
        let shard = |start: i64, end: i64, nodes: Vec<Value>| {
            Value::Array(vec![
                bulk("slots"),
                Value::Array(vec![Value::Int(start), Value::Int(end)]),
                bulk("nodes"),
                Value::Array(nodes),
            ])
        };
        Value::Array(vec![
            shard(
                5461,
                10922,
                vec![
                    node(
                        "9e99",
                        "10.0.0.2",
                        7001,
                        "master",
                        "online",
                        Some("node-b.example.com"),
                    ),
                    node("1d78", "10.0.0.4", 7003, "replica", "online", None),
                ],
            ),
            shard(
                10923,
                16383,
                vec![
                    node("f7dd", "10.0.0.3", 7002, "master", "online", None),
                    node("50bb", "10.0.0.5", 7004, "replica", "online", None),
                ],
            ),
            shard(
                0,
                5460,
                vec![
                    node("5165", "10.0.0.1", 7000, "master", "online", None),
                    node("1017", "10.0.0.6", 7005, "replica", "online", None),
                ],
            ),
        ])
    }

    #[test]
    fn topology_shards_and_nodes_fixtures_shape_the_same_rows() {
        let from_nodes = parse_cluster_nodes(NODES_FIXTURE);
        let from_shards = parse_cluster_shards(&shards_fixture());
        assert_eq!(from_nodes.len(), 6);
        let key = |n: &TopologyNode| {
            (
                n.id.clone(),
                n.role.clone(),
                n.address.clone(),
                n.slots.clone(),
                n.master_id.clone(),
                n.hostname.clone(),
            )
        };
        assert_eq!(
            from_nodes.iter().map(key).collect::<Vec<_>>(),
            from_shards.iter().map(key).collect::<Vec<_>>(),
            "both sources give the same nodes, roles, addresses, slots, masters and hostnames, in the same order"
        );
        // Masters first by slot, each followed by its replica.
        let order: Vec<&str> = from_nodes.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(order, ["5165", "1017", "9e99", "1d78", "f7dd", "50bb"]);
        assert_eq!(
            from_nodes[0].slots_text(),
            "0-5460",
            "migration markers skipped"
        );
        assert_eq!(
            from_nodes.iter().map(TopologyNode::slot_count).sum::<i64>(),
            SLOT_COUNT as i64
        );
        assert_eq!(
            from_nodes[2].hostname.as_deref(),
            Some("node-b.example.com")
        );
        assert_eq!(
            from_nodes[5].health, "suspected",
            "pfail reads as suspected"
        );
        assert_eq!(from_nodes[0].link_state.as_deref(), Some("connected"));
        assert_eq!(from_shards[0].replication_offset, Some(5908));
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

    /// Run one raw command on every fixture node (ACL users are per node).
    async fn on_every_node(args: &[&str]) {
        for port in 7100..=7105 {
            let mut conn = direct(port).await;
            let mut cmd = redis::cmd(args[0]);
            for arg in &args[1..] {
                cmd.arg(*arg);
            }
            let _ = cmd.query_async::<Value>(&mut conn).await;
        }
    }

    /// Entra on a cluster, with an ACL user standing in for the Azure
    /// identity: every node connection signs in as the token's oid; a newer
    /// token rebuilds the routed connection; once the old token is revoked
    /// and every node connection is killed, the reconnects sign in with the
    /// new token, so every master still answers as that identity.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn live_cluster_entra_reauth_reaches_every_node() {
        let Some(mut profile) =
            live_cluster_profile("wsk-entra-cluster", RedisClusterMode::Cluster)
        else {
            eprintln!("skipping: SQLLY_REDIS_CLUSTER_URL not set");
            return;
        };
        let _guard = live_lock().lock().await;
        let user = format!("wsk_entra_cluster_{}", std::process::id());
        let first = entra_jwt(&user, 4_000_000_000);
        let second = entra_jwt(&user, 4_000_000_100);
        let grant_first = format!(">{first}");
        let grant_second = format!(">{second}");
        on_every_node(&[
            "ACL",
            "SETUSER",
            &user,
            "on",
            &grant_first,
            &grant_second,
            "~wsk:*",
            "+@all",
        ])
        .await;
        profile.authentication = sqlly_core::AuthenticationType::EntraBrowser;
        profile.runtime_entra_access_token = Some(first.clone());
        let client = super::super::open_client(&profile)
            .await
            .expect("entra cluster open");
        assert!(client.is_cluster());
        let masters = client.cluster_masters().await.unwrap();
        let mut whoami = redis::cmd("ACL");
        whoami.arg("WHOAMI");
        for master in &masters {
            let name = client.run_on(master, &whoami).await.unwrap();
            assert_eq!(
                text_value(&name).as_deref(),
                Some(user.as_str()),
                "{master:?}"
            );
        }
        // A newer token: the routed connection is rebuilt with it.
        assert!(client.refresh_entra(Some(&second)).await.expect("reauth"));
        assert!(!client.refresh_entra(Some(&second)).await.expect("same"));
        assert_eq!(client.entra_token().as_deref(), Some(second.as_str()));
        // Revoke the first token and drop every connection of the user: the
        // client library reconnects each node with the rebuilt client's
        // password, which must be the new token.
        on_every_node(&["ACL", "SETUSER", &user, "resetpass", &grant_second]).await;
        on_every_node(&["CLIENT", "KILL", "USER", &user]).await;
        let keys: Vec<String> = [0u16, 1, 2]
            .iter()
            .map(|want| {
                (0..)
                    .map(|i| format!("wsk:entra:{i}"))
                    .find(|k| key_slot(k.as_bytes()) / 5462 == *want)
                    .unwrap()
            })
            .collect();
        let mut script = String::new();
        for key in &keys {
            script.push_str(&format!("SET {key} v\nGET {key}\n"));
        }
        let mut run = run_script(&client, &profile, &script).await;
        if run.error.is_some() {
            // The first command after the kill may meet the closed socket;
            // the crate reconnects on the next one.
            run = run_script(&client, &profile, &script).await;
        }
        assert_eq!(run.error, None);
        for master in &masters {
            let name = client.run_on(master, &whoami).await.unwrap();
            assert_eq!(
                text_value(&name).as_deref(),
                Some(user.as_str()),
                "{master:?}"
            );
        }
        let mut cleanup = String::new();
        for key in &keys {
            cleanup.push_str(&format!("DEL {key}\n"));
        }
        let _ = run_script(&client, &profile, &cleanup).await;
        drop(client);
        on_every_node(&["ACL", "DELUSER", &user]).await;
    }
}
