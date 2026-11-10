//! SSH tunnel: local listener forwarding to a remote host:port through SSH.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use russh::Error as RusshError;
use russh::client::{self, AuthResult, Handler};
use russh::keys::agent::client::AgentClient;
use russh::keys::{PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key};
use sqlly_core::connection::{
    ConnectionId, ConnectionProfile, ProxyHop, SshAuth, SshHop, SshKeepalive, SshTunnelConfig,
    TransportChain,
};
use sqlly_core::{StudioError, StudioResult};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Child;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::ssh::challenge::{self, AfterFirstMethod, ResponderAsker};
use crate::ssh::keepalive::{self, SessionClosed};
use crate::ssh::known_hosts::{self, HostKeyError, PromptClock};
use crate::ssh::kube::open_port_forward;
use crate::ssh::proxy::dial_through_proxy;
use crate::ssh::ssm::open_ssm_forward;

struct SessionHandler {
    host: String,
    port: u16,
    /// Runs for exactly as long as the user is being asked to trust an
    /// unknown key, so the connect timeout does not count that wait (see
    /// [`timeout_unless_prompting`]).
    prompting: Arc<PromptClock>,
    /// Tripped when this session ends (see [`crate::ssh::keepalive`]).
    closed: SessionClosed,
}

impl Handler for SessionHandler {
    type Error = RusshError;

    async fn disconnected(
        &mut self,
        reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        self.closed.trip();
        tracing::debug!(host = %self.host, port = self.port, ?reason, "SSH session ended");
        match reason {
            client::DisconnectReason::ReceivedDisconnect(_) => Ok(()),
            client::DisconnectReason::Error(e) => Err(e),
        }
    }

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        // Off the runtime: an unknown key waits here for the user to answer
        // the desktop app's trust dialog.
        let verified = known_hosts::verify_server_key_off_runtime(
            &self.host,
            self.port,
            server_public_key,
            self.prompting.clone(),
        )
        .await;
        match verified {
            Ok(()) => Ok(true),
            Err(HostKeyError::Changed { line }) => Err(RusshError::KeyChanged { line }),
            Err(HostKeyError::Learn(e)) => Err(RusshError::Keys(e)),
            Err(HostKeyError::Rejected) | Err(HostKeyError::PromptUnavailable(_)) => Ok(false),
        }
    }
}

/// Signs in to one hop: the configured method first, then, when the server
/// accepts it only as a first step (`AuthenticationMethods
/// publickey,keyboard-interactive`), the keyboard-interactive questions it
/// asks next, answered by the user through the installed challenge prompt.
async fn authenticate(
    hop: &SshHop,
    handle: &mut client::Handle<SessionHandler>,
    prompting: &Arc<PromptClock>,
) -> StudioResult<()> {
    let first = first_method(hop, handle).await?;
    match first.as_ref().map(challenge::after_first_method) {
        Some(AfterFirstMethod::Done) => Ok(()),
        Some(AfterFirstMethod::KeyboardInteractive) => {
            let mut asker = ResponderAsker::installed(prompting.clone());
            challenge::run_keyboard_interactive(
                handle,
                &mut asker,
                &hop.host,
                hop.port,
                &hop.username,
            )
            .await
        }
        Some(AfterFirstMethod::Failed) | None => Err(StudioError::Authentication(match hop.auth {
            SshAuth::Agent => "SSH agent authentication failed".to_string(),
            _ => "SSH authentication failed".to_string(),
        })),
    }
}

/// The configured method's answer: `None` when the agent had no identity
/// the server took even as a first step.
async fn first_method(
    hop: &SshHop,
    handle: &mut client::Handle<SessionHandler>,
) -> StudioResult<Option<AuthResult>> {
    let username = hop.username.as_str();
    match &hop.auth {
        SshAuth::Password { secret_ref } => {
            let password = secret_ref.resolve()?;
            let result = handle
                .authenticate_password(username, &password)
                .await
                .map_err(|e| StudioError::Connection(e.to_string()))?;
            Ok(Some(result))
        }
        SshAuth::KeyFile { path, passphrase } => {
            let pass = match passphrase {
                Some(r) => Some(r.resolve()?),
                None => None,
            };
            let key = load_secret_key(path, pass.as_deref())
                .map_err(|e| StudioError::Connection(e.to_string()))?;
            let hash = handle
                .best_supported_rsa_hash()
                .await
                .map_err(|e| StudioError::Connection(e.to_string()))?
                .flatten();
            let result = handle
                .authenticate_publickey(username, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(|e| StudioError::Connection(e.to_string()))?;
            Ok(Some(result))
        }
        SshAuth::Agent => authenticate_agent(hop, handle).await,
    }
}

async fn authenticate_agent(
    hop: &SshHop,
    handle: &mut client::Handle<SessionHandler>,
) -> StudioResult<Option<AuthResult>> {
    let username = hop.username.as_str();
    let hash = handle
        .best_supported_rsa_hash()
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))?
        .flatten();

    #[cfg(unix)]
    let mut agent = AgentClient::connect_env()
        .await
        .map_err(|e| StudioError::Connection(format!("ssh-agent: {e}")))?;

    #[cfg(windows)]
    let mut agent = AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent")
        .await
        .map_err(|_| {
            StudioError::Connection(
                "ssh-agent authentication is not available on Windows yet; use a key file or password"
                    .to_string(),
            )
        })?;

    let identities = agent
        .request_identities()
        .await
        .map_err(|e| StudioError::Connection(format!("ssh-agent: {e}")))?;
    for id in identities {
        let key = id.public_key().into_owned();
        let result = handle
            .authenticate_publickey_with(username, key, hash, &mut agent)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
        // An identity the server took, fully or as a first step, ends the
        // search: offering the next key after a partial success would only
        // start the sign-in over.
        if challenge::after_first_method(&result) != AfterFirstMethod::Failed {
            return Ok(Some(result));
        }
    }
    Ok(None)
}

