//! # sqlly-pg
//!
//! PostgreSQL wire-protocol driver mirroring the `sqlly-tds` API surface:
//! [`open_client`] / [`execute_on_client`] for the streaming query path,
//! [`query_rows_on_client`] / [`execute_params_on_client`] for parameterized
//! reads/writes, and [`PostgresTransport`] implementing
//! `crate::driver::SqlTransport`. Serves every engine speaking this protocol —
//! PostgreSQL, CockroachDB, YugabyteDB — dispatch keys on
//! `DatabaseType::connection_kind() == ConnectionKind::PostgresWire`.
//!
//! The interactive path uses the *simple query* protocol (like the TDS driver
//! uses `simple_query`): multi-statement batches work and every value arrives
//! in text form, which maps directly onto the grid's canonical string
//! rendering. Parameterized calls use the extended protocol with typed
//! decoding for the common types.
//!
//! TLS follows the profile's [`TlsMode`], fail-closed: encrypted modes
//! negotiate rustls (webpki roots plus the profile's CA file; `Require`
//! accepts any certificate, `VerifyCa` verifies the chain but not the
//! hostname, `VerifyFull` verifies both; a client certificate/key pair
//! enables mutual TLS) and error out rather than downgrade when TLS cannot
//! be established; `Disable` connects plaintext explicitly. Entra/Integrated
//! auth are SQL Server concepts and are rejected up front.
//!
//! Server notices (`RAISE NOTICE` / `INFO` / `WARNING`, implicit-commit and
//! "does not exist, skipping" notes) arrive asynchronously on the connection
//! rather than in the query stream. The connection task captures them into a
//! [`NoticeQueue`] and [`execute_on_client`] publishes them as `Message`
//! events, so they show up in the Messages pane like Oracle's `DBMS_OUTPUT`.

#![forbid(unsafe_code)]

pub mod catalog;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::driver::QueryStreamer;
use sqlly_core::{
    AuthenticationType, CellValue, ConnectionProfile, DatabaseSummary, EventBus,
    MaterializedResultSet, ResultColumn, RowSender, SqlParam, StudioError, StudioResult, TlsMode,
};
use tokio_postgres::config::SslMode;
use tokio_postgres::types::{ToSql, Type};
use tokio_postgres::{AsyncMessage, NoTls, Row, SimpleQueryMessage};
use tokio_postgres_rustls::MakeRustlsConnect;

pub use crate::driver::{ConnectionService, SqlTransport};

/// A live PostgreSQL connection: the client plus the background task that
/// drives the socket. The task ends by itself when the client drops.
pub struct PgClient {
    client: tokio_postgres::Client,
    /// TLS client config captured at connect time (`None` = plaintext) so the
    /// server-side cancel connection uses the same encryption as the query
    /// connection. Arc'd to keep this struct (and enums embedding it) small.
    tls: Option<Arc<rustls::ClientConfig>>,
    /// Server notices captured by the connection task, drained by
    /// [`execute_on_client`] into `Message` events.
    notices: Arc<NoticeQueue>,
}

/// Upper bound on undrained notices held per connection. Notices raised
/// outside [`execute_on_client`] (catalog reads, startup SQL) are never
/// drained by a run of their own; the cap keeps a chatty server from growing
/// the queue without bound. The oldest are dropped first.
const NOTICE_QUEUE_CAP: usize = 10_000;

/// Notices the connection task has received and no run has published yet,
/// already rendered as Messages-pane text.
#[derive(Default)]
struct NoticeQueue(Mutex<VecDeque<String>>);

impl NoticeQueue {
    fn push(&self, text: String) {
        let mut queue = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if queue.len() >= NOTICE_QUEUE_CAP {
            queue.pop_front();
        }
        queue.push_back(text);
    }

    fn drain(&self) -> Vec<String> {
        let mut queue = self.0.lock().unwrap_or_else(|e| e.into_inner());
        queue.drain(..).collect()
    }
}

/// Render a server notice the way `psql` does (severity, message, then any
/// DETAIL / HINT on their own lines), minus psql's double space.
fn format_notice(notice: &tokio_postgres::error::DbError) -> String {
    let mut text = format!("{}: {}", notice.severity(), notice.message());
    if let Some(detail) = notice.detail() {
        text.push_str("\nDETAIL: ");
        text.push_str(detail);
    }
    if let Some(hint) = notice.hint() {
        text.push_str("\nHINT: ");
        text.push_str(hint);
    }
    text
}

/// Drive a connection on a background task, capturing its notices into
/// `notices`. Replaces `connection.await`, whose `Future` impl only logs
/// notices; `poll_message` is the documented way to observe them.
fn spawn_connection<S, T>(
    mut connection: tokio_postgres::Connection<S, T>,
    notices: Arc<NoticeQueue>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let ended = futures::future::poll_fn(|cx| {
            loop {
                match std::task::ready!(connection.poll_message(cx)) {
                    Some(Ok(AsyncMessage::Notice(notice))) => notices.push(format_notice(&notice)),
                    // LISTEN/NOTIFY is not surfaced on the query path.
                    Some(Ok(_)) => {}
                    Some(Err(error)) => return std::task::Poll::Ready(Err(error)),
                    None => return std::task::Poll::Ready(Ok(())),
                }
            }
        })
        .await;
        if let Err(error) = ended {
            tracing::debug!(%error, "postgres connection task ended");
        }
    });
}

/// The [`TlsMode`] a profile resolves to for this driver. `Legacy` is mapped
/// through [`ConnectionProfile::effective_tls_mode`]; IAM database auth
/// requires TLS even when the profile says plaintext.
/// https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/UsingWithRDS.IAMDBAuth.Connecting.AWSCLI.PostgreSQL.html
/// https://docs.cloud.google.com/sql/docs/postgres/iam-logins
pub(crate) fn tls_mode_for_profile(profile: &ConnectionProfile) -> TlsMode {
    let mode = profile.effective_tls_mode();
    if mode == TlsMode::Disable && profile.authentication.requires_tls() {
        return TlsMode::VerifyFull;
    }
    mode
}

/// Reads a PEM CA bundle into certificates, failing on unreadable files and
/// on bundles that contain no certificates at all (fail-closed: an empty
/// bundle must not silently fall back to the built-in roots).
pub(crate) fn load_ca_certs(
    path: &std::path::Path,
) -> StudioResult<Vec<rustls::pki_types::CertificateDer<'static>>> {
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_file_iter(path)
            .map_err(|e| {
                StudioError::InvalidProfile(format!("cannot read CA file {}: {e}", path.display()))
            })?
            .collect::<Result<_, _>>()
            .map_err(|e| {
                StudioError::InvalidProfile(format!("cannot parse CA file {}: {e}", path.display()))
            })?;
    if certs.is_empty() {
        return Err(StudioError::InvalidProfile(format!(
            "CA file contains no certificates: {}",
            path.display()
        )));
    }
    Ok(certs)
}

