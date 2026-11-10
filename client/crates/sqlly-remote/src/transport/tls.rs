//! A concrete TLS connector that produces the byte stream a [`PeerSession`] runs
//! over in production.
//!
//! This is deliberately separable from the generic session: [`PeerSession`]
//! accepts any `AsyncRead + AsyncWrite`, so unit tests bypass TLS entirely with
//! `tokio::io::duplex`, and only real deployments call [`connect_tls`].
//!
//! TLS here protects the client↔relay hop (metadata, session auth); the payload
//! is *separately* end-to-end encrypted with Noise so the relay never sees it.
//!
//! [`PeerSession`]: crate::transport::PeerSession

use std::sync::Arc;

use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

use crate::transport::error::{RemoteError, Result};

/// Where and how to reach a relay over TLS.
#[derive(Debug, Clone)]
pub struct TlsTarget {
    /// DNS host name (also used for SNI and certificate verification).
    pub host: String,
    /// TCP port.
    pub port: u16,
}

impl TlsTarget {
    /// Construct a target.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
        }
    }
}

/// Build a rustls client config that trusts the Mozilla webpki root set.
fn default_client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Open a TCP+TLS connection to `target`, returning the encrypted byte stream to
/// hand to [`PeerSession::establish`](crate::transport::PeerSession::establish).
pub async fn connect_tls(target: &TlsTarget) -> Result<TlsStream<TcpStream>> {
    connect_tls_with(target, default_client_config()).await
}

/// Like [`connect_tls`] but with a caller-supplied rustls config (e.g. a pinned
/// or self-signed root set for staging).
pub async fn connect_tls_with(
    target: &TlsTarget,
    config: Arc<ClientConfig>,
) -> Result<TlsStream<TcpStream>> {
    let server_name = ServerName::try_from(target.host.clone())
        .map_err(|e| RemoteError::Io(std::io::Error::new(std::io::ErrorKind::InvalidInput, e)))?;

    let tcp = TcpStream::connect((target.host.as_str(), target.port)).await?;
    tcp.set_nodelay(true).ok();

    let connector = TlsConnector::from(config);
    let tls = connector.connect(server_name, tcp).await?;
    Ok(tls)
}