async fn open_session(
    hop: &SshHop,
    via: Option<Arc<client::Handle<SessionHandler>>>,
    first_hop: Option<&ProxyHop>,
    timeout: Duration,
    keepalive: &SshKeepalive,
    closed: &SessionClosed,
) -> StudioResult<client::Handle<SessionHandler>> {
    let config = keepalive::client_config(keepalive);
    let prompting = Arc::new(PromptClock::default());
    let handler = SessionHandler {
        host: hop.host.clone(),
        port: hop.port,
        prompting: prompting.clone(),
        closed: closed.clone(),
    };

    let connect = async {
        let mut handle = if let Some(jump) = via {
            let channel = jump
                .channel_open_direct_tcpip(&hop.host, hop.port as u32, "127.0.0.1", 0)
                .await
                .map_err(|e| StudioError::Connection(e.to_string()))?;
            client::connect_stream(config, channel.into_stream(), handler)
                .await
                .map_err(|e| map_connect_error(&hop.host, hop.port, e))?
        } else if let Some(proxy) = first_hop {
            let stream = dial_through_proxy(proxy, &hop.host, hop.port, timeout).await?;
            client::connect_stream(config, stream, handler)
                .await
                .map_err(|e| map_connect_error(&hop.host, hop.port, e))?
        } else {
            let addr = (hop.host.as_str(), hop.port);
            client::connect(config, addr, handler)
                .await
                .map_err(|e| map_connect_error(&hop.host, hop.port, e))?
        };
        authenticate(hop, &mut handle, &prompting).await?;
        Ok(handle)
    };

    // russh runs the session on its own spawned task, so dropping this
    // future (the caller gave up, an outer deadline fired) does not stop a
    // host-key check already under way: the guard tells the clock the
    // connect is gone, and the check refuses to ask or trust on its behalf.
    let guard = AbandonOnDrop::new(prompting.clone());
    let result = timeout_unless_prompting(timeout, &prompting, connect)
        .await
        .ok_or_else(|| {
            StudioError::Connection(format!(
                "SSH connection to {}:{} timed out after {}s",
                hop.host,
                hop.port,
                timeout.as_secs()
            ))
        })?;
    guard.finished();
    result
}

/// Abandons the prompt clock unless the connect ran to completion, so a
/// connect dropped mid-way (for any reason, not only the timeout) never
/// ends in a trust dialog, or a known_hosts entry, for a connection that no
/// longer exists.
struct AbandonOnDrop {
    prompting: Arc<PromptClock>,
    finished: bool,
}

impl AbandonOnDrop {
    fn new(prompting: Arc<PromptClock>) -> Self {
        Self {
            prompting,
            finished: false,
        }
    }

    /// The connect completed (well or badly): a prompt the handler opened
    /// was answered for a real connection, so nothing is abandoned.
    fn finished(mut self) {
        self.finished = true;
    }
}

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        if !self.finished {
            self.prompting.abandon();
        }
    }
}

