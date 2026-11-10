#![cfg(not(target_arch = "wasm32"))]
//! Reading a host-key fingerprint can take longer than the SSH connect
//! timeout. The timeout must not count the time the trust dialog is open:
//! a Trust that arrives after the timeout would otherwise be thrown away as
//! "SSH connection timed out", and the second attempt would work, which
//! looks like flakiness. Its own binary, because the decider is
//! process-global.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{start_echo_server, start_ssh_server, write_client_key};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey};
use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig};
use sqlly_engines::ssh::known_hosts::{install_host_key_decider, set_known_hosts_path_for_tests};
use sqlly_engines::ssh::tunnel::open_tunnel;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long the "user" takes to click Trust; longer than the connect
/// timeout, which is the whole property: a prompt that outlasts the timeout
/// still connects. The clock arithmetic itself is covered by the
/// paused-time unit tests in `tunnel.rs`; this test only proves the real
/// wiring (russh, the blocking thread, the decider), so the timeout is
/// generous and the handshake has all of it.
const THINKING: Duration = Duration::from_secs(14);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_trust_answer_still_opens_the_tunnel() {
    install_host_key_decider(Arc::new(|_prompt| {
        std::thread::sleep(THINKING);
        true
    }));
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let known_hosts = NamedTempFile::new().unwrap();
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

    let started = std::time::Instant::now();
    let handle = open_tunnel(&cfg, "127.0.0.1", echo_port, CONNECT_TIMEOUT)
        .await
        .expect("a Trust answered after the connect timeout still counts");
    assert!(
        started.elapsed() >= THINKING,
        "the prompt was really waited on"
    );

    let mut stream = TcpStream::connect(("127.0.0.1", handle.local_port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
    let learned = std::fs::read_to_string(known_hosts.path()).unwrap();
    assert!(learned.contains("127.0.0.1"), "{learned}");
}
