//! Staged connection test: Test Connection as a step-by-step trace.
//!
//! A Redis connection fails at one of a handful of places — the name does
//! not resolve, the tunnel or proxy does not come up, nothing listens on the
//! port, the TLS handshake or certificate check fails, the password is
//! wrong, the server will not switch protocol, the logical database is
//! refused, or the endpoint is a cluster node. [`trace_connection`] walks
//! those stages in order and reports each one as it finishes, so the
//! connection editor's diagnostic log shows exactly how far the connection
//! got, and the failing stage becomes the headline of the error:
//!
//! resolve → tunnel / proxy → TCP → TLS (protocol, cipher, SNI, chain
//! verdict) → AUTH → HELLO / PING → SELECT → CLUSTER INFO probe → session.
//!
//! The last stage opens the real query session (protocol negotiation, the
//! connection's name, the startup script), so a passing trace means the
//! same thing a passing Test Connection always meant. The `CLUSTER INFO`
//! stage never fails a test: it says whether the endpoint is a cluster node
//! (routed by slot, or that node only when the connection is set to
//! Standalone) or a standalone server.
//!
//! A cluster seed list is traced through its first seed, and a Sentinel
//! profile through the master the sentinels name (the Resolve stage asks
//! them). The topology pane's per-node trace ([`trace_cluster_node`]) runs
//! the same stages against each node, with the TLS name the node announces
//! and no Session stage.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use redis::{ProtocolVersion, Value};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, RootCertStore, SignatureScheme};
use sqlly_core::{ConnectionProfile, StudioError, StudioResult, TlsMode};

use super::{DEFAULT_PORT, RedisClient};

/// One step of the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Resolve,
    Tunnel,
    Tcp,
    Tls,
    Auth,
    Hello,
    Select,
    Cluster,
    Session,
}

impl Stage {
    /// The stage's name in the log and in the error headline.
    pub fn label(self) -> &'static str {
        match self {
            Stage::Resolve => "Resolve",
            Stage::Tunnel => "Tunnel / proxy",
            Stage::Tcp => "TCP",
            Stage::Tls => "TLS",
            Stage::Auth => "AUTH",
            Stage::Hello => "HELLO / PING",
            Stage::Select => "SELECT",
            Stage::Cluster => "CLUSTER INFO",
            Stage::Session => "Session",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Stage::Resolve => ("services.redis.diagnostics.stage_resolve", "Resolve"),
            Stage::Tunnel => ("services.redis.diagnostics.stage_tunnel", "Tunnel / proxy"),
            Stage::Tcp => ("services.redis.diagnostics.stage_tcp", "TCP"),
            Stage::Tls => ("services.redis.diagnostics.stage_tls", "TLS"),
            Stage::Auth => ("services.redis.diagnostics.stage_auth", "AUTH"),
            Stage::Hello => ("services.redis.diagnostics.stage_hello", "HELLO / PING"),
            Stage::Select => ("services.redis.diagnostics.stage_select", "SELECT"),
            Stage::Cluster => ("services.redis.diagnostics.stage_cluster", "CLUSTER INFO"),
            Stage::Session => ("services.redis.diagnostics.stage_session", "Session"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }
}

/// How a stage ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Passed.
    Ok,
    /// Not applicable to this connection (no tunnel, plaintext, database 0).
    Skipped,
    /// Passed, with something worth knowing (an unverified certificate in
    /// Require mode, a server without RESP3, a cluster node).
    Note,
    /// Failed; the trace stops here.
    Failed,
}

/// One finished stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageReport {
    pub stage: Stage,
    pub outcome: Outcome,
    pub detail: String,
    pub elapsed: Duration,
}

impl StageReport {
    pub fn line_message(&self) -> sqlly_core::DisplayMessage {
        let key = match self.outcome {
            Outcome::Ok => "services.redis.diagnostics.line_ok",
            Outcome::Skipped => "services.redis.diagnostics.line_skipped",
            Outcome::Note => "services.redis.diagnostics.line_note",
            Outcome::Failed => "services.redis.diagnostics.line_failed",
        };
        sqlly_core::DisplayMessage::new(key, self.line())
            .with_arg("stage", self.stage.label())
            .with_arg("elapsed_ms", self.elapsed.as_millis().to_string())
            .with_arg("detail", self.detail.clone())
    }

    /// English compatibility fallback; localized callers use [`Self::line_message`].
    pub fn line(&self) -> String {
        let verdict = match self.outcome {
            Outcome::Ok => "ok",
            Outcome::Skipped => "skipped",
            Outcome::Note => "ok, note",
            Outcome::Failed => "FAILED",
        };
        format!(
            "{} {verdict} ({} ms): {}",
            self.stage.label(),
            self.elapsed.as_millis(),
            self.detail
        )
    }
}

/// Every stage that ran, in order, and the error that stopped the trace.
#[derive(Debug)]
pub struct ConnectionTrace {
    pub stages: Vec<StageReport>,
    error: Option<StudioError>,
}

impl ConnectionTrace {
    /// The stage that failed, when one did.
    pub fn failed(&self) -> Option<&StageReport> {
        self.stages.iter().find(|s| s.outcome == Outcome::Failed)
    }

    /// `Ok` when every stage passed, else the failing stage's error, its
    /// text headed by the stage (`TLS failed: …`).
    pub fn into_result(self) -> StudioResult<()> {
        match self.error {
            None => Ok(()),
            Some(error) => Err(error),
        }
    }
}