/// [`tokio::time::timeout`] for the SSH connect, except that the clock stops
/// while the user is looking at the host-key trust dialog: reading a
/// fingerprint can take a while, and a Trust that arrived after the timeout
/// used to be thrown away as "timed out", while one that arrived just
/// before it left the server almost no time to finish. The server gets the
/// full `timeout` of time the dialog was *not* open, however the two
/// interleave, so a server that hangs still fails in bounded time. `None`
/// means the timeout elapsed.
///
/// The deadline is `start + timeout + time the dialog has been open`. While
/// the dialog is open there is no deadline to wait for, only its closing;
/// once it closes the deadline is recomputed from the exact prompt time.
async fn timeout_unless_prompting<T>(
    timeout: Duration,
    prompting: &PromptClock,
    connect: impl Future<Output = T>,
) -> Option<T> {
    tokio::pin!(connect);
    let started = Instant::now();
    loop {
        // Register for the close before reading the state, so a close that
        // lands in between is not missed. `enable` is what registers: a
        // `Notified` otherwise does nothing until first polled, and a close
        // seen only through the permit `notify_one` stores would be missed
        // if the clock ever notified without storing one.
        let closed = prompting.closed();
        tokio::pin!(closed);
        closed.as_mut().enable();
        if prompting.is_open() {
            tokio::select! {
                result = &mut connect => return Some(result),
                _ = &mut closed => continue,
            }
        }
        let deadline = started + timeout + prompting.total();
        tokio::select! {
            result = &mut connect => return Some(result),
            _ = &mut closed => continue,
            _ = tokio::time::sleep_until(deadline) => {}
        }
        // The sleep fired; unless a prompt opened in the meantime, time is
        // up. The give-up and a prompt's announcement settle the connect
        // with one compare-and-swap each, so exactly one wins: a prompt
        // that got in first keeps the timeout waiting for its close, and
        // one that comes later finds the connect gone and never opens (see
        // `PromptClock::try_abandon`).
        if Instant::now() < started + timeout + prompting.total() {
            continue;
        }
        if !prompting.try_abandon() {
            continue;
        }
        return None;
    }
}

fn map_connect_error(host: &str, port: u16, err: RusshError) -> StudioError {
    match err {
        RusshError::KeyChanged { line } => StudioError::Connection(format!(
            "SSH host key for {host}:{port} changed (known_hosts line {line}); \
             refusing to connect — possible man-in-the-middle attack"
        )),
        RusshError::UnknownKey => {
            StudioError::Connection(format!("SSH host key for {host}:{port} is not trusted"))
        }
        other => StudioError::Connection(other.to_string()),
    }
}

async fn pump_connection(
    mut client_tcp: TcpStream,
    session: Arc<client::Handle<SessionHandler>>,
    target_host: String,
    target_port: u16,
    peer_port: u16,
    closed: SessionClosed,
) {
    let channel = match session
        .channel_open_direct_tcpip(
            &target_host,
            target_port as u32,
            "127.0.0.1",
            peer_port as u32,
        )
        .await
    {
        Ok(ch) => ch,
        Err(_) => {
            // A refused channel on a live session is the far side's answer
            // (nothing listens there); on a dead session the tunnel is gone.
            if session.is_closed() {
                closed.trip();
            }
            return;
        }
    };
    let mut remote = channel.into_stream();
    let _ = tokio::io::copy_bidirectional(&mut client_tcp, &mut remote).await;
}

pub struct TunnelHandle {
    pub local_port: u16,
    shutdown: watch::Sender<bool>,
    task: JoinHandle<()>,
    /// Supervised external process (kubectl). `kill_on_drop` covers the
    /// normal case; `Drop` still calls `start_kill` so an aborted task cannot
    /// leak the child.
    child: Option<Child>,
    /// For an SSH tunnel: tripped when one of its sessions ended.
    closed: Option<SessionClosed>,
    /// End the child's whole process tree on drop (see
    /// [`kill_process_tree`]).
    kill_tree: bool,
}

impl TunnelHandle {
    /// True once the tunnel can carry no more connections: its listener
    /// stopped, its kubectl exited, or its SSH session ended.
    pub fn is_finished(&self) -> bool {
        self.task.is_finished() || self.closed.as_ref().is_some_and(SessionClosed::is_tripped)
    }

    #[doc(hidden)]
    pub fn child_id(&self) -> Option<u32> {
        self.child.as_ref().and_then(|c| c.id())
    }

    pub(crate) fn from_supervised_process(
        local_port: u16,
        shutdown: watch::Sender<bool>,
        task: JoinHandle<()>,
        child: Option<Child>,
    ) -> Self {
        Self {
            local_port,
            shutdown,
            task,
            child,
            closed: None,
            kill_tree: false,
        }
    }

    /// Like [`Self::from_supervised_process`] for a child that starts its own
    /// helpers (the AWS CLI and its Session Manager plugin): teardown ends
    /// the whole process group / tree, not only `child`.
    pub(crate) fn from_supervised_process_tree(
        local_port: u16,
        shutdown: watch::Sender<bool>,
        task: JoinHandle<()>,
        child: Child,
    ) -> Self {
        Self {
            local_port,
            shutdown,
            task,
            child: Some(child),
            closed: None,
            kill_tree: true,
        }
    }
}

