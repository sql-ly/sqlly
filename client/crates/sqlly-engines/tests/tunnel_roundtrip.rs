#![cfg(not(target_arch = "wasm32"))]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::{
    EchoServer, server_config, start_echo_server, start_ssh_server, start_ssh_server_with,
    write_client_key,
};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey, PublicKey, load_secret_key};
use sqlly_core::ConnectionProfile;
use sqlly_core::connection::{
    SecretProvider, SecretRef, SshAuth, SshHop, SshKeepalive, SshTunnelConfig, TransportChain,
};
use sqlly_engines::ssh::TunnelManager;
use sqlly_engines::ssh::known_hosts::{
    HostKeyPrompt, install_host_key_decider, set_known_hosts_path_for_tests,
};
use sqlly_engines::ssh::tunnel::open_tunnel;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn temp_known_hosts() -> NamedTempFile {
    NamedTempFile::new().unwrap()
}

static PROMPT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// These tests share process-global state: the known-hosts path
/// (`set_known_hosts_path_for_tests`), the installed host-key decider, and
/// `PROMPT_COUNT`. Cargo runs test functions in parallel, so without a lock one
/// test can repoint the known-hosts path or bump the prompt counter while
/// another is mid-connection (observed as `PROMPT_COUNT` = 2 where 0 was
/// expected, and an empty learned known-hosts file). Serialize every test that
/// touches this state. The guard is held across `.await`, which is safe on the
/// current-thread `#[tokio::test]` runtime — the single task can't contend with
/// itself, and other tests block on the OS-level `Mutex` until it releases.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn install_recording_decider() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        install_host_key_decider(Arc::new(|_prompt: HostKeyPrompt| {
            PROMPT_COUNT.fetch_add(1, Ordering::SeqCst);
            true
        }));
    });
}

async fn echo_through_tunnel(
    cfg: &SshTunnelConfig,
    echo_port: u16,
) -> sqlly_core::StudioResult<()> {
    let handle = open_tunnel(cfg, "127.0.0.1", echo_port, Duration::from_secs(10)).await?;
    let mut stream = TcpStream::connect(("127.0.0.1", handle.local_port))
        .await
        .map_err(|e| sqlly_core::StudioError::Connection(e.to_string()))?;
    stream
        .write_all(b"ping")
        .await
        .map_err(|e| sqlly_core::StudioError::Connection(e.to_string()))?;
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| sqlly_core::StudioError::Connection(e.to_string()))?;
    assert_eq!(&buf, b"ping");
    Ok(())
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn key_file_tunnel_echo_prompts_once_for_unknown_host() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();

    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let echo_port = start_echo_server().await;
    let ssh_port = start_ssh_server(client_key.public_key(), None).await;

    let cfg = SshTunnelConfig {
        target: SshHop {
            host: "127.0.0.1".to_string(),
            port: ssh_port,
            username: "test".to_string(),
            auth: SshAuth::KeyFile {
                path: key_file.path().to_string_lossy().into_owned(),
                passphrase: None,
            },
        },
        jump: None,
        keepalive: Default::default(),
    };

    assert!(
        std::fs::read_to_string(known_hosts.path())
            .unwrap()
            .is_empty()
    );
    echo_through_tunnel(&cfg, echo_port).await.unwrap();
    let learned = std::fs::read_to_string(known_hosts.path()).unwrap();
    assert!(learned.contains("127.0.0.1"), "{learned}");
    echo_through_tunnel(&cfg, echo_port).await.unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn changed_host_key_fails_without_prompt() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();

    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let wrong_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let echo_port = start_echo_server().await;
    let ssh_port = start_ssh_server(client_key.public_key(), None).await;

    russh::keys::known_hosts::learn_known_hosts_path(
        "127.0.0.1",
        ssh_port,
        wrong_key.public_key(),
        known_hosts.path(),
    )
    .unwrap();

    let cfg = SshTunnelConfig {
        target: SshHop {
            host: "127.0.0.1".to_string(),
            port: ssh_port,
            username: "test".to_string(),
            auth: SshAuth::KeyFile {
                path: key_file.path().to_string_lossy().into_owned(),
                passphrase: None,
            },
        },
        jump: None,
        keepalive: Default::default(),
    };

    PROMPT_COUNT.store(0, Ordering::SeqCst);
    let err = open_tunnel(&cfg, "127.0.0.1", echo_port, Duration::from_secs(10))
        .await
        .err()
        .expect("expected failure")
        .to_string();
    assert!(err.contains("changed"), "{err}");
    assert_eq!(PROMPT_COUNT.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn jump_host_tunnel_echoes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let echo_port = start_echo_server().await;

    let jump_port = start_ssh_server(client_key.public_key(), None).await;
    let target_port = start_ssh_server(client_key.public_key(), None).await;

    let cfg = SshTunnelConfig {
        target: SshHop {
            host: "127.0.0.1".to_string(),
            port: target_port,
            username: "test".to_string(),
            auth: SshAuth::KeyFile {
                path: key_file.path().to_string_lossy().into_owned(),
                passphrase: None,
            },
        },
        jump: Some(SshHop {
            host: "127.0.0.1".to_string(),
            port: jump_port,
            username: "test".to_string(),
            auth: SshAuth::KeyFile {
                path: key_file.path().to_string_lossy().into_owned(),
                passphrase: None,
            },
        }),
        keepalive: Default::default(),
    };

    echo_through_tunnel(&cfg, echo_port).await.unwrap();
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn password_auth_tunnel_echoes() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let echo_port = start_echo_server().await;
    let ssh_port = start_ssh_server(client_key.public_key(), Some("sekret")).await;

    let cfg = SshTunnelConfig {
        target: SshHop {
            host: "127.0.0.1".to_string(),
            port: ssh_port,
            username: "test".to_string(),
            auth: SshAuth::Password {
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "sekret".to_string(),
                },
            },
        },
        jump: None,
        keepalive: Default::default(),
    };

    echo_through_tunnel(&cfg, echo_port).await.unwrap();
}