/// The text of an error without the `Connection error:` style prefix the
/// headline adds back — including a keyed `*Message` error whose fallback
/// already carries that prefix, so the headline never doubles it.
fn plain(error: &StudioError) -> String {
    error.detail()
}

/// [`plain`] for other modules' trace rows.
pub(crate) fn plain_text(error: &StudioError) -> String {
    plain(error)
}

/// The failing stage's error: same kind as the cause, the text headed by
/// the stage.
fn headline(stage: Stage, error: StudioError) -> StudioError {
    let text = format!("{} failed: {}", stage.label(), plain(&error));
    match error {
        StudioError::Authentication(_) | StudioError::AuthenticationMessage(_) => {
            StudioError::Authentication(text)
        }
        StudioError::PermissionDenied(_) | StudioError::PermissionDeniedMessage(_) => {
            StudioError::PermissionDenied(text)
        }
        StudioError::InvalidProfile(_) | StudioError::InvalidProfileMessage(_) => {
            StudioError::InvalidProfile(text)
        }
        StudioError::CapabilityNotSupported(_) | StudioError::CapabilityNotSupportedMessage(_) => {
            StudioError::CapabilityNotSupported(text)
        }
        _ => StudioError::Connection(text),
    }
}

/// Collects stage reports and hands each to the caller's sink as it lands.
struct Recorder<'a> {
    stages: Vec<StageReport>,
    sink: &'a mut (dyn FnMut(&StageReport) + Send),
}

impl Recorder<'_> {
    fn push(&mut self, stage: Stage, outcome: Outcome, detail: String, started: Instant) {
        let report = StageReport {
            stage,
            outcome,
            detail,
            elapsed: started.elapsed(),
        };
        (self.sink)(&report);
        self.stages.push(report);
    }

    fn fail(self, stage: Stage, error: StudioError, started: Instant) -> ConnectionTrace {
        let mut this = self;
        this.push(stage, Outcome::Failed, plain(&error), started);
        ConnectionTrace {
            stages: this.stages,
            error: Some(headline(stage, error)),
        }
    }

    fn done(self) -> ConnectionTrace {
        ConnectionTrace {
            stages: self.stages,
            error: None,
        }
    }
}

async fn bounded<T>(
    limit: Duration,
    what: &str,
    fut: impl std::future::Future<Output = StudioResult<T>>,
) -> StudioResult<T> {
    tokio::time::timeout(limit, fut).await.map_err(|_| {
        StudioError::Connection(format!("{what} timed out after {} s", limit.as_secs()))
    })?
}

fn describe_chain(profile: &ConnectionProfile, local: (&str, u16)) -> String {
    let chain = profile.inline_transport();
    let mut hops = Vec::new();
    if let Some(proxy) = &chain.proxy {
        let kind = match proxy.kind {
            sqlly_core::connection::ProxyKind::Socks5 => "SOCKS5 proxy",
            sqlly_core::connection::ProxyKind::HttpConnect => "HTTP proxy",
        };
        hops.push(format!("{kind} {}:{}", proxy.host, proxy.port));
    }
    if let Some(ssh) = &chain.ssh {
        if let Some(jump) = &ssh.jump {
            hops.push(format!(
                "SSH jump {}@{}:{}",
                jump.username, jump.host, jump.port
            ));
        }
        hops.push(format!(
            "SSH {}@{}:{}",
            ssh.target.username, ssh.target.host, ssh.target.port
        ));
    }
    if let Some(kube) = &chain.kube {
        hops.push(format!(
            "Kubernetes port-forward {}/{} port {}",
            kube.target_kind.kubectl_prefix(),
            kube.target_name,
            kube.remote_port
        ));
    }
    if let Some(ssm) = &chain.ssm {
        hops.push(format!("AWS Session Manager {}", ssm.summary()));
    }
    format!(
        "{} → {}:{} open at {}:{}",
        hops.join(" → "),
        profile.server.trim(),
        profile.port,
        local.0,
        local.1
    )
}

