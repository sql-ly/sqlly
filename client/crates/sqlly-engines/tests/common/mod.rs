//! Scaffolding shared by the SSH tunnel tests: an in-process russh server
//! that forwards `direct-tcpip` channels, a TCP echo server to forward to,
//! and a client key written to a temp file. Each test file is its own
//! binary (the host-key decider is process-global), so this is `mod common;`
//! in each rather than a shared crate.
#![allow(dead_code)]

use std::sync::Arc;
use std::time::Duration;

use rand::rng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handler, Msg, Server, Session};
use russh::{Channel, Error as RusshError};
use ssh_key::LineEnding;
use tempfile::NamedTempFile;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// An SSH server that accepts one client key (and optionally one password)
/// and forwards every `direct-tcpip` channel to the address it names.
pub struct EchoServer {
    pub client_key: PublicKey,
    pub password: Option<String>,
    /// How long `auth_publickey` takes to answer: a slow server for the
    /// connect-timeout tests, instant otherwise.
    pub auth_delay: Duration,
    /// When set, an accepted key is only half the sign-in: the server then
    /// asks for this one-time code over keyboard-interactive, the way
    /// `AuthenticationMethods publickey,keyboard-interactive` does.
    pub one_time_code: Option<String>,
}

impl EchoServer {
    pub fn new(client_key: &PublicKey) -> Self {
        Self {
            client_key: client_key.clone(),
            password: None,
            auth_delay: Duration::ZERO,
            one_time_code: None,
        }
    }
}

impl Server for EchoServer {
    type Handler = Self;

    fn new_client(&mut self, _: Option<std::net::SocketAddr>) -> Self {
        Self {
            client_key: self.client_key.clone(),
            password: self.password.clone(),
            auth_delay: self.auth_delay,
            one_time_code: self.one_time_code.clone(),
        }
    }
}

impl Handler for EchoServer {
    type Error = RusshError;

    async fn auth_publickey(
        &mut self,
        _user: &str,
        key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if !self.auth_delay.is_zero() {
            tokio::time::sleep(self.auth_delay).await;
        }
        if key.public_key == &self.client_key && self.one_time_code.is_some() {
            Ok(Auth::Reject {
                proceed_with_methods: Some((&[russh::MethodKind::KeyboardInteractive][..]).into()),
                partial_success: true,
            })
        } else if key.public_key == &self.client_key {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.password.as_deref() == Some(password) {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_keyboard_interactive<'a>(
        &'a mut self,
        _user: &str,
        _submethods: &str,
        response: Option<russh::server::Response<'a>>,
    ) -> Result<Auth, Self::Error> {
        let Some(code) = self.one_time_code.clone() else {
            return Ok(Auth::reject());
        };
        match response {
            None => Ok(Auth::Partial {
                name: "Two-factor".into(),
                instructions: "Enter the code from your authenticator app".into(),
                prompts: vec![("Verification code: ".into(), false)].into(),
            }),
            Some(mut answers) => {
                let first = answers.next();
                if first.as_deref() == Some(code.as_bytes()) {
                    Ok(Auth::Accept)
                } else {
                    Ok(Auth::reject())
                }
            }
        }
    }

    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        let addr = format!("{host_to_connect}:{port_to_connect}");
        tokio::spawn(async move {
            let Ok(mut tcp) = TcpStream::connect(addr).await else {
                return;
            };
            let mut stream = channel.into_stream();
            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut stream).await;
        });
        Ok(())
    }
}

/// A TCP server that writes back whatever it reads. Returns its port.
pub async fn start_echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(n) = socket.read(&mut buf).await {
                    if n == 0 || socket.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// A server config with a fresh random host key; override fields as needed.
pub fn server_config() -> russh::server::Config {
    russh::server::Config {
        keys: vec![PrivateKey::random(&mut rng(), Algorithm::Ed25519).unwrap()],
        ..Default::default()
    }
}

/// Starts an [`EchoServer`] accepting `client_key` (and `password`, if any)
/// on a free port and returns the port.
pub async fn start_ssh_server(client_key: &PublicKey, password: Option<&str>) -> u16 {
    let server = EchoServer {
        password: password.map(str::to_string),
        ..EchoServer::new(client_key)
    };
    start_ssh_server_with(server, server_config()).await
}

/// Starts the given server with the given config on a free port.
pub async fn start_ssh_server_with(mut server: EchoServer, config: russh::server::Config) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = server.run_on_socket(Arc::new(config), &listener).await;
    });
    port
}

/// Writes the key in OpenSSH form to a temp file the tunnel config can name.
pub fn write_client_key(key: &PrivateKey) -> NamedTempFile {
    let file = NamedTempFile::new().unwrap();
    std::fs::write(file.path(), key.to_openssh(LineEnding::LF).unwrap()).unwrap();
    file
}