#[tokio::test]
async fn load_secret_key_roundtrip_from_openssh_file() {
    let key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let file = write_client_key(&key);
    let loaded = load_secret_key(file.path(), None).unwrap();
    assert_eq!(loaded.public_key(), key.public_key());
}

/// An SSH server that ends a session after `idle` with nothing received —
/// what a bastion's idle timeout or a NAT gateway does to a quiet tunnel.
async fn start_idle_limited_ssh_server(client_key: &PublicKey, idle: Duration) -> u16 {
    let config = russh::server::Config {
        inactivity_timeout: Some(idle),
        ..server_config()
    };
    start_ssh_server_with(EchoServer::new(client_key), config).await
}

/// A profile that reaches the echo server through the SSH server.
fn tunneled_profile(
    ssh_port: u16,
    key_file: &NamedTempFile,
    echo_port: u16,
    keepalive: SshKeepalive,
) -> ConnectionProfile {
    let mut profile = ConnectionProfile::sql_login(
        "tunneled",
        "127.0.0.1",
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.port = echo_port;
    profile.timeout_seconds = 10;
    profile.transport = TransportChain {
        proxy: None,
        ssh: Some(SshTunnelConfig {
            target: SshHop {
                host: "127.0.0.1".to_string(),
                port: ssh_port,
                username: "test".to_string(),
                auth: SshAuth::KeyFile {
                    path: key_file.path().to_string_lossy().into_owned(),
                    passphrase: None,
                },
            },
            jump: None,
            keepalive,
        }),
        kube: None,
        ssm: None,
    };
    profile
}

async fn echo_on(port: u16) -> std::io::Result<()> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ping");
    Ok(())
}