/// End `pid` and everything it started. On Unix `pid` must lead its own
/// process group (`Command::process_group(0)`).
pub(crate) fn kill_process_tree(pid: u32) {
    #[cfg(unix)]
    let mut command = {
        let mut command = std::process::Command::new("kill");
        command.args(["-TERM", "--", &format!("-{pid}")]);
        command
    };
    #[cfg(windows)]
    let mut command = {
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("taskkill");
        command
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .creation_flags(0x0800_0000);
        command
    };
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

impl Drop for TunnelHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        if let Some(child) = self.child.as_mut() {
            if self.kill_tree
                && let Some(pid) = child.id()
            {
                kill_process_tree(pid);
            }
            let _ = child.start_kill();
        }
        self.task.abort();
    }
}

pub async fn open_tunnel(
    cfg: &SshTunnelConfig,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    open_chain(
        &TransportChain {
            proxy: None,
            ssh: Some(cfg.clone()),
            kube: None,
            ssm: None,
        },
        target_host,
        target_port,
        timeout,
    )
    .await
}

pub async fn open_chain(
    chain: &TransportChain,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    if chain.is_empty() {
        return Err(StudioError::Connection(
            "transport chain is empty".to_string(),
        ));
    }
    let errors = chain.validate();
    if !errors.is_empty() {
        return Err(StudioError::Connection(errors.join("; ")));
    }

    if chain.kube.is_some() {
        return Err(StudioError::Connection(
            "kubernetes port-forward is opened via TunnelManager::endpoint_for".to_string(),
        ));
    }
    if chain.ssm.is_some() {
        return Err(StudioError::Connection(
            "AWS Session Manager port-forward is opened via TunnelManager::endpoint_for"
                .to_string(),
        ));
    }

    if let Some(ssh) = &chain.ssh {
        return open_ssh_chain(chain.proxy.as_ref(), ssh, target_host, target_port, timeout).await;
    }

    let proxy = chain
        .proxy
        .as_ref()
        .expect("validated non-empty proxy-only");
    open_proxy_only_chain(proxy, target_host, target_port, timeout).await
}

async fn open_ssh_chain(
    first_hop: Option<&ProxyHop>,
    cfg: &SshTunnelConfig,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| StudioError::Connection(e.to_string()))?
        .port();

    let closed = SessionClosed::new();
    let jump = if let Some(jump_hop) = cfg.jump.as_ref() {
        Some(Arc::new(
            open_session(jump_hop, None, first_hop, timeout, &cfg.keepalive, &closed).await?,
        ))
    } else {
        None
    };
    let target = Arc::new(
        open_session(
            &cfg.target,
            jump.clone(),
            if jump.is_some() { None } else { first_hop },
            timeout,
            &cfg.keepalive,
            &closed,
        )
        .await?,
    );

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let target_host = target_host.to_string();
    let session = Arc::clone(&target);
    let session_closed = closed.clone();

    let task = tokio::spawn(async move {
        let _jump_keepalive = jump;
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                // The SSH session ended (keepalives unanswered, the server
                // hung up): stop listening, so the manager dials a new
                // tunnel for the next connection instead of handing out a
                // port that leads nowhere.
                () = session_closed.tripped() => break,
                accept = listener.accept() => {
                    let Ok((client_tcp, peer)) = accept else { break };
                    if session.is_closed() {
                        session_closed.trip();
                        break;
                    }
                    let host = target_host.clone();
                    let session = Arc::clone(&session);
                    tokio::spawn(pump_connection(
                        client_tcp,
                        session,
                        host,
                        target_port,
                        peer.port(),
                        session_closed.clone(),
                    ));
                }
            }
        }
        if let Ok(handle) = Arc::try_unwrap(session) {
            let _ = handle
                .disconnect(russh::Disconnect::ByApplication, "", "")
                .await;
        }
    });

    Ok(TunnelHandle {
        local_port,
        shutdown: shutdown_tx,
        task,
        child: None,
        closed: Some(closed),
        kill_tree: false,
    })
}

async fn open_proxy_only_chain(
    proxy: &ProxyHop,
    target_host: &str,
    target_port: u16,
    timeout: Duration,
) -> StudioResult<TunnelHandle> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| StudioError::Connection(e.to_string()))?;
    let local_port = listener
        .local_addr()
        .map_err(|e| StudioError::Connection(e.to_string()))?
        .port();
    let proxy = proxy.clone();
    let target_host = target_host.to_string();
    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);

    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_ok() && *shutdown_rx.borrow() {
                        break;
                    }
                }
                accept = listener.accept() => {
                    let Ok((mut client_tcp, _)) = accept else { break };
                    let host = target_host.clone();
                    let proxy = proxy.clone();
                    tokio::spawn(async move {
                        let Ok(mut remote) = dial_through_proxy(
                            &proxy,
                            &host,
                            target_port,
                            timeout,
                        ).await else {
                            return;
                        };
                        let _ = tokio::io::copy_bidirectional(&mut client_tcp, &mut remote).await;
                    });
                }
            }
        }
    });

    Ok(TunnelHandle {
        local_port,
        shutdown: shutdown_tx,
        task,
        child: None,
        closed: None,
        kill_tree: false,
    })
}