/// Walk every stage for the profile, calling `sink` as each one finishes.
pub async fn trace_connection(
    profile: &ConnectionProfile,
    sink: &mut (dyn FnMut(&StageReport) + Send),
) -> ConnectionTrace {
    let mut rec = Recorder {
        stages: Vec::new(),
        sink,
    };
    // A Sentinel profile's server field names a sentinel, not the data
    // server: the Resolve stage asks the sentinels for the master, and the
    // other stages trace that master.
    if let Some(config) = profile.redis_sentinel.as_ref() {
        let started = Instant::now();
        let (host, port) = match Box::pin(super::sentinel::data_endpoint(profile)).await {
            Ok(endpoint) => endpoint,
            Err(error) => return rec.fail(Stage::Resolve, error, started),
        };
        rec.push(
            Stage::Resolve,
            Outcome::Ok,
            format!(
                "the sentinels name {host}:{port} as the master of {}",
                config.master_name
            ),
            started,
        );
        rec.push(
            Stage::Tunnel,
            Outcome::Skipped,
            "direct connection (a Sentinel connection dials the master itself)".to_string(),
            Instant::now(),
        );
        let mut master = profile.clone();
        master.server = host.clone();
        master.port = port;
        let dialed = Dialed {
            profile: &master,
            server: host.clone(),
            server_port: port,
            host,
            port,
            tunneled: false,
            tls_name: None,
            cluster_node: false,
        };
        return trace_dialed(rec, dialed, Some(profile)).await;
    }
    // A cluster seed list is traced through its first seed; the session
    // stage then opens the routed client from every seed.
    let session_profile = profile;
    let first_seed = super::cluster::single_host(profile);
    let profile = first_seed.as_ref();
    let limit = profile.timeout();
    let tunneled = !profile.inline_transport().is_empty();
    let server = profile.server.trim().to_string();
    let server_port = if profile.port == 0 {
        DEFAULT_PORT
    } else {
        profile.port
    };

    // Resolve.
    let started = Instant::now();
    if tunneled {
        rec.push(
            Stage::Resolve,
            Outcome::Skipped,
            format!("{server} is resolved on the far side of the tunnel"),
            started,
        );
    } else {
        let host = server
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let lookup = bounded(limit, "name lookup", async {
            tokio::net::lookup_host((host.as_str(), server_port))
                .await
                .map(|addrs| addrs.collect::<Vec<_>>())
                .map_err(|e| StudioError::Connection(format!("cannot resolve {host}: {e}")))
        })
        .await;
        match lookup {
            Ok(addrs) if !addrs.is_empty() => {
                let mut shown: Vec<String> = addrs.iter().map(|a| a.ip().to_string()).collect();
                shown.dedup();
                let more = shown.len().saturating_sub(4);
                shown.truncate(4);
                let suffix = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                rec.push(
                    Stage::Resolve,
                    Outcome::Ok,
                    format!("{host} → {}{suffix}", shown.join(", ")),
                    started,
                );
            }
            Ok(_) => {
                return rec.fail(
                    Stage::Resolve,
                    StudioError::Connection(format!("{host} has no addresses")),
                    started,
                );
            }
            Err(error) => return rec.fail(Stage::Resolve, error, started),
        }
    }

    // Tunnel / proxy.
    let started = Instant::now();
    let (host, port) = match crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await
    {
        Ok(endpoint) => endpoint,
        Err(error) => return rec.fail(Stage::Tunnel, error, started),
    };
    let port = if port == 0 { DEFAULT_PORT } else { port };
    if tunneled {
        rec.push(
            Stage::Tunnel,
            Outcome::Ok,
            describe_chain(profile, (&host, port)),
            started,
        );
    } else {
        rec.push(
            Stage::Tunnel,
            Outcome::Skipped,
            "direct connection".to_string(),
            started,
        );
    }

    let dialed = Dialed {
        profile,
        host,
        port,
        tunneled,
        server,
        server_port,
        tls_name: None,
        cluster_node: false,
    };
    trace_dialed(rec, dialed, Some(session_profile)).await
}

/// One cluster node's trace for the topology pane: `node` is the profile
/// cut to that node (database 0), `advertised` the address the cluster
/// names it by (the TLS name, as the routed client uses it), `dial` where
/// the node map and the tunnel send the socket, and `route` how the dial
/// address was found. The `CLUSTER INFO` stage fails when the node does not
/// report `cluster_state:ok`; there is no Session stage (the routed session
/// is already open).
pub async fn trace_cluster_node(
    node: &ConnectionProfile,
    advertised: (&str, u16),
    dial: (&str, u16),
    route: String,
    sink: &mut (dyn FnMut(&StageReport) + Send),
) -> ConnectionTrace {
    let mut rec = Recorder {
        stages: Vec::new(),
        sink,
    };
    let limit = node.timeout();
    let (advertised_host, advertised_port) = advertised;
    let (dial_host, dial_port) = dial;
    let tunneled = !node.inline_transport().is_empty();
    let started = Instant::now();
    if tunneled {
        rec.push(
            Stage::Resolve,
            Outcome::Skipped,
            format!("{advertised_host} is resolved on the far side of the tunnel"),
            started,
        );
    } else {
        let host = dial_host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let lookup = bounded(limit, "name lookup", async {
            tokio::net::lookup_host((host.as_str(), dial_port))
                .await
                .map(|addrs| addrs.map(|a| a.ip().to_string()).collect::<Vec<_>>())
                .map_err(|e| StudioError::Connection(format!("cannot resolve {host}: {e}")))
        })
        .await;
        match lookup {
            Ok(mut ips) if !ips.is_empty() => {
                ips.dedup();
                ips.truncate(4);
                rec.push(
                    Stage::Resolve,
                    Outcome::Ok,
                    format!("{host} → {}", ips.join(", ")),
                    started,
                );
            }
            Ok(_) => {
                return rec.fail(
                    Stage::Resolve,
                    StudioError::Connection(format!("{host} has no addresses")),
                    started,
                );
            }
            Err(error) => return rec.fail(Stage::Resolve, error, started),
        }
    }
    let moved = !(advertised_host.eq_ignore_ascii_case(dial_host) && advertised_port == dial_port);
    rec.push(
        Stage::Tunnel,
        if moved { Outcome::Ok } else { Outcome::Skipped },
        route,
        Instant::now(),
    );
    let tls_name = moved.then(|| advertised_host.to_string());
    let dialed = Dialed {
        profile: node,
        host: dial_host.to_string(),
        port: dial_port,
        tunneled,
        server: advertised_host.to_string(),
        server_port: advertised_port,
        tls_name,
        cluster_node: true,
    };
    trace_dialed(rec, dialed, None).await
}