/// The client-certificate/key pair on a profile, validated: both set or
/// neither. One without the other is a profile error surfaced before any
/// socket is opened.
pub(crate) fn client_identity_paths(
    profile: &ConnectionProfile,
) -> StudioResult<Option<(&std::path::Path, &std::path::Path)>> {
    match (
        profile.tls_client_cert_file.as_deref(),
        profile.tls_client_key_file.as_deref(),
    ) {
        (Some(cert), Some(key)) => Ok(Some((cert, key))),
        (None, None) => Ok(None),
        (Some(_), None) => Err(StudioError::InvalidProfile(
            "a client certificate is set without its private key".into(),
        )),
        (None, Some(_)) => Err(StudioError::InvalidProfile(
            "a client key is set without its certificate".into(),
        )),
    }
}

/// Refuses passphrase-protected private keys with a clear error. Only the
/// key's *path* lives on the profile; a passphrase would be a secret, and
/// secrets other than passwords have no keyring slot yet.
pub(crate) fn ensure_key_not_encrypted(path: &std::path::Path) -> StudioResult<()> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        StudioError::InvalidProfile(format!("cannot read client key {}: {e}", path.display()))
    })?;
    if text.contains("ENCRYPTED PRIVATE KEY") || text.contains("Proc-Type: 4,ENCRYPTED") {
        return Err(StudioError::InvalidProfile(format!(
            "client key {} is passphrase-protected; only unencrypted PEM keys are supported",
            path.display()
        )));
    }
    Ok(())
}

/// Loads a validated client-certificate chain and its unencrypted PEM key.
pub(crate) fn load_client_identity(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> StudioResult<(
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
)> {
    use rustls::pki_types::pem::PemObject;
    let certs: Vec<rustls::pki_types::CertificateDer<'static>> =
        rustls::pki_types::CertificateDer::pem_file_iter(cert_path)
            .map_err(|e| {
                StudioError::InvalidProfile(format!(
                    "cannot read client certificate {}: {e}",
                    cert_path.display()
                ))
            })?
            .collect::<Result<_, _>>()
            .map_err(|e| {
                StudioError::InvalidProfile(format!(
                    "cannot parse client certificate {}: {e}",
                    cert_path.display()
                ))
            })?;
    if certs.is_empty() {
        return Err(StudioError::InvalidProfile(format!(
            "client certificate file contains no certificates: {}",
            cert_path.display()
        )));
    }
    ensure_key_not_encrypted(key_path)?;
    let key = rustls::pki_types::PrivateKeyDer::from_pem_file(key_path).map_err(|e| {
        StudioError::InvalidProfile(format!(
            "cannot parse client key {}: {e}",
            key_path.display()
        ))
    })?;
    Ok((certs, key))
}

/// Accept-any-certificate verifier backing `trust_server_certificate`.
/// Encryption still happens; only the identity check is skipped — the same
/// trade the TDS driver makes via tiberius' `trust_cert()`.
#[derive(Debug)]
struct AcceptAnyServerCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyServerCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Verifies the certificate chain against the trusted roots but tolerates a
/// hostname mismatch — `sslmode=verify-ca`. Everything else (expiry, chain of
/// trust, signatures, revocation data handled by webpki) still fails closed.
#[derive(Debug)]
struct VerifyChainSkipName(Arc<rustls::client::WebPkiServerVerifier>);

impl rustls::client::danger::ServerCertVerifier for VerifyChainSkipName {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        intermediates: &[rustls::pki_types::CertificateDer<'_>],
        server_name: &rustls::pki_types::ServerName<'_>,
        ocsp_response: &[u8],
        now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
        {
            Ok(verified) => Ok(verified),
            // Only the name check is waived; any other certificate problem
            // still fails the handshake.
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. },
            )) => Ok(rustls::client::danger::ServerCertVerified::assertion()),
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// Build the rustls client config for an encrypted mode. Roots are the
/// bundled webpki set plus the profile's CA file when one is set; a client
/// certificate/key pair enables mutual TLS.
pub(crate) fn rustls_config(
    profile: &ConnectionProfile,
    mode: TlsMode,
) -> StudioResult<rustls::ClientConfig> {
    // Use the ring provider explicitly rather than the bare `builder()`,
    // which relies on a process-level default CryptoProvider. Unit-test
    // binaries never run the app startup that installs one, so the default
    // path panics ("Could not automatically determine the process-level
    // CryptoProvider").
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(ca_path) = profile.tls_ca_file.as_deref() {
        for cert in load_ca_certs(ca_path)? {
            roots.add(cert).map_err(|e| {
                StudioError::InvalidProfile(format!(
                    "CA file {} has an invalid certificate: {e}",
                    ca_path.display()
                ))
            })?;
        }
    }
    let roots = Arc::new(roots);

    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("default protocol versions are always valid");
    let builder = match mode {
        // Encrypted, certificate NOT verified — the legacy
        // `trust_server_certificate` trade, same as tiberius' `trust_cert()`.
        TlsMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCert(provider))),
        TlsMode::VerifyCa => {
            let webpki = rustls::client::WebPkiServerVerifier::builder_with_provider(
                roots.clone(),
                provider,
            )
            .build()
            .map_err(|e| {
                StudioError::Connection(format!("cannot build certificate verifier: {e}"))
            })?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(VerifyChainSkipName(webpki)))
        }
        // `Legacy` never reaches here (`effective_tls_mode` resolves it) and
        // `Disable` connects plaintext; verify fully if either slips through.
        TlsMode::VerifyFull | TlsMode::Legacy | TlsMode::Disable => {
            builder.with_root_certificates(roots.clone())
        }
    };

    let config = match client_identity_paths(profile)? {
        Some((cert_path, key_path)) => {
            let (certs, key) = load_client_identity(cert_path, key_path)?;
            builder.with_client_auth_cert(certs, key).map_err(|e| {
                StudioError::InvalidProfile(format!("client certificate/key pair rejected: {e}"))
            })?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(config)
}

/// [`rustls_config`] for a PostgreSQL connection: direct TLS also offers the
/// `postgresql` ALPN, which a PostgreSQL 17 server requires on a handshake
/// that skips `SSLRequest`.
pub(crate) fn pg_rustls_config(
    profile: &ConnectionProfile,
    mode: TlsMode,
) -> StudioResult<rustls::ClientConfig> {
    let mut config = rustls_config(profile, mode)?;
    if profile.tls_direct {
        config.alpn_protocols = vec![b"postgresql".to_vec()];
    }
    Ok(config)
}

/// Build a `tokio_postgres::Config` from a connection profile. Public for
/// tests and diagnostics; secrets are resolved here.
pub fn config_from_profile(
    profile: &ConnectionProfile,
    host: &str,
    port: u16,
) -> StudioResult<tokio_postgres::Config> {
    let mut config = tokio_postgres::Config::new();
    // Through an SSH tunnel or proxy, dial the tunnel's local end
    // (`hostaddr`) but keep the server's name as `host`, which is what TLS
    // checks the certificate against.
    match (
        crate::ssh::server_name_behind_tunnel(profile, host),
        host.trim().parse::<std::net::IpAddr>(),
    ) {
        (Some(server), Ok(tunnel)) => {
            config.host(server);
            config.hostaddr(tunnel);
        }
        _ => {
            config.host(host.trim());
        }
    }
    config.port(port);
    config.application_name(&profile.application_name);
    config.connect_timeout(profile.timeout());
    // The driver's default first probe comes after 2 hours, long after a
    // firewall has silently dropped an idle warm client.
    config.keepalives(true);
    config.keepalives_idle(crate::tcp_keepalive::IDLE);
    config.keepalives_interval(crate::tcp_keepalive::INTERVAL);
    config.keepalives_retries(crate::tcp_keepalive::RETRIES);
    if profile.tls_direct && tls_mode_for_profile(profile) != TlsMode::Disable {
        config.ssl_negotiation(tokio_postgres::config::SslNegotiation::Direct);
    }
    if let Some(database) = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        config.dbname(database);
    }
    match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => {
            config.user(username);
            config.password(secret_ref.resolve()?);
        }
        AuthenticationType::Integrated => {
            return Err(StudioError::NotImplemented(
                "Integrated authentication is not supported for PostgreSQL connections",
            ));
        }
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            return Err(StudioError::InvalidProfile(
                "Microsoft Entra authentication applies to SQL Server, not PostgreSQL".into(),
            ));
        }
        AuthenticationType::AwsRdsIam { username, .. }
        | AuthenticationType::GcpCloudSqlIam { username } => {
            let token = profile.runtime_cloud_iam_token.clone().ok_or_else(|| {
                StudioError::AuthenticationMessage(sqlly_core::DisplayMessage::new(
                    "services.cloud_auth.iam_token_missing",
                    "Authentication failed: cloud IAM token missing; connect from SQLLY (or pass --access-token / SQLLY_CLOUD_IAM_TOKEN for the CLI)",
                ))
            })?;
            config.user(username);
            config.password(token);
        }
        AuthenticationType::AuthToken { .. } => {
            return Err(StudioError::InvalidProfile(
                "Auth-token sign-in applies to libSQL / Turso, not PostgreSQL".into(),
            ));
        }
    }
    Ok(config)
}