/// Forwards are keyed by (connection, remote port): the connection's own
/// port is the forward every driver uses, and a tool that needs a second
/// port on the same server (clickhouse-client's native protocol next to the
/// HTTP port SQLly uses) gets its own forward through the same transport.
/// Every forward of a connection lives until [`TunnelManager::close`] for
/// that connection (the app calls it on disconnect and on deletion) or
/// [`TunnelManager::close_all`].
type ForwardKey = (ConnectionId, u16);

/// A forward to another host behind a connection's transport (a Redis
/// Cluster node the seed advertised), keyed by (connection, host, port).
type NodeForwardKey = (ConnectionId, String, u16);

pub struct TunnelManager {
    tunnels: Mutex<HashMap<ForwardKey, Arc<TunnelHandle>>>,
    node_tunnels: Mutex<HashMap<NodeForwardKey, Arc<TunnelHandle>>>,
    /// Connections whose tunnel ended and was dialed again since an engine
    /// last asked (see [`TunnelManager::take_reestablished`]).
    reestablished: Mutex<HashSet<ConnectionId>>,
}

impl TunnelManager {
    fn new() -> Self {
        TunnelManager {
            tunnels: Mutex::new(HashMap::new()),
            node_tunnels: Mutex::new(HashMap::new()),
            reestablished: Mutex::new(HashSet::new()),
        }
    }