/// Where the protocol stages connect.
struct Dialed<'p> {
    /// Credentials, TLS settings, database and timeout.
    profile: &'p ConnectionProfile,
    /// The socket's destination (after tunnelling and the node map).
    host: String,
    port: u16,
    /// Whether the socket goes through a tunnel or proxy (for the hints).
    tunneled: bool,
    /// The server the trace is about, as the profile or cluster names it.
    server: String,
    server_port: u16,
    /// The name a verifying TLS handshake checks, when not the dialed host.
    tls_name: Option<String>,
    /// A cluster node: `CLUSTER INFO` must say the cluster is healthy.
    cluster_node: bool,
}

/// TCP onwards: the stages every trace shares. `session` is the profile
/// whose real session the last stage opens (`None` skips that stage).
async fn trace_dialed(
    mut rec: Recorder<'_>,
    dialed: Dialed<'_>,
    session: Option<&ConnectionProfile>,
) -> ConnectionTrace {
    let Dialed {
        profile,
        host,
        port,
        tunneled,
        server,
        server_port,
        tls_name,
        cluster_node,
    } = dialed;
    let limit = profile.timeout();

    // TCP.
    let started = Instant::now();
    let dial_host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let tcp = bounded(limit, "TCP connect", async {
        tokio::net::TcpStream::connect((dial_host.as_str(), port))
            .await
            .map_err(|e| StudioError::Connection(format!("cannot connect to {host}:{port}: {e}")))
    })
    .await;
    let tcp = match tcp {
        Ok(tcp) => tcp,
        Err(error) => return rec.fail(Stage::Tcp, error, started),
    };
    let peer = tcp
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| format!("{host}:{port}"));
    rec.push(
        Stage::Tcp,
        Outcome::Ok,
        if tunneled {
            format!("connected to the tunnel's local end {peer}")
        } else {
            format!("connected to {peer}")
        },
        started,
    );

    // TLS.
    let started = Instant::now();
    let mode = profile.effective_tls_mode();
    if mode == TlsMode::Disable {
        drop(tcp);
        rec.push(
            Stage::Tls,
            Outcome::Skipped,
            "plaintext (TLS mode Disable)".to_string(),
            started,
        );
    } else {
        let tls_profile = profile.clone();
        // Through a tunnel, Verify full checks the profile's server name
        // (as the session does), not the tunnel's local end.
        let tls_host = tls_name
            .clone()
            .or_else(|| super::tunneled_tls_name(profile, &dial_host).map(|(name, _)| name))
            .unwrap_or_else(|| dial_host.clone());
        let probe = match tcp.into_std() {
            Ok(std_tcp) => tokio::task::spawn_blocking(move || {
                tls_probe(&tls_profile, &tls_host, mode, std_tcp, limit)
            })
            .await
            .unwrap_or_else(|e| Err(StudioError::Connection(format!("TLS probe stopped: {e}")))),
            Err(e) => Err(StudioError::Connection(format!("TLS probe: {e}"))),
        };
        match probe {
            Ok((outcome, detail)) => rec.push(Stage::Tls, outcome, detail, started),
            Err(error) => {
                let error = if tunneled {
                    StudioError::Connection(format!(
                        "{} (the tunnel accepted the connection; check that {server}:{server_port} \
                         is reachable from the far end)",
                        plain(&error)
                    ))
                } else {
                    error
                };
                return rec.fail(Stage::Tls, error, started);
            }
        }
    }

    // AUTH: the client library signs in (when the profile has a password)
    // as part of opening the connection; `ACL WHOAMI` then names the user,
    // and a server that wanted a password says NOAUTH here.
    let started = Instant::now();
    let mut probe_profile = profile.clone();
    probe_profile.database = None;
    let probe_name = tls_name.clone().map(|name| (name, server_port));
    let client = match open_probe(&probe_profile, &host, port, probe_name).await {
        Ok(client) => client,
        Err(error) => {
            let error = if tunneled
                && matches!(
                    error,
                    StudioError::Connection(_) | StudioError::ConnectionMessage(_)
                ) {
                StudioError::Connection(format!(
                    "{} (the tunnel accepted the connection; check that {server}:{server_port} \
                     is reachable from the far end)",
                    plain(&error)
                ))
            } else {
                error
            };
            return rec.fail(Stage::Auth, error, started);
        }
    };
    let mut whoami = redis::cmd("ACL");
    whoami.arg("WHOAMI");
    let has_password = matches!(super::credentials(profile), Ok((_, Some(_))));
    match client.run(&whoami).await {
        Ok(value) => {
            // DragonflyDB answers "User is <name>".
            let user = text(&value)
                .map(|u| u.strip_prefix("User is ").map(str::to_string).unwrap_or(u))
                .unwrap_or_else(|| "default".to_string());
            let how = if has_password {
                "signed in"
            } else {
                "no password needed"
            };
            rec.push(
                Stage::Auth,
                Outcome::Ok,
                format!("{how} as {user}"),
                started,
            );
        }
        Err(StudioError::Authentication(text)) => {
            return rec.fail(
                Stage::Auth,
                StudioError::Authentication(if has_password {
                    text
                } else {
                    format!("{text} — the server wants a password; set one on the connection")
                }),
                started,
            );
        }
        Err(_) => {
            // Before Redis 6 there is no ACL; the server accepted the
            // connection, which is all AUTH can prove there.
            let how = if has_password {
                "password accepted"
            } else {
                "no password sent"
            };
            rec.push(Stage::Auth, Outcome::Ok, how.to_string(), started);
        }
    }

    // HELLO / PING.
    let started = Instant::now();
    let mut hello = redis::cmd("HELLO");
    hello.arg("3");
    let protocol = match client.run(&hello).await {
        Ok(reply) => {
            let field = |name: &str| {
                super::shape::find_field(&reply, name)
                    .and_then(|v| text(&v))
                    .unwrap_or_default()
            };
            format!(
                "RESP3 ({} {}, {} {})",
                field("server"),
                field("version"),
                field("mode"),
                field("role")
            )
        }
        Err(StudioError::Authentication(text)) => {
            return rec.fail(Stage::Hello, StudioError::Authentication(text), started);
        }
        Err(error) => format!("RESP2 (the server refused HELLO 3: {})", plain(&error)),
    };
    let pinged = Instant::now();
    match client.run(&redis::cmd("PING")).await {
        Ok(_) => {
            let rtt = pinged.elapsed();
            let outcome = if protocol.starts_with("RESP3") {
                Outcome::Ok
            } else {
                Outcome::Note
            };
            rec.push(
                Stage::Hello,
                outcome,
                format!("{protocol}; PING {:.1} ms", rtt.as_secs_f64() * 1000.0),
                started,
            );
        }
        Err(error) => return rec.fail(Stage::Hello, error, started),
    }

    // SELECT.
    let started = Instant::now();
    let database = match super::database_index(profile) {
        Ok(db) => db,
        Err(error) => return rec.fail(Stage::Select, error, started),
    };
    if database == 0 {
        rec.push(
            Stage::Select,
            Outcome::Skipped,
            "database 0 (the default)".to_string(),
            started,
        );
    } else {
        match client.select_database(database).await {
            Ok(()) => rec.push(
                Stage::Select,
                Outcome::Ok,
                format!("database {database}"),
                started,
            ),
            Err(error) => {
                let text = plain(&error);
                let hint = if text.contains("cluster") {
                    " — a cluster has only database 0; clear the Database field"
                } else {
                    ""
                };
                return rec.fail(
                    Stage::Select,
                    StudioError::Connection(format!("{text}{hint}")),
                    started,
                );
            }
        }
    }

    // CLUSTER INFO probe (never fails a connection test; fails a cluster
    // node's trace when the node does not report a healthy cluster).
    let started = Instant::now();
    let mut cluster = redis::cmd("CLUSTER");
    cluster.arg("INFO");
    match client.run(&cluster).await {
        Ok(reply) => {
            let info = text(&reply).unwrap_or_default();
            let field = |name: &str| {
                info.lines()
                    .find_map(|l| l.strip_prefix(&format!("{name}:")))
                    .map(str::trim)
                    .unwrap_or("?")
                    .to_string()
            };
            let summary = format!(
                "cluster node: state {}, {} known nodes, {} slots assigned",
                field("cluster_state"),
                field("cluster_known_nodes"),
                field("cluster_slots_assigned")
            );
            if cluster_node {
                if field("cluster_state") != "ok" {
                    return rec.fail(Stage::Cluster, StudioError::Connection(summary), started);
                }
                rec.push(Stage::Cluster, Outcome::Ok, summary, started);
            } else {
                let how = if sqlly_core::redis_cluster::cluster_mode(profile)
                    == sqlly_core::redis_cluster::RedisClusterMode::Standalone
                {
                    "the connection is set to Standalone, so SQLly works with this node only and \
                     keys owned by other nodes answer MOVED"
                } else {
                    "SQLly routes every command to the master that owns its slot"
                };
                rec.push(
                    Stage::Cluster,
                    Outcome::Note,
                    format!("{summary} — {how}"),
                    started,
                );
            }
        }
        Err(error) if cluster_node => {
            return rec.fail(Stage::Cluster, error, started);
        }
        Err(error) => {
            let text = plain(&error);
            // Redis: "cluster support disabled"; DragonflyDB: "Cluster is
            // disabled".
            let lower = text.to_ascii_lowercase();
            if lower.contains("cluster support disabled") || lower.contains("cluster is disabled") {
                rec.push(
                    Stage::Cluster,
                    Outcome::Ok,
                    "standalone server (cluster support disabled)".to_string(),
                    started,
                );
            } else {
                rec.push(
                    Stage::Cluster,
                    Outcome::Skipped,
                    format!("the server does not answer CLUSTER INFO: {text}"),
                    started,
                );
            }
        }
    }
    drop(client);

    // Session: the real query connection, startup script included.
    let Some(session_profile) = session else {
        return rec.done();
    };
    let started = Instant::now();
    match Box::pin(super::open_client(session_profile)).await {
        Ok(session) => {
            rec.push(
                Stage::Session,
                Outcome::Ok,
                format!(
                    "{} session ready as {}",
                    super::hello::protocol_label(session.protocol()),
                    super::setname::client_name(session_profile)
                ),
                started,
            );
            rec.done()
        }
        Err(error) => rec.fail(Stage::Session, error, started),
    }
}