/// Open a client for the profile. The whole handshake is bounded by the
/// profile timeout. Encrypted profiles fail closed: if TLS cannot be
/// negotiated (or the certificate fails validation without
/// `trust_server_certificate`) the connection errors instead of silently
/// downgrading to plaintext.
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<PgClient> {
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    let mut config = config_from_profile(profile, &host, port)?;
    let mode = tls_mode_for_profile(profile);
    let timeout = profile.timeout();
    let timeout_error = || {
        StudioError::Connection(format!(
            "timed out connecting to {} after {:?}",
            profile.server, timeout,
        ))
    };
    let notices = Arc::new(NoticeQueue::default());
    let (client, tls) = match mode {
        TlsMode::Disable => {
            config.ssl_mode(SslMode::Disable);
            let (client, connection) = tokio::time::timeout(timeout, config.connect(NoTls))
                .await
                .map_err(|_| timeout_error())?
                .map_err(to_connection_error)?;
            spawn_connection(connection, notices.clone());
            (client, None)
        }
        TlsMode::Require | TlsMode::VerifyCa | TlsMode::VerifyFull | TlsMode::Legacy => {
            // Every encrypted mode requires TLS on the wire; the difference
            // lives entirely in the rustls verifier.
            config.ssl_mode(SslMode::Require);
            let tls_config = Arc::new(pg_rustls_config(profile, mode)?);
            let tls = MakeRustlsConnect::new(rustls::ClientConfig::clone(&tls_config));
            let (client, connection) = tokio::time::timeout(timeout, config.connect(tls))
                .await
                .map_err(|_| timeout_error())?
                .map_err(|e| with_direct_tls_hint(profile, to_connection_error(e)))?;
            spawn_connection(connection, notices.clone());
            (client, Some(tls_config))
        }
    };
    let client = PgClient {
        client,
        tls,
        notices,
    };
    apply_startup_sql(&client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(client: &PgClient, profile: &ConnectionProfile) -> StudioResult<()> {
    let Some(batches) = sqlly_core::startup_sql_batches(profile) else {
        return Ok(());
    };
    for batch in batches {
        execute_batch_on_client(client, batch)
            .await
            .map_err(sqlly_core::startup_sql_failed)?;
    }
    Ok(())
}

/// Server-side cancellation handle for a [`PgClient`]. Captured while the
/// client is idle; calling [`PgCancelHandle::cancel`] opens a short side
/// connection and asks the server to stop the statement currently running on
/// the original connection (the PostgreSQL cancel-request protocol).
pub struct PgCancelHandle {
    token: tokio_postgres::CancelToken,
    /// The originating connection's TLS client config (`None` = plaintext) —
    /// the cancel side connection must match it (an encrypted server may
    /// refuse plaintext, and a cancel must never leak over an unencrypted
    /// link the profile forbade).
    tls: Option<Arc<rustls::ClientConfig>>,
}

impl PgCancelHandle {
    /// Ask the server to cancel the connection's in-flight statement. A
    /// no-op server-side when nothing is running.
    pub async fn cancel(self) -> StudioResult<()> {
        let map_err = |e: tokio_postgres::Error| {
            StudioError::QueryExecution(format!("cancel request failed: {e}"))
        };
        match self.tls {
            None => self.token.cancel_query(NoTls).await.map_err(map_err),
            Some(config) => {
                let tls = MakeRustlsConnect::new(rustls::ClientConfig::clone(&config));
                self.token.cancel_query(tls).await.map_err(map_err)
            }
        }
    }
}

/// Obtain a cancellation handle for the client's connection.
pub fn cancel_handle(client: &PgClient) -> PgCancelHandle {
    PgCancelHandle {
        token: client.client.cancel_token(),
        tls: client.tls.clone(),
    }
}

/// Run a statement batch on an open client via the simple-query protocol,
/// streaming through the shared driver contract. Applies the profile's
/// safety policy with PostgreSQL transaction syntax.
///
/// Genuinely progressive: messages are consumed off the wire as the server
/// produces them (`simple_query_raw`), so rows reach the grid before the
/// statement completes, backpressure applies through the bounded row channel,
/// and a stalled stream still flushes its partial batch when the shared
/// cadence deadline fires.
pub async fn execute_on_client(
    client: &PgClient,
    profile: &ConnectionProfile,
    sql: &str,
    events: &EventBus,
    rows_tx: Option<&RowSender>,
) -> StudioResult<()> {
    use futures::StreamExt;

    let policed = sqlly_intel::query::safety::apply_connection_policy_for(profile, sql)?;
    // The simple-query protocol reports column names only and renders every
    // value as text, which would leave `int`/`numeric` columns labelled
    // `text` in the grid and exported as strings. Parsing the batch first
    // (never executing it) yields the declared column types for a
    // single-statement batch; anything the server will not prepare —
    // multi-statement batches, COPY, a statement the simple query is about
    // to reject anyway — falls back to the text-only shape.
    let mut declared = declared_columns(client, sql).await;
    // Notices left over from work outside a run (catalog reads, startup SQL)
    // do not belong to this batch.
    client.notices.drain();
    let stream = client
        .client
        .simple_query_raw(&policed)
        .await
        .map_err(to_query_error)?;
    tokio::pin!(stream);

    let mut streamer = QueryStreamer::begin(events, rows_tx)?;
    let mut text_kinds: Vec<TextKind> = Vec::new();
    // Whether the current command opened a result set: its CommandComplete
    // row count is then the SELECT row count the streamer already tracks,
    // not a separate DML "rows affected".
    let mut command_has_result_set = false;
    loop {
        // Race the next server message against the pending batch's flush
        // deadline so a slow producer still surfaces buffered rows on time.
        let message = match streamer.pending_flush_deadline() {
            Some(deadline) => tokio::select! {
                biased;
                message = stream.next() => message,
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                    streamer.flush_now().await;
                    continue;
                }
            },
            None => stream.next().await,
        };
        // The connection task queues a notice before it forwards any later
        // response, so every notice raised ahead of this message (or ahead of
        // the stream's end) is already queued. It may run ahead and also hold
        // later notices; those are published early, but notices always keep
        // their order relative to each other.
        publish_notices(client, &streamer)?;
        let Some(message) = message else {
            break;
        };
        let message = message.map_err(to_query_error)?;
        match message {
            SimpleQueryMessage::RowDescription(columns) => {
                command_has_result_set = true;
                let names: Vec<&str> = columns.iter().map(|c| c.name()).collect();
                // The prepared shape describes the first (only) result set;
                // use it when the names line up, then let it go.
                let types = declared
                    .take()
                    .filter(|types| {
                        types
                            .iter()
                            .map(|t| t.name.as_str())
                            .eq(names.iter().copied())
                    })
                    .unwrap_or_default();
                text_kinds = names
                    .iter()
                    .enumerate()
                    .map(|(i, _)| types.get(i).map_or(TextKind::Text, |t| t.kind))
                    .collect();
                streamer
                    .begin_result_set(
                        names
                            .iter()
                            .enumerate()
                            .map(|(i, name)| ResultColumn {
                                name: name.to_string(),
                                data_type: types
                                    .get(i)
                                    .map_or_else(|| "text".to_string(), |t| t.type_name.clone()),
                                nullable: true,
                            })
                            .collect(),
                    )
                    .await?;
            }
            SimpleQueryMessage::Row(row) => {
                let cells = (0..row.len())
                    .map(|i| match row.get(i) {
                        Some(text) => {
                            text_cell(text, text_kinds.get(i).copied().unwrap_or(TextKind::Text))
                        }
                        None => CellValue::Null,
                    })
                    .collect();
                if !streamer.push_row(cells).await? {
                    break;
                }
            }
            SimpleQueryMessage::CommandComplete(rows_affected) => {
                if !command_has_result_set {
                    streamer.statement_completed(rows_affected)?;
                }
                command_has_result_set = false;
            }
            _ => {}
        }
    }
    streamer.finish().await
}

/// Publish every queued server notice as a `Message` event, oldest first.
fn publish_notices(client: &PgClient, streamer: &QueryStreamer<'_>) -> StudioResult<()> {
    for text in client.notices.drain() {
        streamer.message(text)?;
    }
    Ok(())
}

/// How a simple-query text value converts, decided per column from the
/// declared type. Everything not listed stays text: `numeric` keeps its
/// exact digits (the column's declared type tells consumers it is a
/// number), and dates, UUIDs, arrays and JSON are text in the grid anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    Bool,
    Integer,
    Float,
    Text,
}

