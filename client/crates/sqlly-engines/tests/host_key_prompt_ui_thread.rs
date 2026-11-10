#![cfg(not(target_arch = "wasm32"))]
//! The desktop app answers SSH host-key prompts from its UI thread: the
//! decider hands the prompt to the UI and waits for the user's answer. That
//! wait happens inside the SSH handshake, which runs on the async runtime,
//! so it must not block a runtime worker in a way tokio forbids. This test
//! installs a decider shaped exactly like the app's (prompt sent to another
//! thread, answer received on a oneshot) and opens a tunnel to an unknown
//! host. Its own binary, because the decider is process-global.

mod common;

use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use common::{start_echo_server, start_ssh_server, write_client_key};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey};
use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig};
use sqlly_engines::ssh::known_hosts::{
    HostKeyPrompt, install_host_key_decider, set_known_hosts_path_for_tests,
};
use sqlly_engines::ssh::tunnel::open_tunnel;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

/// The app's decider: queue the prompt for the UI thread, then wait for the
/// answer on a oneshot (`app/ssh_host_key.rs`). Here the "UI thread" is a
/// plain thread that trusts every key and reports the prompts it saw.
/// Installed once per process; tests take turns through [`SERIAL`].
fn ui_thread_prompts() -> &'static Mutex<mpsc::Receiver<HostKeyPrompt>> {
    static PROMPTS: OnceLock<Mutex<mpsc::Receiver<HostKeyPrompt>>> = OnceLock::new();
    PROMPTS.get_or_init(|| Mutex::new(install_ui_thread_decider()))
}

/// The tests share the decider and the known-hosts path override.
static SERIAL: Mutex<()> = Mutex::new(());

fn install_ui_thread_decider() -> mpsc::Receiver<HostKeyPrompt> {
    let (to_ui, ui_inbox) = mpsc::channel::<(HostKeyPrompt, oneshot::Sender<bool>)>();
    let (seen_tx, seen_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok((prompt, respond)) = ui_inbox.recv() {
            let _ = seen_tx.send(prompt);
            let _ = respond.send(true);
        }
    });
    install_host_key_decider(Arc::new(move |prompt| {
        let (respond_tx, respond_rx) = oneshot::channel();
        if to_ui.send((prompt, respond_tx)).is_err() {
            return false;
        }
        respond_rx.blocking_recv().unwrap_or(false)
    }));
    seen_rx
}

async fn trusting_an_unknown_host_from_the_ui_thread_opens_the_tunnel() {
    // Install before the handshake needs it.
    ui_thread_prompts();
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

    let handle = open_tunnel(&cfg, "127.0.0.1", echo_port, Duration::from_secs(10))
        .await
        .expect("the tunnel opens once the UI trusts the key");
    let mut stream = TcpStream::connect(("127.0.0.1", handle.local_port))
        .await
        .unwrap();
    stream.write_all(b"ping").await.unwrap();
    let mut buf = [0u8; 4];
    stream.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"ping");

    let prompt = ui_thread_prompts()
        .lock()
        .unwrap()
        .try_recv()
        .expect("the UI was asked once");
    assert_eq!(prompt.port, ssh_port);
    let learned = std::fs::read_to_string(known_hosts.path()).unwrap();
    assert!(learned.contains("127.0.0.1"), "{learned}");
}

// The app runs services on both runtime shapes (`lib.rs` multi-thread,
// `service_router.rs` current-thread); the prompt must work on either.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL
async fn trusting_from_the_ui_thread_works_on_a_multi_thread_runtime() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    trusting_an_unknown_host_from_the_ui_thread_opens_the_tunnel().await;
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL
async fn trusting_from_the_ui_thread_works_on_a_current_thread_runtime() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    trusting_an_unknown_host_from_the_ui_thread_opens_the_tunnel().await;
}