    pub fn global() -> &'static Self {
        static MANAGER: OnceLock<TunnelManager> = OnceLock::new();
        MANAGER.get_or_init(TunnelManager::new)
    }

    /// True once after a tunnel of connection `id` ended (its SSH session
    /// dropped, its kubectl exited) and was dialed again for a new
    /// connection; the flag clears as it is read. An engine calls this
    /// after [`Self::endpoint_for`] to tell the user the tunnel was
    /// re-established. Nothing that ran over the old tunnel is re-run.
    pub fn take_reestablished(&self, id: &ConnectionId) -> bool {
        self.reestablished.lock().remove(id)
    }

    pub async fn endpoint_for(&self, profile: &ConnectionProfile) -> StudioResult<(String, u16)> {
        let chain = profile.inline_transport();
        if chain.is_empty() {
            return Ok((profile.server.trim().to_string(), profile.port));
        }

        if let Some(kube) = chain.kube.as_ref() {
            return self
                .reuse_or_open(
                    (profile.id, profile.port),
                    open_port_forward(kube, profile.timeout()),
                )
                .await;
        }

        if let Some(ssm) = chain.ssm.as_ref() {
            return self
                .reuse_or_open(
                    (profile.id, profile.port),
                    open_ssm_forward(
                        ssm,
                        ssm.remote_host_or(&profile.server),
                        profile.port,
                        profile.timeout(),
                    ),
                )
                .await;
        }

        self.reuse_or_open(
            (profile.id, profile.port),
            open_chain(
                &chain,
                profile.server.trim(),
                profile.port,
                profile.timeout(),
            ),
        )
        .await
    }

    /// The endpoint for another port on the profile's server, through the
    /// same transport chain as [`Self::endpoint_for`] (SSH hops and/or
    /// proxy). A direct profile dials the server itself. A Kubernetes
    /// port-forward targets one service port chosen in its own config, so it
    /// cannot carry a second one and is refused. The forward is kept until
    /// the connection is closed, like the connection's own.
    pub async fn endpoint_for_port(
        &self,
        profile: &ConnectionProfile,
        remote_port: u16,
    ) -> StudioResult<(String, u16)> {
        if remote_port == profile.port {
            return self.endpoint_for(profile).await;
        }
        let chain = profile.inline_transport();
        if chain.is_empty() {
            return Ok((profile.server.trim().to_string(), remote_port));
        }
        if chain.kube.is_some() {
            return Err(StudioError::CapabilityNotSupported(
                "a Kubernetes port-forward carries only the connection's own port".to_string(),
            ));
        }
        // A Session Manager session forwards one port; another port on the
        // same host is simply a second session through the same node.
        if let Some(ssm) = chain.ssm.as_ref() {
            return self
                .reuse_or_open(
                    (profile.id, remote_port),
                    open_ssm_forward(
                        ssm,
                        ssm.remote_host_or(&profile.server),
                        remote_port,
                        profile.timeout(),
                    ),
                )
                .await;
        }
        self.reuse_or_open(
            (profile.id, remote_port),
            open_chain(
                &chain,
                profile.server.trim(),
                remote_port,
                profile.timeout(),
            ),
        )
        .await
    }

    /// The endpoint for `host:port` as seen from the far side of the
    /// profile's transport chain: a Redis Cluster node that the seed
    /// advertised by its private address gets its own local forward through
    /// the same SSH hops and/or proxy, so a node address that only the
    /// bastion can reach is still dialable. The profile's own server and
    /// port reuse [`Self::endpoint_for`]. A direct profile dials `host:port`
    /// itself; a Kubernetes port-forward carries one service port only and
    /// is refused. Forwards live until the connection is closed.
    pub async fn endpoint_for_node(
        &self,
        profile: &ConnectionProfile,
        host: &str,
        port: u16,
    ) -> StudioResult<(String, u16)> {
        let host = host.trim();
        if host.eq_ignore_ascii_case(profile.server.trim()) && port == profile.port {
            return self.endpoint_for(profile).await;
        }
        let chain = profile.inline_transport();
        if chain.is_empty() {
            return Ok((host.to_string(), port));
        }
        if chain.kube.is_some() {
            return Err(StudioError::CapabilityNotSupported(format!(
                "a Kubernetes port-forward carries only the connection's own port, so cluster node {host}:{port} cannot be reached through it"
            )));
        }
        let key = (profile.id, host.to_string(), port);
        let ended = {
            let tunnels = self.node_tunnels.lock();
            match tunnels.get(&key) {
                Some(existing) if !existing.is_finished() => {
                    return Ok(("127.0.0.1".to_string(), existing.local_port));
                }
                Some(_) => true,
                None => false,
            }
        };
        // Like the connection's own forward: a node tunnel whose session
        // ended (keepalives unanswered) is dialed again on next use, and
        // the engine is told once (see `take_reestablished`).
        let handle = Arc::new(match chain.ssm.as_ref() {
            Some(ssm) => open_ssm_forward(ssm, host, port, profile.timeout()).await?,
            None => open_chain(&chain, host, port, profile.timeout()).await?,
        });
        let local = handle.local_port;
        self.node_tunnels.lock().insert(key, handle);
        if ended {
            tracing::info!(
                port,
                "cluster node tunnel re-established after its session ended"
            );
            self.reestablished.lock().insert(profile.id);
        }
        Ok(("127.0.0.1".to_string(), local))
    }

    async fn reuse_or_open(
        &self,
        key: ForwardKey,
        open: impl std::future::Future<Output = StudioResult<TunnelHandle>>,
    ) -> StudioResult<(String, u16)> {
        let ended = {
            let tunnels = self.tunnels.lock();
            match tunnels.get(&key) {
                Some(existing) if !existing.is_finished() => {
                    return Ok(("127.0.0.1".to_string(), existing.local_port));
                }
                Some(_) => true,
                None => false,
            }
        };

        // A tunnel that ended is dialed again here, on the next request for
        // it: the connection that hit the dead tunnel has already failed,
        // and only connections opened from now on use the new one.
        let handle = Arc::new(open.await?);
        let port = handle.local_port;
        self.tunnels.lock().insert(key, handle);
        if ended {
            tracing::info!(
                port = key.1,
                "tunnel re-established after its session ended"
            );
            self.reestablished.lock().insert(key.0);
        }
        Ok(("127.0.0.1".to_string(), port))
    }

    /// Close every forward of connection `id` (its own and any extra-port
    /// forwards). Dropping a handle stops its listener and SSH session.
    pub fn close(&self, id: &ConnectionId) {
        self.tunnels.lock().retain(|(owner, _), _| owner != id);
        self.node_tunnels
            .lock()
            .retain(|(owner, _, _), _| owner != id);
        self.reestablished.lock().remove(id);
    }

    /// How many forwards connection `id` currently holds.
    pub fn forward_count(&self, id: &ConnectionId) -> usize {
        self.tunnels
            .lock()
            .keys()
            .filter(|(owner, _)| owner == id)
            .count()
            + self
                .node_tunnels
                .lock()
                .keys()
                .filter(|(owner, _, _)| owner == id)
                .count()
    }

    pub fn close_all(&self) {
        self.tunnels.lock().clear();
        self.node_tunnels.lock().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn profile_without_tunnel() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "t",
            "db.example.com",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        )
    }

    #[tokio::test]
    async fn endpoint_for_returns_server_directly_when_no_transport() {
        let profile = profile_without_tunnel();
        let (host, port) = TunnelManager::global()
            .endpoint_for(&profile)
            .await
            .expect("endpoint");
        assert_eq!(host, "db.example.com");
        assert_eq!(port, 1433);
    }

    #[tokio::test]
    async fn open_chain_with_empty_chain_is_error() {
        match open_chain(
            &TransportChain::default(),
            "db.example.com",
            5432,
            Duration::from_secs(1),
        )
        .await
        {
            Err(e) => assert!(e.to_string().contains("empty")),
            Ok(_) => panic!("expected empty chain to fail"),
        }
    }

    // `#[tokio::test]` (not `#[test]`): building a `TunnelHandle` calls
    // `tokio::spawn`, which panics ("no reactor running") without an ambient
    // runtime. The body itself needs no `.await`.
    #[tokio::test]
    async fn close_all_drops_every_handle() {
        let manager = TunnelManager::new();
        let (shutdown_tx, _) = watch::channel(false);
        let handle = Arc::new(TunnelHandle {
            local_port: 12345,
            shutdown: shutdown_tx,
            task: tokio::spawn(async {}),
            child: None,
            closed: None,
            kill_tree: false,
        });
        let id1 = ConnectionId::new();
        let id2 = ConnectionId::new();
        manager.tunnels.lock().insert((id1, 1), Arc::clone(&handle));
        manager.tunnels.lock().insert((id2, 1), handle);
        manager.close_all();
        assert!(manager.tunnels.lock().is_empty());
    }

    /// A connection's extra-port forward lives and dies with the connection:
    /// `close(id)` drops every forward of that connection and no other.
    #[tokio::test]
    async fn close_drops_every_port_forward_of_that_connection_only() {
        let manager = TunnelManager::new();
        let handle = |port| {
            let (shutdown, _) = watch::channel(false);
            Arc::new(TunnelHandle {
                local_port: port,
                shutdown,
                task: tokio::spawn(std::future::pending::<()>()),
                child: None,
                closed: None,
                kill_tree: false,
            })
        };
        let a = ConnectionId::new();
        let b = ConnectionId::new();
        manager.tunnels.lock().insert((a, 8123), handle(40001));
        manager.tunnels.lock().insert((a, 9000), handle(40002));
        manager.tunnels.lock().insert((b, 8123), handle(40003));
        manager
            .node_tunnels
            .lock()
            .insert((a, "10.0.0.5".into(), 7001), handle(40004));
        assert_eq!(manager.forward_count(&a), 3);
        // Reuse returns the live extra forward instead of opening another.
        let reused = manager
            .reuse_or_open((a, 9000), async {
                Err(StudioError::Connection("must not reopen".into()))
            })
            .await
            .expect("reused");
        assert_eq!(reused, ("127.0.0.1".to_string(), 40002));
        manager.close(&a);
        assert!(manager.node_tunnels.lock().is_empty());
        assert_eq!(manager.forward_count(&a), 0);
        assert_eq!(manager.forward_count(&b), 1);
    }

    #[tokio::test]
    async fn endpoint_for_port_dials_directly_without_a_transport() {
        let profile = profile_without_tunnel();
        let endpoint = TunnelManager::global()
            .endpoint_for_port(&profile, 9000)
            .await
            .expect("endpoint");
        assert_eq!(endpoint, ("db.example.com".to_string(), 9000));
    }

    /// A cluster node on a direct profile is dialed as advertised; the
    /// profile's own server and port reuse the connection's endpoint.
    #[tokio::test]
    async fn endpoint_for_node_dials_directly_without_a_transport() {
        let mut profile = profile_without_tunnel();
        profile.port = 7000;
        let manager = TunnelManager::global();
        assert_eq!(
            manager
                .endpoint_for_node(&profile, "10.0.0.5", 7001)
                .await
                .expect("endpoint"),
            ("10.0.0.5".to_string(), 7001)
        );
        assert_eq!(
            manager
                .endpoint_for_node(&profile, "db.example.com", 7000)
                .await
                .expect("endpoint"),
            ("db.example.com".to_string(), 7000)
        );
        assert_eq!(manager.forward_count(&profile.id), 0);
    }

    /// A server that never answers still fails after `timeout` of
    /// non-prompt time, however long the dialog was open before that.
    /// Paused time: the clock only moves when every task is waiting, so the
    /// elapsed assertions are exact rather than a race with the scheduler.
    #[tokio::test(start_paused = true)]
    async fn a_hung_server_still_times_out_after_the_prompt_closes() {
        let prompting = Arc::new(PromptClock::default());
        let clock = prompting.clone();
        tokio::spawn(async move {
            let open = clock.open();
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(open);
        });
        let started = Instant::now();
        let result = timeout_unless_prompting(
            Duration::from_secs(10),
            &prompting,
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_none());
        let waited = started.elapsed();
        assert!(
            waited >= Duration::from_secs(40) && waited < Duration::from_secs(41),
            "prompt (30s) plus the full timeout (10s), not less: {waited:?}"
        );
    }

    /// A dialog that opens and closes inside the first timeout window still
    /// gives the server the whole timeout of its own: 3s of prompt, then the
    /// server answers 9s in, which is under the 10s it is owed.
    #[tokio::test(start_paused = true)]
    async fn prompt_time_inside_the_window_is_credited() {
        let prompting = Arc::new(PromptClock::default());
        let clock = prompting.clone();
        tokio::spawn(async move {
            let open = clock.open();
            tokio::time::sleep(Duration::from_secs(3)).await;
            drop(open);
        });
        let result = timeout_unless_prompting(Duration::from_secs(10), &prompting, async {
            tokio::time::sleep(Duration::from_secs(12)).await;
            "connected"
        })
        .await;
        assert_eq!(result, Some("connected"));
    }

    /// The prompting thread announces the dialog before the runtime stamps
    /// it. A deadline landing in that gap must see the dialog as open and
    /// wait for the close, not call the connect a hang.
    #[tokio::test(start_paused = true)]
    async fn a_prompt_announced_at_the_deadline_is_not_charged_as_a_hang() {
        let prompting = Arc::new(PromptClock::default());
        let clock = prompting.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(9_999)).await;
            clock.announce_open();
            tokio::time::sleep(Duration::from_millis(2)).await;
            let open = clock.open();
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(open);
        });
        let result = timeout_unless_prompting(Duration::from_secs(10), &prompting, async {
            tokio::time::sleep(Duration::from_secs(15)).await;
            "connected"
        })
        .await;
        assert_eq!(result, Some("connected"));
    }

    /// A prompt announced in the same instant the deadline lands: whichever
    /// side moves second sees the other, so the dialog either opens and is
    /// credited (the timeout keeps waiting) or never opens (the connect is
    /// abandoned), never both.
    #[tokio::test(start_paused = true)]
    async fn an_abandon_and_an_announcement_always_see_each_other() {
        // Timeout moved first: the connect is given up, the prompt loses
        // and must not open, and nothing is left looking open.
        let clock = PromptClock::default();
        assert!(clock.try_abandon());
        assert!(!clock.announce_open(), "the prompt loses to the give-up");
        assert!(clock.is_abandoned());
        clock.withdraw_announcement();
        assert!(!clock.is_open());
        assert!(clock.is_abandoned(), "a lost prompt cannot take it back");

        // Prompting side moved first: the timeout loses and keeps waiting,
        // and nothing is abandoned (no misleading "abandoned" for a prompt
        // that did open).
        let clock = PromptClock::default();
        assert!(clock.announce_open());
        assert!(!clock.try_abandon(), "the timeout loses to the prompt");
        assert!(clock.is_open());
        assert!(!clock.is_abandoned());
        // Once the prompt closes, the timeout can give the connect up.
        clock.withdraw_announcement();
        assert!(clock.try_abandon());

        // And on the timeout itself: once it gives a connect up with no
        // prompt in sight, the clock stays abandoned, so a prompt that
        // announces itself a moment later finds the connect gone and stays
        // closed.
        let prompting = Arc::new(PromptClock::default());
        let result = timeout_unless_prompting(
            Duration::from_secs(10),
            &prompting,
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_none());
        assert!(
            !prompting.announce_open(),
            "the late prompt finds the connect given up"
        );
        assert!(prompting.is_abandoned());
    }

    /// An announcement the runtime never gets to stamp (the prompting thread
    /// died) is withdrawn, and the timeout still ends in bounded time.
    #[tokio::test(start_paused = true)]
    async fn a_withdrawn_announcement_lets_the_timeout_fire() {
        let prompting = Arc::new(PromptClock::default());
        let clock = prompting.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            clock.announce_open();
            tokio::time::sleep(Duration::from_secs(20)).await;
            clock.withdraw_announcement();
        });
        let started = Instant::now();
        let result = timeout_unless_prompting(
            Duration::from_secs(10),
            &prompting,
            std::future::pending::<()>(),
        )
        .await;
        assert!(result.is_none());
        assert!(started.elapsed() >= Duration::from_secs(25));
    }

    /// Dropping the connect (not a timeout: the caller just let go) while
    /// a prompt is open abandons the clock, so the handler, which keeps
    /// running on russh's own task, refuses the Trust; a connect that ran
    /// to the end abandons nothing.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_connect_abandons_an_open_prompt() {
        let prompting = Arc::new(PromptClock::default());
        let _open = prompting.open();
        let connect = {
            let prompting = prompting.clone();
            async move {
                let guard = AbandonOnDrop::new(prompting.clone());
                let result = timeout_unless_prompting(
                    Duration::from_secs(10),
                    &prompting,
                    std::future::pending::<()>(),
                )
                .await;
                guard.finished();
                result
            }
        };
        let mut connect = Box::pin(connect);
        tokio::select! {
            _ = connect.as_mut() => unreachable!("the prompt is open, so there is no deadline"),
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        assert!(!prompting.is_abandoned());
        drop(connect);
        assert!(prompting.is_abandoned());

        let finished = Arc::new(PromptClock::default());
        let guard = AbandonOnDrop::new(finished.clone());
        guard.finished();
        assert!(!finished.is_abandoned());
    }
}