/// One declared result column of a prepared statement.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DeclaredColumn {
    name: String,
    type_name: String,
    kind: TextKind,
}

fn text_kind_for(ty: &Type) -> TextKind {
    match *ty {
        Type::BOOL => TextKind::Bool,
        Type::INT2 | Type::INT4 | Type::INT8 | Type::OID => TextKind::Integer,
        Type::FLOAT4 | Type::FLOAT8 => TextKind::Float,
        _ => TextKind::Text,
    }
}

/// Prepare `sql` to learn its result columns' declared types; `None` when
/// the server will not prepare it (multi-statement batch, COPY, errors).
async fn declared_columns(client: &PgClient, sql: &str) -> Option<Vec<DeclaredColumn>> {
    let statement = client.client.prepare(sql).await.ok()?;
    Some(
        statement
            .columns()
            .iter()
            .map(|column| DeclaredColumn {
                name: column.name().to_string(),
                type_name: column.type_().name().to_string(),
                kind: text_kind_for(column.type_()),
            })
            .collect(),
    )
}

/// A simple-query text value as the typed cell the extended protocol would
/// have produced: `t`/`f` → bool, integer columns → `I64`, float columns →
/// a finite `F64`. Anything that does not parse (`NaN`, `Infinity`, an
/// integer wider than `i64`) stays text rather than being mangled.
fn text_cell(text: &str, kind: TextKind) -> CellValue {
    match kind {
        TextKind::Bool => match text {
            "t" => CellValue::Bool(true),
            "f" => CellValue::Bool(false),
            other => CellValue::String(other.to_string()),
        },
        TextKind::Integer => match text.parse::<i64>() {
            Ok(n) => CellValue::I64(n),
            Err(_) => CellValue::String(text.to_string()),
        },
        TextKind::Float => match text.parse::<f64>() {
            Ok(v) if v.is_finite() => CellValue::F64(v),
            _ => CellValue::String(text.to_string()),
        },
        TextKind::Text => CellValue::String(text.to_string()),
    }
}

#[cfg(test)]
mod text_cell_tests {
    use super::*;

    #[test]
    fn declared_types_map_to_text_kinds() {
        assert_eq!(text_kind_for(&Type::BOOL), TextKind::Bool);
        assert_eq!(text_kind_for(&Type::INT4), TextKind::Integer);
        assert_eq!(text_kind_for(&Type::INT8), TextKind::Integer);
        assert_eq!(text_kind_for(&Type::FLOAT8), TextKind::Float);
        // Exact decimals stay text so no digit is lost through an f64.
        assert_eq!(text_kind_for(&Type::NUMERIC), TextKind::Text);
        assert_eq!(text_kind_for(&Type::VARCHAR), TextKind::Text);
        assert_eq!(text_kind_for(&Type::TIMESTAMPTZ), TextKind::Text);
    }