/// Keepalives every second hold a quiet tunnel open past a server that
/// drops sessions idle for 3 s: after 4.5 s of silence the manager hands out
/// the same tunnel, it still carries traffic, and nothing was re-dialed.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn keepalives_hold_an_idle_tunnel_open_past_the_servers_idle_limit() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let echo_port = start_echo_server().await;
    let ssh_port =
        start_idle_limited_ssh_server(client_key.public_key(), Duration::from_secs(3)).await;
    let profile = tunneled_profile(
        ssh_port,
        &key_file,
        echo_port,
        SshKeepalive {
            interval_seconds: Some(1),
            max_missed: Some(3),
        },
    );
    let manager = TunnelManager::global();
    let (_, first) = manager.endpoint_for(&profile).await.unwrap();
    echo_on(first).await.unwrap();
    tokio::time::sleep(Duration::from_millis(4500)).await;
    let (_, again) = manager.endpoint_for(&profile).await.unwrap();
    assert_eq!(again, first, "the idle tunnel stayed open");
    assert!(!manager.take_reestablished(&profile.id));
    echo_on(again).await.unwrap();
    manager.close(&profile.id);
}

/// Without keepalives the same server drops the quiet session; the next
/// request for the tunnel dials a new one (a new local port), reports the
/// re-dial exactly once, and the new tunnel carries traffic. Nothing that
/// used the old tunnel is replayed: the manager only re-dials on request.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn a_dropped_session_is_redialed_on_next_use_and_reported_once() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let echo_port = start_echo_server().await;
    let ssh_port =
        start_idle_limited_ssh_server(client_key.public_key(), Duration::from_secs(1)).await;
    let profile = tunneled_profile(
        ssh_port,
        &key_file,
        echo_port,
        SshKeepalive {
            interval_seconds: Some(0),
            max_missed: None,
        },
    );
    let manager = TunnelManager::global();
    let (_, first) = manager.endpoint_for(&profile).await.unwrap();
    echo_on(first).await.unwrap();
    assert!(!manager.take_reestablished(&profile.id));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let (_, second) = manager.endpoint_for(&profile).await.unwrap();
    assert_ne!(second, first, "a new tunnel was dialed");
    assert!(manager.take_reestablished(&profile.id), "reported");
    assert!(!manager.take_reestablished(&profile.id), "reported once");
    echo_on(second).await.unwrap();
    let (_, third) = manager.endpoint_for(&profile).await.unwrap();
    assert_eq!(third, second, "the live tunnel is reused");
    manager.close(&profile.id);
}

/// Live: a Redis connection through an SSH tunnel whose session the server
/// drops when idle. The first run carries no note; after the drop the next
/// connection dials a new tunnel and its run carries the "tunnel
/// re-established" message, once. Opt-in through `SQLLY_REDIS_URL`.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn redis_run_after_a_dropped_tunnel_says_the_tunnel_was_reestablished() {
    let Ok(url) = std::env::var("SQLLY_REDIS_URL") else {
        eprintln!("skipping: SQLLY_REDIS_URL not set");
        return;
    };
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let ssh_port =
        start_idle_limited_ssh_server(client_key.public_key(), Duration::from_secs(1)).await;
    let direct = sqlly_engines::redis::profile_from_url("wsj tunnel", &url).unwrap();
    let mut profile = tunneled_profile(
        ssh_port,
        &key_file,
        direct.port,
        SshKeepalive {
            interval_seconds: Some(0),
            max_missed: None,
        },
    );
    profile.database_type = direct.database_type;
    profile.server = direct.server.clone();
    profile.authentication = direct.authentication.clone();
    profile.encrypt = direct.encrypt;
    profile.trust_server_certificate = direct.trust_server_certificate;
    profile.tls_mode = direct.tls_mode;
    profile.database = Some("15".into());

    async fn run_messages(profile: &ConnectionProfile) -> Vec<String> {
        let client = sqlly_engines::redis::open_client(profile).await.unwrap();
        let events = sqlly_core::EventBus::new(64);
        let mut sub = events.subscribe();
        sqlly_engines::redis::execute_on_client(&client, profile, "PING", &events, None)
            .await
            .unwrap();
        let mut out = Vec::new();
        while let Ok(event) = sub.try_recv() {
            if let sqlly_core::EventPayload::Message { text, .. } = event.payload {
                out.push(text);
            }
        }
        out
    }

    let first = run_messages(&profile).await;
    assert!(
        !first.iter().any(|m| m.contains("re-established")),
        "{first:?}"
    );
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let after_drop = run_messages(&profile).await;
    assert!(
        after_drop
            .iter()
            .any(|m| m == sqlly_engines::redis::TUNNEL_REESTABLISHED_NOTE),
        "{after_drop:?}"
    );
    let next = run_messages(&profile).await;
    assert!(
        !next.iter().any(|m| m.contains("re-established")),
        "the note is shown once: {next:?}"
    );
    TunnelManager::global().close(&profile.id);
}