/// A RESP2 connection for the protocol stages, without `SELECT` (the
/// profile's database is cleared by the caller) or the session's extras.
async fn open_probe(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
    tls_name: Option<(String, u16)>,
) -> StudioResult<RedisClient> {
    let tls_name = tls_name.or_else(|| super::tunneled_tls_name(profile, host));
    match super::connect_named(profile, host, port, ProtocolVersion::RESP2, tls_name).await {
        Ok(client) => Ok(client),
        Err(refusal) => Err(refusal.into_error()),
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) | Value::VerbatimString { text: s, .. } => Some(s.clone()),
        Value::Okay => Some("OK".to_string()),
        Value::Int(n) => Some(n.to_string()),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// TLS probe
// ---------------------------------------------------------------------------

/// Verifies the server certificate with the real verifier, records the
/// verdict, and lets the handshake finish either way, so the probe can
/// report the protocol and cipher even for a certificate the profile's mode
/// does not require to verify. The mode decides afterwards whether the
/// verdict fails the stage.
#[derive(Debug)]
struct RecordingVerifier {
    inner: Arc<dyn ServerCertVerifier>,
    verdict: Arc<parking_lot::Mutex<Option<Result<(), rustls::Error>>>>,
    presented: Arc<parking_lot::Mutex<usize>>,
}

impl ServerCertVerifier for RecordingVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let result = self
            .inner
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
            .map(|_| ());
        *self.verdict.lock() = Some(result);
        *self.presented.lock() = 1 + intermediates.len();
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// The roots the driver checks a certificate against for this profile (see
/// [`super::trust`]): the bundled public roots, the CA file alone, and the
/// OS trust store when the profile adds it.
fn trust_roots(profile: &ConnectionProfile) -> StudioResult<(RootCertStore, String)> {
    super::trust::root_store(profile)
}

fn tls_version_label(version: Option<rustls::ProtocolVersion>) -> String {
    match version {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLS 1.3".to_string(),
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLS 1.2".to_string(),
        Some(other) => format!("{other:?}"),
        None => "TLS".to_string(),
    }
}

/// Handshake over the already-connected socket and judge the certificate by
/// the profile's mode. Blocking; runs on a blocking thread.
fn tls_probe(
    profile: &ConnectionProfile,
    host: &str,
    mode: TlsMode,
    mut tcp: std::net::TcpStream,
    limit: Duration,
) -> StudioResult<(Outcome, String)> {
    let io = |e: std::io::Error| StudioError::Connection(format!("TLS handshake: {e}"));
    tcp.set_nonblocking(false).map_err(io)?;
    tcp.set_read_timeout(Some(limit)).map_err(io)?;
    tcp.set_write_timeout(Some(limit)).map_err(io)?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let (roots, roots_label) = trust_roots(profile)?;
    let inner = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()
    .map_err(|e| StudioError::InvalidProfile(format!("cannot build the TLS verifier: {e}")))?;
    let verdict = Arc::new(parking_lot::Mutex::new(None));
    let presented = Arc::new(parking_lot::Mutex::new(0usize));
    let verifier = Arc::new(RecordingVerifier {
        inner,
        verdict: verdict.clone(),
        presented: presented.clone(),
    });
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| StudioError::Connection(format!("TLS setup: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let mut config = match (&profile.tls_client_cert_file, &profile.tls_client_key_file) {
        (Some(cert), Some(key)) => {
            let cert_pem = super::read_pem(cert, "client certificate")?;
            let key_pem = super::read_pem(key, "client key")?;
            let chain = CertificateDer::pem_slice_iter(&cert_pem)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    StudioError::InvalidProfile(format!("cannot parse the client certificate: {e}"))
                })?;
            let key = PrivateKeyDer::from_pem_slice(&key_pem).map_err(|e| {
                StudioError::InvalidProfile(format!("cannot parse the client key: {e}"))
            })?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| StudioError::InvalidProfile(format!("client certificate: {e}")))?
        }
        _ => builder.with_no_client_auth(),
    };
    // The driver sends no SNI in Require mode (nothing is verified); the
    // verifying modes send the host name it dials.
    let sni = mode != TlsMode::Require;
    config.enable_sni = sni;
    let name = ServerName::try_from(host.to_string()).map_err(|e| {
        StudioError::InvalidProfile(format!("{host} is not a TLS server name: {e}"))
    })?;
    let sni_label = match (&name, sni) {
        (_, false) => "no SNI (Require mode)".to_string(),
        (ServerName::DnsName(dns), true) => format!("SNI {}", dns.as_ref()),
        (_, true) => format!("no SNI ({host} is an IP address)"),
    };
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| StudioError::Connection(format!("TLS setup: {e}")))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut tcp).map_err(|e| {
            let text = e.to_string();
            if text.contains("UnexpectedEof") || e.kind() == std::io::ErrorKind::UnexpectedEof {
                StudioError::Connection(
                    "the server closed the connection during the TLS handshake (is it a TLS port? \
                     does it want a client certificate?)"
                        .to_string(),
                )
            } else {
                StudioError::Connection(format!("TLS handshake: {text}"))
            }
        })?;
    }
    // Close politely; nothing else is sent on this socket.
    conn.send_close_notify();
    let _ = conn.write_tls(&mut tcp);
    let _ = tcp.flush();
    let mut sink = [0u8; 1];
    let _ = tcp.set_read_timeout(Some(Duration::from_millis(50)));
    let _ = tcp.read(&mut sink);

    let suite = conn
        .negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()))
        .unwrap_or_else(|| "unknown cipher".to_string());
    let version = tls_version_label(conn.protocol_version());
    let chain_len = *presented.lock();
    let base = format!("{version}, {suite}, {sni_label}, {chain_len} certificate(s) presented");
    let verdict = verdict.lock().take().unwrap_or(Ok(()));
    let name_only = |e: &rustls::Error| {
        matches!(
            e,
            rustls::Error::InvalidCertificate(rustls::CertificateError::NotValidForName)
                | rustls::Error::InvalidCertificate(
                    rustls::CertificateError::NotValidForNameContext { .. }
                )
        )
    };
    Ok(match (mode, verdict) {
        (TlsMode::Require, Ok(())) => (
            Outcome::Ok,
            format!("{base}; not verified (Require), though it would pass against {roots_label}"),
        ),
        (TlsMode::Require, Err(e)) => (
            Outcome::Note,
            format!("{base}; not verified (Require) — against {roots_label} it fails: {e}"),
        ),
        (TlsMode::VerifyCa, Ok(())) => (
            Outcome::Ok,
            format!("{base}; chain verified against {roots_label}"),
        ),
        (TlsMode::VerifyCa, Err(e)) if name_only(&e) => (
            Outcome::Ok,
            format!(
                "{base}; chain verified against {roots_label} (the host name does not match, \
                 which Verify CA allows)"
            ),
        ),
        (_, Ok(())) => (
            Outcome::Ok,
            format!("{base}; chain and host name verified against {roots_label}"),
        ),
        (_, Err(e)) => {
            return Err(StudioError::Connection(format!(
                "{base}; the certificate does not verify against {roots_label}: {e}"
            )));
        }
    })
}