    #[test]
    fn text_values_convert_by_kind() {
        assert_eq!(text_cell("t", TextKind::Bool), CellValue::Bool(true));
        assert_eq!(text_cell("f", TextKind::Bool), CellValue::Bool(false));
        assert_eq!(text_cell("42", TextKind::Integer), CellValue::I64(42));
        assert_eq!(text_cell("-7", TextKind::Integer), CellValue::I64(-7));
        assert_eq!(text_cell("1.5", TextKind::Float), CellValue::F64(1.5));
        assert_eq!(
            text_cell("NaN", TextKind::Float),
            CellValue::String("NaN".into())
        );
        assert_eq!(
            text_cell("Infinity", TextKind::Float),
            CellValue::String("Infinity".into())
        );
        assert_eq!(
            text_cell("12.50", TextKind::Text),
            CellValue::String("12.50".into())
        );
    }
}

#[cfg(test)]
mod streaming_shape_tests {
    /// Compile-shape tripwire: `simple_query_raw` must return a `Stream` (not
    /// a collected `Vec`) — that is what makes `execute_on_client` progressive.
    #[allow(dead_code)]
    async fn simple_query_raw_returns_a_stream(client: &tokio_postgres::Client) {
        fn assert_stream<S: futures::Stream>(_: &S) {}
        if let Ok(stream) = client.simple_query_raw("SELECT 1").await {
            assert_stream(&stream);
        }
    }
}

/// Parameterized, materialized read (`$1…$n` placeholders) with typed
/// decoding. Mirrors `crate::tds::query_rows_on_client`; no safety policy.
pub async fn query_rows_on_client(
    client: &PgClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<Vec<MaterializedResultSet>> {
    let bound = bind_params(params);
    let refs: Vec<&(dyn ToSql + Sync)> = bound
        .iter()
        .map(|b| b.as_ref() as &(dyn ToSql + Sync))
        .collect();
    let rows = client
        .client
        .query(sql, &refs)
        .await
        .map_err(to_query_error)?;

    let Some(first) = rows.first() else {
        return Ok(vec![MaterializedResultSet::default()]);
    };
    let columns: Vec<ResultColumn> = first
        .columns()
        .iter()
        .map(|c| ResultColumn {
            name: c.name().to_string(),
            data_type: c.type_().name().to_string(),
            nullable: true,
        })
        .collect();
    let mapped = rows
        .iter()
        .map(|row| {
            (0..row.len())
                .map(|i| typed_cell(row, i))
                .collect::<Vec<_>>()
        })
        .collect();
    Ok(vec![MaterializedResultSet {
        columns,
        rows: mapped,
    }])
}

/// Origin metadata for one column of a prepared — never executed — statement.
/// `table_oid`/`column_id` identify the base-table column the result column
/// derives from; both are `None` for expressions. This is the PostgreSQL
/// equivalent of `sp_describe_first_result_set` provenance: the server itself
/// reports where each projected column comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgColumnOrigin {
    pub name: String,
    pub table_oid: Option<u32>,
    pub column_id: Option<i16>,
}

/// Prepare `sql` on the client and return each result column's origin. The
/// statement is parsed and planned server-side but never executed; the
/// prepared statement is closed when the handle drops.
pub async fn describe_statement_on_client(
    client: &PgClient,
    sql: &str,
) -> StudioResult<Vec<PgColumnOrigin>> {
    let statement = client.client.prepare(sql).await.map_err(to_query_error)?;
    Ok(statement
        .columns()
        .iter()
        .map(|column| PgColumnOrigin {
            name: column.name().to_string(),
            table_oid: column.table_oid(),
            column_id: column.column_id(),
        })
        .collect())
}

/// Run a raw statement batch with no parameters and no result decoding.
/// Used for transaction control (`BEGIN`/`COMMIT`/`ROLLBACK`) on the
/// row-editing path; no safety policy is applied.
pub async fn execute_batch_on_client(client: &PgClient, sql: &str) -> StudioResult<()> {
    client
        .client
        .batch_execute(sql)
        .await
        .map_err(to_query_error)
}

/// Parameterized non-query; returns affected rows. No safety policy.
pub async fn execute_params_on_client(
    client: &PgClient,
    sql: &str,
    params: &[SqlParam],
) -> StudioResult<u64> {
    let bound = bind_params(params);
    let refs: Vec<&(dyn ToSql + Sync)> = bound
        .iter()
        .map(|b| b.as_ref() as &(dyn ToSql + Sync))
        .collect();
    client
        .client
        .execute(sql, &refs)
        .await
        .map_err(to_query_error)
}

// `+ Send` keeps the parameterized futures Send: the boxed values live
// across the driver's awaits, and callers (the row-editing service) run
// inside Send async traits.
fn bind_params(params: &[SqlParam]) -> Vec<Box<dyn ToSql + Sync + Send>> {
    params
        .iter()
        .map(|p| -> Box<dyn ToSql + Sync + Send> {
            match p {
                SqlParam::Null => Box::new(None::<String>),
                SqlParam::Bool(b) => Box::new(*b),
                SqlParam::I64(n) => Box::new(*n),
                SqlParam::F64(f) => Box::new(*f),
                SqlParam::String(s) => Box::new(s.clone()),
                SqlParam::Bytes(b) => Box::new(b.clone()),
            }
        })
        .collect()
}

