//! SSH tunnels through a server that wants a key and then a one-time code
//! (`AuthenticationMethods publickey,keyboard-interactive`), against the
//! in-process russh server in `common`.
#![cfg(not(target_arch = "wasm32"))]

mod common;

use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use common::{
    EchoServer, server_config, start_echo_server, start_ssh_server_with, write_client_key,
};
use rand::rng;
use russh::keys::{Algorithm, PrivateKey};
use sqlly_core::StudioError;
use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig};
use sqlly_engines::ssh::known_hosts::{install_host_key_decider, set_known_hosts_path_for_tests};
use sqlly_engines::ssh::tunnel::open_tunnel;
use sqlly_engines::ssh::{SshChallengePrompt, install_ssh_challenge_responder};
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// The process-wide responder and host-key decider are shared by every test
/// in this binary; run them one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// What the fake user types next (`None` = Cancel), and every prompt shown.
static NEXT_ANSWER: Mutex<Option<String>> = Mutex::new(None);
static PROMPTS: Mutex<Vec<SshChallengePrompt>> = Mutex::new(Vec::new());

fn install_fake_user() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    INSTALLED.get_or_init(|| {
        install_host_key_decider(Arc::new(|_| true));
        install_ssh_challenge_responder(Arc::new(|prompt| {
            let answer = NEXT_ANSWER.lock().unwrap().clone();
            let fields = prompt.fields.len();
            PROMPTS.lock().unwrap().push(prompt);
            answer.map(|code| vec![code; fields])
        }));
    });
}

async fn mfa_tunnel(
    answer: Option<&str>,
) -> (sqlly_core::StudioResult<()>, Vec<SshChallengePrompt>) {
    install_fake_user();
    *NEXT_ANSWER.lock().unwrap() = answer.map(str::to_string);
    PROMPTS.lock().unwrap().clear();
    let known_hosts = NamedTempFile::new().unwrap();
    set_known_hosts_path_for_tests(known_hosts.path().to_path_buf());

    let client_key = PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap();
    let key_file = write_client_key(&client_key);
    let echo_port = start_echo_server().await;
    let ssh_port = start_ssh_server_with(
        EchoServer {
            one_time_code: Some("246810".into()),
            ..EchoServer::new(client_key.public_key())
        },
        server_config(),
    )
    .await;
    let cfg = SshTunnelConfig {
        target: SshHop {
            host: "127.0.0.1".into(),
            port: ssh_port,
            username: "deploy".into(),
            auth: SshAuth::KeyFile {
                path: key_file.path().to_string_lossy().into_owned(),
                passphrase: None,
            },
        },
        jump: None,
        keepalive: Default::default(),
    };
    let result = async {
        let handle = open_tunnel(&cfg, "127.0.0.1", echo_port, Duration::from_secs(10)).await?;
        let mut stream = TcpStream::connect(("127.0.0.1", handle.local_port))
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
        stream
            .write_all(b"ping")
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
        let mut buf = [0u8; 4];
        stream
            .read_exact(&mut buf)
            .await
            .map_err(|e| StudioError::Connection(e.to_string()))?;
        assert_eq!(&buf, b"ping");
        Ok(())
    }
    .await;
    let prompts = PROMPTS.lock().unwrap().clone();
    (result, prompts)
}

fn message_key(err: &StudioError) -> Option<&'static str> {
    match err {
        StudioError::AuthenticationMessage(m) => m.key(),
        _ => None,
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL
async fn key_then_one_time_code_opens_the_tunnel() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (result, prompts) = mfa_tunnel(Some("246810")).await;
    result.unwrap();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    let prompt = &prompts[0];
    assert_eq!(prompt.username, "deploy");
    assert_eq!(prompt.name, "Two-factor");
    assert_eq!(
        prompt.instructions,
        "Enter the code from your authenticator app"
    );
    assert_eq!(prompt.fields.len(), 1);
    assert_eq!(prompt.fields[0].prompt, "Verification code: ");
    assert!(!prompt.fields[0].echo, "a one-time code is masked");
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL
async fn a_wrong_code_is_reported_as_rejected() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (result, prompts) = mfa_tunnel(Some("000000")).await;
    let err = result.unwrap_err();
    assert_eq!(
        message_key(&err),
        Some("services.errors.ssh_challenge_rejected"),
        "{err:?}"
    );
    assert_eq!(prompts.len(), 1);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // serialized on SERIAL
async fn cancelling_the_code_prompt_fails_the_connect() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let (result, _) = mfa_tunnel(None).await;
    let err = result.unwrap_err();
    assert_eq!(
        message_key(&err),
        Some("services.errors.ssh_challenge_cancelled"),
        "{err:?}"
    );
}
