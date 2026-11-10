#![cfg(not(target_arch = "wasm32"))]
//! The host-key trust dialog may open and close well inside the connect
//! timeout and still eat most of it. The time the dialog is open must not
//! count against the server at all: here the user answers before the
//! timeout, but the server's (slow) authentication only finishes after
//! prompt + auth has passed it. Its own binary, because the decider is
//! process-global.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{
    EchoServer, server_config, start_echo_server, start_ssh_server_with, write_client_key,
};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey};
use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig};
use sqlly_engines::ssh::known_hosts::{install_host_key_decider, set_known_hosts_path_for_tests};
use sqlly_engines::ssh::tunnel::open_tunnel;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// How long the "user" takes to click Trust: inside the connect timeout,
/// with room to spare.
const THINKING: Duration = Duration::from_secs(4);
/// How long the server takes to accept the key: inside the timeout on its
/// own, with seconds left for a loaded machine's key generation and
/// handshake. Prompt + auth is past the timeout, so charging the prompt to
/// the server (the old behaviour) fails this test, and so does crediting
/// only a prompt that is still open when the timeout fires: this one has
/// closed by then. The clock arithmetic itself is covered by the
/// paused-time unit tests in `tunnel.rs`; this test only proves the real
/// wiring, so the margins are wide.
const SERVER_AUTH: Duration = Duration::from_secs(12);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prompt_time_inside_the_timeout_is_not_charged_to_the_server() {
    install_host_key_decider(Arc::new(|_prompt| {
        std::thread::sleep(THINKING);
        true
    }));
    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let known_hosts = NamedTempFile::new().unwrap();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let echo_port = start_echo_server().await;
    let ssh_port = start_ssh_server_with(
        EchoServer {
            // A slow server, but one that answers within the connect
            // timeout on its own.
            auth_delay: SERVER_AUTH,
            ..EchoServer::new(client_key.public_key())
        },
        server_config(),
    )
    .await;
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
        .expect("the dialog's time must not count against the server");
    assert!(
        started.elapsed() >= THINKING + SERVER_AUTH,
        "both the prompt and the server were really waited on"
    );

    let mut stream = TcpStream::connect(("127.0.0.1", handle.local_port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");
}