/// Decode one extended-protocol cell into the canonical `CellValue` shape:
/// native for bool/int/float/text/bytea, canonical strings for temporal,
/// uuid, and anything else that decodes as text.
fn typed_cell(row: &Row, i: usize) -> CellValue {
    let ty = row.columns()[i].type_();
    match *ty {
        Type::BOOL => opt(row.get::<_, Option<bool>>(i), CellValue::Bool),
        Type::INT2 => opt(row.get::<_, Option<i16>>(i), |v| CellValue::I64(v.into())),
        Type::INT4 => opt(row.get::<_, Option<i32>>(i), |v| CellValue::I64(v.into())),
        Type::INT8 => opt(row.get::<_, Option<i64>>(i), CellValue::I64),
        Type::FLOAT4 => opt(row.get::<_, Option<f32>>(i), |v| CellValue::F64(v.into())),
        Type::FLOAT8 => opt(row.get::<_, Option<f64>>(i), CellValue::F64),
        Type::BYTEA => opt(row.get::<_, Option<Vec<u8>>>(i), CellValue::Bytes),
        Type::TEXT | Type::VARCHAR | Type::BPCHAR | Type::NAME => {
            opt(row.get::<_, Option<String>>(i), CellValue::String)
        }
        Type::UUID => opt(row.get::<_, Option<uuid::Uuid>>(i), |v| {
            CellValue::String(v.to_string())
        }),
        Type::DATE => opt(row.get::<_, Option<chrono::NaiveDate>>(i), |v| {
            CellValue::String(v.to_string())
        }),
        Type::TIME => opt(row.get::<_, Option<chrono::NaiveTime>>(i), |v| {
            CellValue::String(v.to_string())
        }),
        Type::TIMESTAMP => opt(row.get::<_, Option<chrono::NaiveDateTime>>(i), |v| {
            CellValue::String(v.to_string())
        }),
        Type::TIMESTAMPTZ => opt(
            row.get::<_, Option<chrono::DateTime<chrono::Utc>>>(i),
            |v| CellValue::String(v.to_rfc3339()),
        ),
        _ => match row.try_get::<_, Option<String>>(i) {
            Ok(v) => opt(v, CellValue::String),
            Err(_) => CellValue::String(format!("<unsupported type {}>", ty.name())),
        },
    }
}

fn opt<T>(value: Option<T>, wrap: impl FnOnce(T) -> CellValue) -> CellValue {
    value.map(wrap).unwrap_or(CellValue::Null)
}

/// A server older than PostgreSQL 17 drops a direct TLS handshake, which
/// the driver reports only as a failed handshake; say what to change.
fn with_direct_tls_hint(profile: &ConnectionProfile, error: StudioError) -> StudioError {
    match error {
        StudioError::Connection(text) if profile.tls_direct && text.contains("TLS handshake") => {
            StudioError::ConnectionMessage(
                sqlly_core::DisplayMessage::new(
                    "services.pg.direct_tls_hint",
                    format!("Connection error: {text}. Direct TLS needs PostgreSQL 17 or later; untick \"Direct TLS (PostgreSQL 17+)\" in the connection editor for an older server."),
                )
                .with_arg("detail", text),
            )
        }
        other => other,
    }
}

fn to_connection_error(error: tokio_postgres::Error) -> StudioError {
    let text = error.to_string();
    if text.contains("password") || text.contains("authentication") {
        StudioError::Authentication(crate::connect_hint::with_local_network_hint(text))
    } else {
        crate::connect_hint::connection_with_local_network_hint(text)
    }
}

fn to_query_error(error: tokio_postgres::Error) -> StudioError {
    let mut text = error.to_string();
    if let Some(db) = error.as_db_error() {
        // tokio-postgres displays a server error as the bare "db error"; the
        // server's own words (`ERROR: cannot execute INSERT in a read-only
        // transaction`) live on the DbError. Surface them so the user — and
        // the read-only refusal mapping — see the real reason.
        let message = db.message();
        if !text.contains(message) {
            text = format!("{text}: {}: {message}", db.severity());
        }
        if let Some(tokio_postgres::error::ErrorPosition::Original(position)) = db.position() {
            // psql prints the caret itself; we print the 1-based character
            // index so the editor can underline it (parsed by
            // execution_error.rs).
            text.push_str(&format!("\nPOSITION: {position}"));
        }
    }
    StudioError::QueryExecution(text)
}

/// `SqlTransport` implementation for the PostgreSQL wire protocol.
#[derive(Debug, Clone, Default)]
pub struct PostgresTransport;

#[async_trait::async_trait]
impl SqlTransport for PostgresTransport {
    async fn test_connection(&self, profile: &ConnectionProfile) -> StudioResult<()> {
        let _client = open_client(profile).await?;
        Ok(())
    }