/// Verify a profile connects: the staged trace with its stages sent to the
/// debug log, failing with the failing stage's headline.
pub async fn test_connection(profile: &ConnectionProfile) -> StudioResult<()> {
    let mut sink = |report: &StageReport| {
        tracing::debug!(target: "sqlly::redis::test", "{}", report.line());
    };
    trace_connection(profile, &mut sink).await.into_result()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn profile(server: &str, port: u16) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "wsj diagnose",
            server,
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        p.database_type = sqlly_core::DatabaseType::Redis;
        p.port = port;
        p.encrypt = false;
        p.timeout_seconds = 3;
        p
    }

    async fn run_trace(p: &ConnectionProfile) -> (ConnectionTrace, Vec<String>) {
        let mut lines = Vec::new();
        let mut sink = |r: &StageReport| lines.push(r.line());
        let trace = trace_connection(p, &mut sink).await;
        (trace, lines)
    }

    #[test]
    fn the_failing_stage_heads_the_error() {
        let error = headline(
            Stage::Tls,
            StudioError::Connection("certificate expired".into()),
        );
        assert_eq!(
            error.to_string(),
            "Connection error: TLS failed: certificate expired"
        );
        let auth = headline(
            Stage::Auth,
            StudioError::Authentication("WRONGPASS invalid username-password pair".into()),
        );
        assert!(
            matches!(auth, StudioError::Authentication(ref t) if t.starts_with("AUTH failed: WRONGPASS"))
        );
        let report = StageReport {
            stage: Stage::Hello,
            outcome: Outcome::Note,
            detail: "RESP2".into(),
            elapsed: Duration::from_millis(3),
        };
        assert_eq!(report.line(), "HELLO / PING ok, note (3 ms): RESP2");
    }

    #[test]
    fn keyed_errors_head_the_stage_without_a_doubled_prefix() {
        let refused = headline(Stage::Hello, super::super::Refusal::Hello.into_error());
        let text = refused.to_string();
        assert!(matches!(refused, StudioError::Connection(_)));
        assert_eq!(text.matches("Connection error: ").count(), 1, "{text}");
        assert!(
            text.ends_with("failed: the server refused both RESP3 and RESP2 handshakes"),
            "{text}"
        );

        let auth = headline(
            Stage::Auth,
            StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::raw(
                "Authentication failed: WRONGPASS",
            )),
        );
        assert!(
            matches!(auth, StudioError::Authentication(ref t) if t == "AUTH failed: WRONGPASS"),
            "{auth:?}"
        );

        let unsupported = headline(
            Stage::Hello,
            StudioError::CapabilityNotSupportedMessage(sqlly_core::DisplayMessage::raw(
                "HELLO is not available",
            )),
        );
        assert!(matches!(
            unsupported,
            StudioError::CapabilityNotSupported(_)
        ));
        assert_eq!(
            unsupported
                .to_string()
                .matches("Server capability not supported: ")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn an_unresolvable_host_stops_at_resolve() {
        let p = profile("no-such-host.invalid", 6379);
        let (trace, lines) = run_trace(&p).await;
        assert_eq!(trace.failed().map(|s| s.stage), Some(Stage::Resolve));
        assert_eq!(lines.len(), 1);
        let error = trace.into_result().unwrap_err().to_string();
        assert!(error.contains("Resolve failed"), "{error}");
    }

    #[tokio::test]
    async fn a_closed_port_stops_at_tcp() {
        // Bind and drop: nothing listens on the port afterwards.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let p = profile("127.0.0.1", port);
        let (trace, lines) = run_trace(&p).await;
        assert_eq!(trace.failed().map(|s| s.stage), Some(Stage::Tcp));
        assert!(lines[0].starts_with("Resolve ok"), "{lines:?}");
        assert!(lines[1].starts_with("Tunnel / proxy skipped"), "{lines:?}");
        assert!(lines[2].starts_with("TCP FAILED"), "{lines:?}");
        assert!(
            trace
                .into_result()
                .unwrap_err()
                .to_string()
                .contains("TCP failed")
        );
    }

    #[tokio::test]
    async fn a_plaintext_server_fails_a_tls_profile_at_tls() {
        // A listener that accepts and closes: what a plaintext port behind a
        // TLS profile looks like to the handshake.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = socket.write_all(b"-ERR not TLS\r\n").await;
            }
        });
        let mut p = profile("127.0.0.1", port);
        p.tls_mode = TlsMode::Require;
        p.encrypt = true;
        p.trust_server_certificate = true;
        let (trace, _) = run_trace(&p).await;
        assert_eq!(trace.failed().map(|s| s.stage), Some(Stage::Tls));
    }

    fn live_lines_contain(lines: &[String], stage: &str, needle: &str) -> bool {
        lines
            .iter()
            .any(|l| l.starts_with(stage) && l.contains(needle))
    }

    /// Live: the standalone fixture passes every stage, reports RESP3 and a
    /// standalone server; a wrong database fails at SELECT on the cluster
    /// fixture; the TLS-only fixture reports its protocol, cipher and chain
    /// verdict. Opt-in through `SQLLY_REDIS_URL`, `SQLLY_REDIS_CLUSTER_URL`,
    /// and `SQLLY_REDIS_TLS_URL` + `SQLLY_REDIS_TLS_CA_FILE`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn live_trace_names_every_stage() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        if let Some(mut p) = super::super::live_profile_from_env("wsj diagnose") {
            p.database = Some("15".into());
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert!(trace.failed().is_none(), "{lines:#?}");
            assert!(
                live_lines_contain(&lines, "HELLO / PING ok", "RESP3"),
                "{lines:#?}"
            );
            assert!(
                live_lines_contain(&lines, "SELECT ok", "database 15"),
                "{lines:#?}"
            );
            assert!(
                live_lines_contain(&lines, "CLUSTER INFO ok", "standalone"),
                "{lines:#?}"
            );
            assert!(
                live_lines_contain(&lines, "Session ok", "sqlly:wsj-diagnose:"),
                "{lines:#?}"
            );
            assert_eq!(lines.len(), 9, "{lines:#?}");
            // A wrong password fails at AUTH, with the stage as headline.
            p.authentication = sqlly_core::AuthenticationType::SqlLogin {
                username: "nobody".into(),
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "wrong".into(),
                },
            };
            let (trace, _) = run_trace(&p).await;
            assert_eq!(trace.failed().map(|s| s.stage), Some(Stage::Auth));
            assert!(matches!(
                trace.into_result(),
                Err(StudioError::Authentication(t)) if t.starts_with("AUTH failed")
            ));
        } else {
            eprintln!("skipping standalone: SQLLY_REDIS_URL not set");
        }
        if let Ok(url) = std::env::var("SQLLY_REDIS_CLUSTER_URL") {
            let mut p = super::super::profile_from_url("wsj cluster", &url).unwrap();
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert!(trace.failed().is_none(), "{lines:#?}");
            assert!(
                live_lines_contain(&lines, "CLUSTER INFO ok, note", "cluster node: state ok"),
                "{lines:#?}"
            );
            p.database = Some("3".into());
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert_eq!(
                trace.failed().map(|s| s.stage),
                Some(Stage::Select),
                "{lines:#?}"
            );
            assert!(
                trace
                    .into_result()
                    .unwrap_err()
                    .to_string()
                    .contains("only database 0")
            );
        } else {
            eprintln!("skipping cluster: SQLLY_REDIS_CLUSTER_URL not set");
        }
        if let Ok(url) = std::env::var("SQLLY_REDIS_TLS_URL") {
            let mut p = super::super::profile_from_url("wsj tls", &url).unwrap();
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert!(trace.failed().is_none(), "{lines:#?}");
            assert!(live_lines_contain(&lines, "TLS ok", "TLS 1."), "{lines:#?}");
            assert!(
                live_lines_contain(&lines, "TLS ok", "no SNI (Require mode)"),
                "{lines:#?}"
            );
            // Verify full without the CA: the chain verdict fails the stage.
            p.tls_mode = TlsMode::VerifyFull;
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert_eq!(
                trace.failed().map(|s| s.stage),
                Some(Stage::Tls),
                "{lines:#?}"
            );
            if let Ok(ca) = std::env::var("SQLLY_REDIS_TLS_CA_FILE") {
                p.tls_ca_file = Some(ca.into());
                let (trace, lines) = run_trace(&p).await;
                eprintln!("{}", lines.join("\n"));
                assert!(trace.failed().is_none(), "{lines:#?}");
                assert!(
                    live_lines_contain(&lines, "TLS ok", "chain and host name verified"),
                    "{lines:#?}"
                );
                // The OS store as well: still verified, against both.
                p.tls_trust_os_roots = true;
                let (trace, lines) = run_trace(&p).await;
                eprintln!("{}", lines.join("\n"));
                assert!(trace.failed().is_none(), "{lines:#?}");
                p.tls_ca_file = None;
            }
            // The OS store alone does not hold the fixture's private CA, so
            // the option does not loosen anything: the stage still fails.
            p.tls_trust_os_roots = true;
            let (trace, lines) = run_trace(&p).await;
            eprintln!("{}", lines.join("\n"));
            assert_eq!(
                trace.failed().map(|s| s.stage),
                Some(Stage::Tls),
                "{lines:#?}"
            );
        } else {
            eprintln!("skipping TLS: SQLLY_REDIS_TLS_URL not set");
        }
    }
}