/// Live: a Redis connection through a real OpenSSH server stays usable
/// across 90 s of silence, on the same tunnel, with nothing re-dialed. Opt-in
/// through `SQLLY_SSH_LIVE=user:password@host:port` (an sshd that allows TCP
/// forwarding, e.g. with `ClientAliveCountMax 1`) and
/// `SQLLY_SSH_LIVE_REDIS=host:port` (the Redis server as the SSH host sees
/// it). `SQLLY_SSH_LIVE_IDLE_SECS` shortens the idle period.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL; see its doc comment
async fn live_openssh_tunnel_survives_idle_with_keepalives() {
    let (Ok(ssh), Ok(redis)) = (
        std::env::var("SQLLY_SSH_LIVE"),
        std::env::var("SQLLY_SSH_LIVE_REDIS"),
    ) else {
        eprintln!("skipping: SQLLY_SSH_LIVE / SQLLY_SSH_LIVE_REDIS not set");
        return;
    };
    let idle = std::env::var("SQLLY_SSH_LIVE_IDLE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90u64);
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    install_recording_decider();
    let known_hosts = temp_known_hosts();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());
    let (credentials, ssh_addr) = ssh.rsplit_once('@').expect("user:password@host:port");
    let (user, password) = credentials.split_once(':').expect("user:password");
    let (ssh_host, ssh_port) = ssh_addr.rsplit_once(':').expect("host:port");
    let (redis_host, redis_port) = redis.rsplit_once(':').expect("host:port");
    let mut profile = ConnectionProfile::sql_login(
        "wsj ssh live",
        redis_host,
        "",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.database_type = sqlly_core::DatabaseType::Redis;
    profile.port = redis_port.parse().unwrap();
    profile.encrypt = false;
    profile.trust_server_certificate = false;
    profile.tls_mode = sqlly_core::TlsMode::Disable;
    profile.transport = TransportChain {
        proxy: None,
        ssh: Some(SshTunnelConfig {
            target: SshHop {
                host: ssh_host.to_string(),
                port: ssh_port.parse().unwrap(),
                username: user.to_string(),
                auth: SshAuth::Password {
                    secret_ref: SecretRef {
                        provider: SecretProvider::InMemory,
                        key: password.to_string(),
                    },
                },
            },
            jump: None,
            keepalive: SshKeepalive::default(),
        }),
        kube: None,
        ssm: None,
    };
    let manager = TunnelManager::global();
    let (_, first) = manager.endpoint_for(&profile).await.unwrap();
    let client = sqlly_engines::redis::open_client(&profile).await.unwrap();
    sqlly_engines::redis::query_rows_on_client(&client, "PING")
        .await
        .unwrap();
    eprintln!("idling {idle} s through the tunnel on local port {first}");
    tokio::time::sleep(Duration::from_secs(idle)).await;
    sqlly_engines::redis::query_rows_on_client(&client, "PING")
        .await
        .expect("the connection held across the idle period");
    let (_, again) = manager.endpoint_for(&profile).await.unwrap();
    assert_eq!(again, first, "same tunnel");
    assert!(!manager.take_reestablished(&profile.id));
    manager.close(&profile.id);
}