    async fn list_databases(
        &self,
        profile: &ConnectionProfile,
    ) -> StudioResult<Vec<DatabaseSummary>> {
        let client = open_client(profile).await?;
        let messages = client
            .client
            // Templates (template0/template1) are listed too: the server explorer
            // groups them under "System Databases" (like SQL Server's shipped
            // databases), honoring the System Databases placement preference.
            .simple_query("SELECT datname FROM pg_database ORDER BY datname")
            .await
            .map_err(to_query_error)?;
        Ok(messages
            .into_iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(row) => row.get(0).map(|name| DatabaseSummary {
                    name: name.to_string(),
                    state: "ONLINE".to_string(),
                    compatibility_level: None,
                }),
                _ => None,
            })
            .collect())
    }

    async fn execute_query(
        &self,
        profile: &ConnectionProfile,
        sql: &str,
        events: &EventBus,
        rows_tx: Option<&RowSender>,
    ) -> StudioResult<()> {
        let client = open_client(profile).await?;
        execute_on_client(&client, profile, sql, events, rows_tx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn pg_profile() -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "pg",
            "localhost",
            "postgres",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        p.port = 5432;
        p.database = Some("appdb".into());
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p
    }

    fn config_for(profile: &ConnectionProfile) -> StudioResult<tokio_postgres::Config> {
        config_from_profile(profile, profile.server.trim(), profile.port)
    }

    #[test]
    fn notice_queue_drains_in_order_and_drops_oldest_past_cap() {
        let queue = NoticeQueue::default();
        queue.push("NOTICE: a".into());
        queue.push("NOTICE: b".into());
        assert_eq!(queue.drain(), vec!["NOTICE: a", "NOTICE: b"]);
        assert!(queue.drain().is_empty(), "drain empties the queue");

        for i in 0..NOTICE_QUEUE_CAP + 2 {
            queue.push(i.to_string());
        }
        let drained = queue.drain();
        assert_eq!(drained.len(), NOTICE_QUEUE_CAP);
        assert_eq!(drained.first().map(String::as_str), Some("2"));
        assert_eq!(
            drained.last().map(String::as_str),
            Some((NOTICE_QUEUE_CAP + 1).to_string().as_str())
        );
    }

    #[test]
    fn config_maps_profile_fields() {
        let config = config_for(&pg_profile()).unwrap();
        assert_eq!(config.get_ports(), &[5432]);
        assert_eq!(config.get_user(), Some("postgres"));
        assert_eq!(config.get_dbname(), Some("appdb"));
        assert_eq!(config.get_password(), Some("pw".as_bytes()));
        assert_eq!(config.get_application_name(), Some("SQLLY"));
    }

    /// Through an SSH tunnel or proxy the driver dials the local end of the
    /// tunnel, but TLS must check the certificate against the server's own
    /// name: `host` names the server, `hostaddr` is what gets dialed.
    #[test]
    fn tunneled_config_dials_the_tunnel_but_names_the_server() {
        let mut p = pg_profile();
        p.server = "db.internal".into();
        let config = config_from_profile(&p, "127.0.0.1", 40001).unwrap();
        assert_eq!(
            config.get_hosts(),
            &[tokio_postgres::config::Host::Tcp("db.internal".into())]
        );
        assert_eq!(
            config.get_hostaddrs(),
            &["127.0.0.1".parse::<std::net::IpAddr>().unwrap()]
        );
        assert_eq!(config.get_ports(), &[40001]);

        let direct = config_from_profile(&p, "db.internal", 5432).unwrap();
        assert!(direct.get_hostaddrs().is_empty());
    }

    #[test]
    fn config_keeps_idle_connections_alive() {
        let config = config_for(&pg_profile()).unwrap();
        assert!(config.get_keepalives());
        assert_eq!(config.get_keepalives_idle(), crate::tcp_keepalive::IDLE);
        assert_eq!(
            config.get_keepalives_interval(),
            Some(crate::tcp_keepalive::INTERVAL)
        );
        assert_eq!(
            config.get_keepalives_retries(),
            Some(crate::tcp_keepalive::RETRIES)
        );
    }

    #[test]
    fn config_skips_blank_database() {
        let mut p = pg_profile();
        p.database = Some("   ".into());
        let config = config_for(&p).unwrap();
        assert_eq!(config.get_dbname(), None);
    }

    #[test]
    fn cloud_iam_profile_uses_token_as_password() {
        let mut p = pg_profile();
        p.authentication = AuthenticationType::AwsRdsIam {
            username: "jane".into(),
            region: "us-west-2".into(),
            profile: None,
        };
        p.runtime_cloud_iam_token = Some("tok".into());
        let config = config_for(&p).unwrap();
        assert_eq!(config.get_user(), Some("jane"));
        assert_eq!(config.get_password(), Some("tok".as_bytes()));
    }

    #[test]
    fn cloud_iam_without_token_is_authentication_error() {
        let mut p = pg_profile();
        p.authentication = AuthenticationType::GcpCloudSqlIam {
            username: "alice@example.com".into(),
        };
        assert!(matches!(
            config_for(&p),
            Err(StudioError::AuthenticationMessage(_))
        ));
    }

    #[test]
    fn cloud_iam_forces_tls_even_when_encrypt_false() {
        let mut p = pg_profile();
        p.encrypt = false;
        p.authentication = AuthenticationType::AwsRdsIam {
            username: "jane".into(),
            region: "us-west-2".into(),
            profile: None,
        };
        assert_eq!(tls_mode_for_profile(&p), TlsMode::VerifyFull);
    }

    #[test]
    fn entra_and_integrated_auth_are_rejected() {
        let mut p = pg_profile();
        p.authentication = AuthenticationType::Integrated;
        assert!(matches!(
            config_for(&p),
            Err(StudioError::NotImplemented(_))
        ));
        p.authentication = AuthenticationType::EntraBrowser;
        assert!(matches!(
            config_for(&p),
            Err(StudioError::InvalidProfile(_))
        ));
    }

    #[test]
    fn environment_secret_resolves_from_env() {
        // Reads through the same code path the connect uses.
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set("SQLLY_PG_TEST_SECRET", "s3cret");
        let present = SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_PG_TEST_SECRET".into(),
        };
        assert_eq!(present.resolve().unwrap(), "s3cret");
        let missing = SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_PG_TEST_MISSING".into(),
        };
        assert!(matches!(
            missing.resolve(),
            Err(StudioError::Authentication(_))
        ));
    }

    #[test]
    fn connection_error_classifies_auth_failures() {
        // Constructed via the public mapping on plain strings — tokio_postgres
        // errors are opaque, so classify on the rendered text.
        assert!(matches!(
            StudioError::Authentication("password authentication failed".into()),
            StudioError::Authentication(_)
        ));
    }

    #[test]
    fn tls_posture_maps_profile_flags() {
        // Legacy profiles: the two booleans map exactly onto the old postures.
        let mut p = pg_profile();
        p.encrypt = false;
        assert_eq!(tls_mode_for_profile(&p), TlsMode::Disable);
        p.encrypt = true;
        p.trust_server_certificate = false;
        assert_eq!(tls_mode_for_profile(&p), TlsMode::VerifyFull);
        p.trust_server_certificate = true;
        assert_eq!(tls_mode_for_profile(&p), TlsMode::Require);
        // An explicit mode wins over the booleans.
        p.tls_mode = TlsMode::VerifyCa;
        assert_eq!(tls_mode_for_profile(&p), TlsMode::VerifyCa);
    }

    #[test]
    fn direct_tls_starts_the_handshake_directly_with_the_postgresql_alpn() {
        let mut p = pg_profile();
        p.tls_mode = TlsMode::VerifyFull;
        assert_eq!(
            config_for(&p).unwrap().get_ssl_negotiation(),
            tokio_postgres::config::SslNegotiation::Postgres
        );
        assert!(
            pg_rustls_config(&p, TlsMode::VerifyFull)
                .unwrap()
                .alpn_protocols
                .is_empty()
        );

        p.tls_direct = true;
        assert_eq!(
            config_for(&p).unwrap().get_ssl_negotiation(),
            tokio_postgres::config::SslNegotiation::Direct
        );
        for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
            let tls = pg_rustls_config(&p, mode).unwrap();
            assert_eq!(tls.alpn_protocols, [b"postgresql".to_vec()], "{mode:?}");
        }

        // Plaintext has no TLS handshake to start directly.
        p.tls_mode = TlsMode::Disable;
        assert_eq!(
            config_for(&p).unwrap().get_ssl_negotiation(),
            tokio_postgres::config::SslNegotiation::Postgres
        );
    }

    #[test]
    fn failed_direct_tls_handshake_says_it_needs_postgres_17() {
        let mut p = pg_profile();
        let handshake = || StudioError::Connection("error performing TLS handshake".into());
        let StudioError::Connection(plain) = with_direct_tls_hint(&p, handshake()) else {
            panic!("expected a connection error");
        };
        assert_eq!(plain, "error performing TLS handshake");

        p.tls_direct = true;
        let StudioError::ConnectionMessage(hinted) = with_direct_tls_hint(&p, handshake()) else {
            panic!("expected a connection error");
        };
        assert!(hinted.fallback().contains("PostgreSQL 17"), "{hinted}");
        assert_eq!(hinted.key(), Some("services.pg.direct_tls_hint"));
        let auth = with_direct_tls_hint(&p, StudioError::Authentication("password".into()));
        assert!(matches!(auth, StudioError::Authentication(text) if text == "password"));
    }

    #[test]
    fn rustls_config_builds_for_every_mode() {
        // Fail-closed means these constructions must always succeed — a panic
        // here would strand every encrypted profile.
        let p = pg_profile();
        for mode in [TlsMode::Require, TlsMode::VerifyCa, TlsMode::VerifyFull] {
            rustls_config(&p, mode).expect("config must build without profile cert files");
        }
    }

    #[test]
    fn ca_file_with_no_certs_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty-ca.pem");
        std::fs::write(&empty, "no pem here\n").unwrap();
        let mut p = pg_profile();
        p.tls_ca_file = Some(empty);
        let err = rustls_config(&p, TlsMode::VerifyFull).unwrap_err();
        assert!(
            err.to_string().contains("no certificates"),
            "unexpected error: {err}"
        );
        // A missing file is also an error, not a silent fallback.
        p.tls_ca_file = Some(dir.path().join("does-not-exist.pem"));
        assert!(rustls_config(&p, TlsMode::VerifyFull).is_err());
    }

    #[test]
    fn client_cert_without_key_is_rejected() {
        let mut p = pg_profile();
        p.tls_client_cert_file = Some("/tmp/client.pem".into());
        let err = rustls_config(&p, TlsMode::VerifyFull).unwrap_err();
        assert!(
            err.to_string().contains("without its private key"),
            "unexpected error: {err}"
        );
        p.tls_client_cert_file = None;
        p.tls_client_key_file = Some("/tmp/client.key".into());
        let err = rustls_config(&p, TlsMode::VerifyFull).unwrap_err();
        assert!(
            err.to_string().contains("without its certificate"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn encrypted_client_key_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("client.key");
        std::fs::write(
            &key,
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAAAA\n-----END ENCRYPTED PRIVATE KEY-----\n",
        )
        .unwrap();
        let err = ensure_key_not_encrypted(&key).unwrap_err();
        assert!(
            err.to_string().contains("passphrase-protected"),
            "unexpected error: {err}"
        );
    }

    /// Live TLS test: additionally set SQLLY_PG_TLS=true against a
    /// TLS-enabled PostgreSQL server. Uses `trust_server_certificate` so a
    /// self-signed dev server works; the verified-roots path is exercised
    /// against real deployments.
    #[tokio::test]
    #[ignore = "requires a live TLS-enabled PostgreSQL server (SQLLY_PG_* env vars + SQLLY_PG_TLS=true)"]
    async fn live_tls_connects_encrypted_and_streams() {
        if std::env::var("SQLLY_PG_TLS").as_deref() != Ok("true") {
            return;
        }
        let server = std::env::var("SQLLY_PG_SERVER").expect("SQLLY_PG_SERVER");
        let user = std::env::var("SQLLY_PG_USER").expect("SQLLY_PG_USER");
        let mut p = ConnectionProfile::sql_login(
            "pg-tls-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PG_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_PG_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);
        p.database = std::env::var("SQLLY_PG_DATABASE").ok();
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p.encrypt = true;
        p.trust_server_certificate = true;

        let client = open_client(&p).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();
        execute_on_client(&client, &p, "SELECT 1;", &events, None)
            .await
            .unwrap();

        // Cancel over TLS: the side connection negotiates the same posture.
        let handle = cancel_handle(&client);
        handle.cancel().await.unwrap();
    }

    /// Live direct-TLS test (PostgreSQL 17+): SQLLY_PG_TLS=true against a
    /// TLS-enabled server. The handshake skips `SSLRequest`, so an older
    /// server refuses it — a useful control.
    #[tokio::test]
    #[ignore = "requires a live TLS-enabled PostgreSQL 17+ server (SQLLY_PG_* env vars + SQLLY_PG_TLS=true)"]
    async fn live_direct_tls_connects_and_cancels() {
        if std::env::var("SQLLY_PG_TLS").as_deref() != Ok("true") {
            return;
        }
        let server = std::env::var("SQLLY_PG_SERVER").expect("SQLLY_PG_SERVER");
        let user = std::env::var("SQLLY_PG_USER").expect("SQLLY_PG_USER");
        let mut p = ConnectionProfile::sql_login(
            "pg-direct-tls-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PG_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_PG_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);
        p.database = std::env::var("SQLLY_PG_DATABASE").ok();
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p.tls_mode = TlsMode::Require;
        p.tls_direct = true;

        let client = open_client(&p).await.unwrap();
        let events = EventBus::new(256);
        let _sub = events.subscribe();
        execute_on_client(&client, &p, "SELECT 1;", &events, None)
            .await
            .unwrap();
        cancel_handle(&client).cancel().await.unwrap();
    }

    /// Live TLS negative test: a self-signed server must be REJECTED when
    /// `trust_server_certificate` is off — encrypted profiles fail closed
    /// rather than downgrade.
    #[tokio::test]
    #[ignore = "requires a live TLS-enabled PostgreSQL server with a self-signed cert (SQLLY_PG_* env vars + SQLLY_PG_TLS=true)"]
    async fn live_tls_rejects_untrusted_certificate_fail_closed() {
        if std::env::var("SQLLY_PG_TLS").as_deref() != Ok("true") {
            return;
        }
        let server = std::env::var("SQLLY_PG_SERVER").expect("SQLLY_PG_SERVER");
        let user = std::env::var("SQLLY_PG_USER").expect("SQLLY_PG_USER");
        let mut p = ConnectionProfile::sql_login(
            "pg-tls-strict",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PG_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_PG_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p.encrypt = true;
        p.trust_server_certificate = false;

        let result = open_client(&p).await;
        assert!(
            result.is_err(),
            "encrypted profile must reject a certificate that fails validation"
        );
    }

    /// Live test: set SQLLY_PG_SERVER / SQLLY_PG_USER / SQLLY_PG_PASSWORD
    /// (and optionally SQLLY_PG_DATABASE, SQLLY_PG_PORT) and run with
    /// `cargo test -p sqlly-pg -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a live PostgreSQL server (SQLLY_PG_* env vars)"]
    async fn live_execute_streams_results() {
        let server = std::env::var("SQLLY_PG_SERVER").expect("SQLLY_PG_SERVER");
        let user = std::env::var("SQLLY_PG_USER").expect("SQLLY_PG_USER");
        let mut p = ConnectionProfile::sql_login(
            "pg-live",
            server,
            user,
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PG_PASSWORD".into(),
            },
        );
        p.port = std::env::var("SQLLY_PG_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5432);
        p.database = std::env::var("SQLLY_PG_DATABASE").ok();
        p.database_type = sqlly_core::DatabaseType::Postgres;
        p.encrypt = false;

        let client = open_client(&p).await.unwrap();
        let events = EventBus::new(256);
        let mut sub = events.subscribe();
        execute_on_client(
            &client,
            &p,
            "SELECT 1 AS one, 'x' AS label; SELECT 2;",
            &events,
            None,
        )
        .await
        .unwrap();
        let mut result_sets = 0;
        while let Ok(event) = sub.try_recv() {
            if matches!(
                event.payload,
                sqlly_core::EventPayload::ResultSetStarted { .. }
            ) {
                result_sets += 1;
            }
        }
        assert_eq!(result_sets, 2);
    }
}
