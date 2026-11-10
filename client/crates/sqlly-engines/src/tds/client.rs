//! Opening a warm `tiberius` client from a [`ConnectionProfile`] — TCP + TLS +
//! LOGIN7 config, authentication (SQL login, Microsoft Entra, Azure SQL
//! gateway login routing) — plus server-side cancellation of a running query.

use sqlly_core::{AuthenticationType, ConnectionProfile, StudioError, StudioResult};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::tds::execute::execute_batch_on_client;

/// Concrete `tiberius` client type used throughout this crate. Public so
/// long-running hosts (the persistent engine) can hold and reuse a warm client
/// across queries via [`open_client`] / [`crate::tds::execute_on_client`].
pub type TdsClient = Client<Compat<TcpStream>>;

/// Establish a warm `tiberius` client for a profile without running a query.
///
/// Exposed so long-running hosts (the persistent engine) can cache and reuse a
/// live connection across requests instead of paying the TCP + TLS + LOGIN7
/// handshake (~170ms locally) on every query. The actual SELECT is sub-millisecond,
/// so connection reuse is the single largest latency win.
pub async fn open_client(profile: &ConnectionProfile) -> StudioResult<TdsClient> {
    let mut client = connect(profile).await?;
    apply_startup_sql(&mut client, profile).await?;
    Ok(client)
}

async fn apply_startup_sql(
    client: &mut TdsClient,
    profile: &ConnectionProfile,
) -> StudioResult<()> {
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

/// Server-side cancellation handle for a [`TdsClient`]: the session id
/// (`@@SPID`) captured while the client is idle, plus the profile needed to
/// open a short side connection that issues `KILL <spid>`.
///
/// `tiberius` does not expose the TDS Attention signal, so killing the session
/// is the connection-aware equivalent. That is stronger than Attention (the
/// whole session dies, not just the running batch), which matches SQLLY's
/// existing contract: a client whose stream was abandoned mid-run MUST be
/// discarded anyway. `KILL` requires the ALTER ANY CONNECTION permission
/// (sysadmin/processadmin); without it the request fails and cancellation
/// silently degrades to client-side only — cancel is best-effort by design.
pub struct TdsCancelHandle {
    profile: ConnectionProfile,
    spid: i16,
}

impl TdsCancelHandle {
    /// Ask the server to stop the session's running batch. Best-effort: a
    /// failure (most commonly missing KILL permission) means the statement
    /// keeps running server-side.
    pub async fn cancel(self) -> StudioResult<()> {
        let mut side = connect(&self.profile).await?;
        side.simple_query(format!("KILL {}", self.spid))
            .await
            .map_err(|e| StudioError::QueryExecution(format!("cancel request failed: {e}")))?
            .into_results()
            .await
            .map_err(|e| StudioError::QueryExecution(format!("cancel request failed: {e}")))?;
        Ok(())
    }
}

/// Obtain a cancellation handle for the client's connection. Must be called
/// while the client is idle (it runs a sub-millisecond `SELECT @@SPID` round
/// trip on the live connection).
pub async fn cancel_handle(
    client: &mut TdsClient,
    profile: &ConnectionProfile,
) -> StudioResult<TdsCancelHandle> {
    let row = client
        .simple_query("SELECT @@SPID")
        .await
        .map_err(|e| StudioError::QueryExecution(format!("spid probe failed: {e}")))?
        .into_row()
        .await
        .map_err(|e| StudioError::QueryExecution(format!("spid probe failed: {e}")))?
        .ok_or_else(|| {
            StudioError::QueryExecutionMessage(sqlly_core::DisplayMessage::new(
                "services.tds.errors.spid_no_row",
                "Query execution error: spid probe returned no row",
            ))
        })?;
    let spid: i16 = row.get(0).ok_or_else(|| {
        StudioError::QueryExecutionMessage(sqlly_core::DisplayMessage::new(
            "services.tds.errors.spid_no_value",
            "Query execution error: spid probe returned no value",
        ))
    })?;
    Ok(TdsCancelHandle {
        profile: profile.clone(),
        spid,
    })
}

pub(crate) async fn connect(profile: &ConnectionProfile) -> StudioResult<TdsClient> {
    let (host, port) = crate::ssh::TunnelManager::global()
        .endpoint_for(profile)
        .await?;
    // The config names the server (TLS certificate name, LOGIN7 server
    // name, Azure gateway routing); the socket goes to `host:port`, which is
    // the local end of an SSH tunnel or proxy when the profile has one.
    let config = config_from_profile(profile)?;
    let timeout = profile.timeout();
    // Bound the entire connect sequence (TCP connect + TLS handshake + login).
    // Without this, a stalled handshake or an unreachable/unresponsive server
    // blocks forever, which previously wedged the engine's query task with no
    // terminal frame (the client UI stuck on "Query Running" indefinitely).
    let attempt = async {
        let tcp = TcpStream::connect(dial_address(&host, port))
            .await
            .map_err(|err| connection_error(profile.server.trim(), &err.to_string()))?;
        tcp.set_nodelay(true)
            .map_err(|err| StudioError::Connection(err.to_string()))?;
        // Best effort, like the PostgreSQL and MySQL drivers: a refused
        // socket option must not cost the connection.
        if let Err(err) = crate::tcp_keepalive::apply(&tcp) {
            tracing::warn!(server = %profile.server.trim(), %err, "could not enable TCP keepalive");
        }
        Client::connect(config, tcp.compat_write())
            .await
            .map_err(|err| login_error(profile.server.trim(), err))
    };

    match tokio::time::timeout(timeout, attempt).await {
        Ok(result) => result,
        Err(_) => Err(StudioError::Connection(format!(
            "connection timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

/// Where the socket goes: the transport endpoint ([`TunnelManager`]'s local
/// end of an SSH tunnel or proxy, or the server itself for a direct
/// connection).
///
/// [`TunnelManager`]: crate::ssh::TunnelManager
fn dial_address(host: &str, port: u16) -> String {
    format!("{}:{port}", host.trim())
}

/// The error for a failed TLS handshake or login. A server that only offers
/// TLS 1.0 or 1.1 gets a message naming the cause and the fix; SQLLY never
/// falls back to those versions, so the raw rustls text ("peer is
/// incompatible") would leave the user with nothing to act on.
fn login_error(server: &str, err: tiberius::error::Error) -> StudioError {
    if !is_legacy_tls_only(&err) {
        return StudioError::Connection(err.to_string());
    }
    StudioError::ConnectionMessage(
        sqlly_core::DisplayMessage::new(
            "services.errors.sql_server_legacy_tls",
            format!(
                "Connection error: {server} only offers TLS 1.0 or 1.1, and SQLLY requires TLS 1.2 or newer. \
                 Install the TLS 1.2 update for the server (Microsoft publishes one for SQL Server 2008 R2, 2012 and 2014) or upgrade SQL Server. \
                 Turning off Encrypt does not help: SQL Server still uses TLS to protect the login."
            ),
        )
        .with_arg("server", server),
    )
}

/// Whether a connect failed because the server only speaks TLS older than
/// 1.2: it answered with a TLS 1.0/1.1 ServerHello (rustls refuses it), or
/// it rejected our TLS 1.2+ hello with a `protocol_version` alert. tiberius
/// hands the rustls error over as text inside an I/O error, so the check
/// compares against rustls's own rendering of exactly those two errors.
fn is_legacy_tls_only(err: &tiberius::error::Error) -> bool {
    let text = match err {
        tiberius::error::Error::Io { message, .. } => message.as_str(),
        tiberius::error::Error::Tls(message) => message.as_str(),
        _ => return false,
    };
    [
        rustls::Error::PeerIncompatible(rustls::PeerIncompatible::ServerDoesNotSupportTls12Or13),
        rustls::Error::AlertReceived(rustls::AlertDescription::ProtocolVersion),
    ]
    .iter()
    .any(|known| text.contains(&known.to_string()))
}

fn connection_error(hostname: &str, error: &str) -> StudioError {
    let message = connection_error_message(hostname, error);
    if message.starts_with("We couldn't find the hostname") {
        StudioError::ConnectionMessage(
            sqlly_core::DisplayMessage::new(
                "services.errors.connection_hostname_not_found",
                format!("Connection error: {message}"),
            )
            .with_arg("hostname", hostname),
        )
    } else {
        crate::connect_hint::connection_with_local_network_hint(error.to_string())
    }
}

fn connection_error_message(hostname: &str, error: &str) -> String {
    let error_lowercase = error.to_ascii_lowercase();
    if error_lowercase.contains("failed to lookup address information")
        || error_lowercase.contains("nodename nor servname provided, or not known")
        || error_lowercase.contains("name or service not known")
        || error_lowercase.contains("no such host is known")
        || error_lowercase.contains("temporary failure in name resolution")
    {
        format!("We couldn't find the hostname \"{hostname}\". Make sure you entered it correctly.")
    } else {
        crate::connect_hint::with_local_network_hint(error.to_string())
    }
}

fn config_from_profile(profile: &ConnectionProfile) -> StudioResult<Config> {
    let mut config = Config::new();
    let server = profile.server.trim();
    if server.is_empty() {
        return Err(StudioError::InvalidProfile(
            "server is required".to_string(),
        ));
    }
    config.host(server);
    config.port(profile.port);
    config.application_name(&profile.application_name);
    if let Some(database) = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|db| !db.is_empty())
    {
        config.database(database);
    }
    config.encryption(if profile.encrypt {
        EncryptionLevel::Required
    } else {
        EncryptionLevel::Off
    });
    if profile.trust_server_certificate {
        config.trust_cert();
    }
    // tiberius 0.13 introduced bounded defaults for both of its timeout knobs
    // (15s handshake, 30s per server round-trip while reading results). The
    // handshake is already bounded by `profile.timeout()` in `connect`, and a
    // SQL client must let a long-running statement (a big sort, a slow
    // procedure, a WAITFOR) run until the user cancels it or the profile's
    // statement timeout fires — so restore the pre-0.13 wait-indefinitely
    // behaviour for both.
    config.handshake_timeout(None);
    config.command_timeout(None);

    let auth = match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => {
            let password = resolve_secret(secret_ref)?;
            let login = azure_sql_login(server, username);
            AuthMethod::sql_server(login, password)
        }
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            // The interactive device-code / browser flow (sqlly-security's
            // EntraIdAuthProvider) yields an AAD access token that the UI caches
            // and exposes to the transport. The persistent engine sends it on the
            // request profile; one-shot CLI execution can still use environment
            // variables for compatibility.
            let token = profile
                .runtime_entra_access_token
                .clone()
                .map(Ok)
                .unwrap_or_else(|| entra_access_token(server))?;
            AuthMethod::aad_token(token)
        }
        AuthenticationType::Integrated => {
            // Integrated (SSPI/Kerberos) auth requires a platform-specific
            // tiberius feature that is not enabled in this build. Surface a
            // precise, actionable error rather than a generic NotImplemented.
            return Err(StudioError::Authentication(
                "integrated authentication requires the platform integrated-auth feature; \
                 use SQL or Microsoft Entra authentication on this build"
                    .to_string(),
            ));
        }
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. } => {
            return Err(StudioError::InvalidProfile(
                "Cloud IAM authentication applies to PostgreSQL and MySQL, not SQL Server".into(),
            ));
        }
        AuthenticationType::AuthToken { .. } => {
            return Err(StudioError::InvalidProfile(
                "Auth-token sign-in applies to libSQL / Turso, not SQL Server".into(),
            ));
        }
    };
    config.authentication(auth);

    Ok(config)
}

/// Whether `host` is an Azure SQL **Database** logical-server endpoint reached
/// through the multi-tenant gateway (as opposed to a Managed Instance).
///
/// Azure SQL Database servers are addressed as `<server>.database.windows.net`,
/// a single label in front of the well-known suffix. Managed Instance carries
/// an extra DNS-zone label (`<instance>.<zone>.database.windows.net`) and is a
/// full SQL Server engine that does NOT use the gateway's user-name routing, so
/// it is deliberately excluded here.
fn is_azure_sql_db_gateway(host: &str) -> bool {
    host.to_ascii_lowercase()
        .strip_suffix(".database.windows.net")
        .is_some_and(|prefix| !prefix.is_empty() && !prefix.contains('.'))
}

/// Normalizes a SQL-authentication user name for Azure SQL Database.
///
/// The Azure SQL Database gateway determines the target logical server from the
/// segment *after the final `@`* in the login name. A login whose own name
/// contains `@` — e.g. `dba@contoso.com` — is therefore misread as a
/// request for server `contoso.com`, and the gateway fails the login
/// with error 40532 ("Cannot open server '<x>' requested by the login. The
/// login failed."), even though TLS SNI and the TDS LOGIN7 `ServerName` field
/// both already carry the correct host.
///
/// SSMS, sqlcmd and the SQL ODBC/JDBC drivers work around this by appending
/// `@<short-server-name>` so the trailing segment names the real logical
/// server. We do the same, but only when:
///   * the endpoint is an Azure SQL Database gateway host (not a Managed
///     Instance, which would treat the suffix as part of the literal login), and
///   * the login already contains an `@` whose trailing segment is not already
///     the correct short server name (avoids double-suffixing and leaves
///     plain, working logins untouched).
fn azure_sql_login(server: &str, username: &str) -> String {
    if !is_azure_sql_db_gateway(server) {
        return username.to_string();
    }
    let Some(short) = server.split('.').next().filter(|s| !s.is_empty()) else {
        return username.to_string();
    };
    let needs_routing_suffix = username.contains('@')
        && !username
            .rsplit('@')
            .next()
            .unwrap_or_default()
            .eq_ignore_ascii_case(short);
    if needs_routing_suffix {
        format!("{username}@{short}")
    } else {
        username.to_string()
    }
}

/// Resolves the password for a SQL login through the shared
/// [`sqlly_core::secret_resolver::resolve_secret`] boundary (`InMemory` carries
/// the value; `Environment` reads an env var; `MacOsKeychain` reads the OS
/// keyring; missing secrets error).
fn resolve_secret(secret_ref: &sqlly_core::SecretRef) -> StudioResult<String> {
    sqlly_core::secret_resolver::resolve_secret(secret_ref)
}

/// Looks up the cached Microsoft Entra access token for `server`, preferring a
/// server-scoped variable (`SQLLY_ENTRA_TOKEN_<host>`, with non-alphanumeric
/// characters folded to `_` and upper-cased) over the generic
/// `SQLLY_ENTRA_ACCESS_TOKEN`. Legacy `OSXQL_*` names remain supported.
fn entra_access_token(server: &str) -> StudioResult<String> {
    let scoped_key = format!(
        "SQLLY_ENTRA_TOKEN_{}",
        server
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            })
            .collect::<String>()
    );
    let legacy_scoped_key = scoped_key.replacen("SQLLY_", "OSXQL_", 1);
    std::env::var(&scoped_key)
        .or_else(|_| std::env::var(&legacy_scoped_key))
        .or_else(|_| std::env::var("SQLLY_ENTRA_ACCESS_TOKEN"))
        .or_else(|_| std::env::var("OSXQL_ENTRA_ACCESS_TOKEN"))
        .map_err(|_| {
            StudioError::Authentication(format!(
                "missing Microsoft Entra access token; set {scoped_key} or SQLLY_ENTRA_ACCESS_TOKEN"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    /// What rustls really reports when a server answers our ClientHello with
    /// a TLS 1.0 ServerHello, wrapped the way tokio-rustls hands it to
    /// tiberius (an `io::Error` carrying the rustls error).
    fn tls10_server_hello_error() -> tiberius::error::Error {
        use std::io::Write as _;
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(rustls::RootCertStore::empty())
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from("legacy.example.test").unwrap();
        let mut conn = rustls::ClientConnection::new(std::sync::Arc::new(config), name).unwrap();
        let mut client_hello = Vec::new();
        conn.write_tls(&mut client_hello).unwrap();
        // ServerHello choosing TLS 1.0 (0x0301): what an unpatched SQL
        // Server 2008 R2 / 2012 / 2014 sends back.
        let mut body = vec![0x03, 0x01];
        body.extend_from_slice(&[0x11; 32]); // random
        body.push(0); // session id length
        body.extend_from_slice(&[0x00, 0x2f]); // TLS_RSA_WITH_AES_128_CBC_SHA
        body.push(0); // null compression
        let mut handshake = vec![0x02, 0, 0, body.len() as u8];
        handshake.extend_from_slice(&body);
        let mut record = vec![0x16, 0x03, 0x01, 0, handshake.len() as u8];
        record.extend_from_slice(&handshake);
        let mut incoming: &[u8] = &record;
        conn.read_tls(&mut incoming).unwrap();
        let err = conn
            .process_new_packets()
            .expect_err("a TLS 1.0 ServerHello must be refused");
        let mut sink = Vec::new();
        let _ = sink.write_all(b"");
        tiberius::error::Error::from(std::io::Error::new(std::io::ErrorKind::InvalidData, err))
    }

    #[test]
    fn a_tls10_only_server_gets_the_legacy_tls_message() {
        let err = tls10_server_hello_error();
        assert!(is_legacy_tls_only(&err), "{err}");
        let mapped = login_error("sql2008.example.test", err);
        let StudioError::ConnectionMessage(message) = &mapped else {
            panic!("expected a keyed connection message, got {mapped:?}");
        };
        assert_eq!(message.key(), Some("services.errors.sql_server_legacy_tls"));
        let text = message.fallback();
        assert!(text.contains("sql2008.example.test"), "{text}");
        assert!(text.contains("TLS 1.0 or 1.1"), "{text}");
        assert!(text.contains("TLS 1.2"), "{text}");
        assert!(text.contains("2008 R2"), "{text}");
        assert!(text.contains("Encrypt"), "{text}");
    }

    #[test]
    fn a_protocol_version_alert_from_the_server_gets_the_legacy_tls_message() {
        let err = tiberius::error::Error::from(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::AlertReceived(rustls::AlertDescription::ProtocolVersion),
        ));
        assert!(is_legacy_tls_only(&err), "{err}");
        assert!(matches!(
            login_error("db", err),
            StudioError::ConnectionMessage(_)
        ));
    }

    #[test]
    fn other_tls_and_login_failures_keep_their_own_text() {
        for err in [
            tiberius::error::Error::from(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::AlertReceived(rustls::AlertDescription::HandshakeFailure),
            )),
            tiberius::error::Error::from(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "connection reset by peer",
            )),
            tiberius::error::Error::Tls("the server's certificate was rejected".into()),
        ] {
            assert!(!is_legacy_tls_only(&err), "{err}");
            let text = err.to_string();
            match login_error("db", err) {
                StudioError::Connection(message) => assert_eq!(message, text),
                other => panic!("expected the raw connection error, got {other:?}"),
            }
        }
    }

    fn profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "password".to_string(),
            },
        )
    }

    fn config_for(profile: &ConnectionProfile) -> StudioResult<Config> {
        config_from_profile(profile)
    }

    #[test]
    fn sql_login_in_memory_secret_builds_sql_server_auth() {
        let config = config_for(&profile()).expect("config builds");
        assert_eq!(config.get_addr(), "localhost:1433");
    }

    /// Through an SSH tunnel or proxy the socket goes to the local end of the
    /// tunnel, but TLS (certificate name) and the login (LOGIN7 server name,
    /// Azure gateway routing) must see the server's own name.
    #[test]
    fn tunneled_config_names_the_server_not_the_tunnel() {
        let mut profile = profile();
        profile.server = "db.internal".to_string();
        let config = config_from_profile(&profile).expect("config builds");
        assert_eq!(config.get_addr(), format!("db.internal:{}", profile.port));
        assert_eq!(dial_address("127.0.0.1", 40001), "127.0.0.1:40001");
    }

    #[test]
    fn config_trims_endpoint_before_addressing() {
        let mut profile = profile();
        profile.server = "\tsql-contoso-prod.database.windows.net\r\n".to_string();
        profile.database = Some(" contoso\n".to_string());

        let config = config_for(&profile).expect("config builds");

        assert_eq!(
            config.get_addr(),
            "sql-contoso-prod.database.windows.net:1433"
        );
    }

    #[test]
    fn config_rejects_empty_server_after_trim() {
        let mut profile = profile();
        profile.server = " \r\n".to_string();

        let err = config_for(&profile).unwrap_err();

        match err {
            StudioError::InvalidProfile(msg) => assert_eq!(msg, "server is required"),
            other => panic!("expected InvalidProfile error, got {other:?}"),
        }
    }

    #[test]
    fn unresolved_hostname_has_an_actionable_message() {
        assert_eq!(
            connection_error_message(
                "missing.database.windows.net",
                "failed to lookup address information: nodename nor servname provided, or not known",
            ),
            "We couldn't find the hostname \"missing.database.windows.net\". Make sure you entered it correctly."
        );
    }

    #[test]
    fn other_connection_errors_keep_their_detail() {
        assert_eq!(
            connection_error_message("server.example", "Connection refused (os error 61)"),
            "Connection refused (os error 61)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn no_route_to_host_explains_local_network_privacy() {
        let text = connection_error_message("192.168.1.50", "No route to host (os error 65)");
        assert!(text.contains("Local Network"), "{text}");
    }

    #[test]
    fn entra_auth_uses_cached_access_token_from_env() {
        // Server-scoped variable name derived from the host.
        let server = "entratest.database.windows.net";
        let scoped = "SQLLY_ENTRA_TOKEN_ENTRATEST_DATABASE_WINDOWS_NET";
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set(scoped, "fake-jwt-token");
        let profile = ConnectionProfile::azure_sql_entra_browser("azure", server, "sales");
        let config = config_for(&profile);
        drop(env);
        assert!(
            config.is_ok(),
            "entra config should build with cached token"
        );
    }

    #[test]
    fn entra_auth_uses_runtime_access_token_without_env() {
        let mut profile = ConnectionProfile::azure_sql_entra_browser(
            "azure",
            "runtime-token.database.windows.net",
            "sales",
        );
        profile.runtime_entra_access_token = Some("fake-runtime-token".to_string());

        let config = config_for(&profile);

        assert!(
            config.is_ok(),
            "entra config should build with runtime token"
        );
    }

    #[test]
    fn entra_auth_without_token_reports_actionable_error() {
        // Use a host whose scoped var and the generic var are both unset.
        let profile =
            ConnectionProfile::azure_sql_entra_device_code("azure", "no-token-host-xyz", "sales");
        // Ensure the generic fallback is absent for this assertion.
        let had_generic = std::env::var("SQLLY_ENTRA_ACCESS_TOKEN").is_ok()
            || std::env::var("OSXQL_ENTRA_ACCESS_TOKEN").is_ok();
        if !had_generic {
            let err = config_for(&profile).unwrap_err();
            match err {
                StudioError::Authentication(msg) => {
                    assert!(msg.contains("Entra access token"), "msg: {msg}");
                }
                other => panic!("expected Authentication error, got {other:?}"),
            }
        }
    }

    #[test]
    fn integrated_auth_reports_actionable_error() {
        let mut profile = profile();
        profile.authentication = AuthenticationType::Integrated;
        let err = config_for(&profile).unwrap_err();
        match err {
            StudioError::Authentication(msg) => {
                assert!(msg.contains("integrated authentication"), "msg: {msg}");
            }
            other => panic!("expected Authentication error, got {other:?}"),
        }
    }

    // --- Azure SQL Database gateway login routing (error 40532) ---------------

    #[test]
    fn azure_login_appends_short_server_when_username_contains_at() {
        // The reported failure: an `@`-bearing login is misrouted by the gateway
        // to the domain after the `@`. Appending `@<short-server>` fixes routing.
        assert_eq!(
            azure_sql_login("sql-contoso-prod.database.windows.net", "dba@contoso.com"),
            "dba@contoso.com@sql-contoso-prod"
        );
    }

    #[test]
    fn azure_login_leaves_plain_username_untouched() {
        // A login without `@` already routes via the LOGIN7 ServerName field;
        // we must not alter it (and risk regressing working connections).
        assert_eq!(
            azure_sql_login("sql-contoso-prod.database.windows.net", "contosoadmin"),
            "contosoadmin"
        );
    }

    #[test]
    fn azure_login_does_not_double_suffix_correct_routing() {
        // Already-correct `login@server` form (any case) is left as-is.
        assert_eq!(
            azure_sql_login(
                "sql-contoso-prod.database.windows.net",
                "dba@sql-contoso-prod"
            ),
            "dba@sql-contoso-prod"
        );
        assert_eq!(
            azure_sql_login(
                "sql-contoso-prod.database.windows.net",
                "dba@SQL-CONTOSO-PROD"
            ),
            "dba@SQL-CONTOSO-PROD"
        );
    }

    #[test]
    fn non_azure_host_never_rewrites_username() {
        // On-prem / container SQL Server keeps email-style logins verbatim.
        assert_eq!(
            azure_sql_login("localhost", "dba@contoso.com"),
            "dba@contoso.com"
        );
        assert_eq!(
            azure_sql_login("sql.internal.corp", "dba@contoso.com"),
            "dba@contoso.com"
        );
    }

    #[test]
    fn managed_instance_is_excluded_from_gateway_routing() {
        // Managed Instance (extra DNS-zone label) is a full engine that treats
        // the `@`-suffix as part of the literal login, so it must be untouched.
        assert!(!is_azure_sql_db_gateway(
            "myinst.a1b2c3d4e5f6.database.windows.net"
        ));
        assert_eq!(
            azure_sql_login(
                "myinst.a1b2c3d4e5f6.database.windows.net",
                "dba@contoso.com"
            ),
            "dba@contoso.com"
        );
    }

    #[test]
    fn gateway_detection_matches_only_single_label_db_hosts() {
        assert!(is_azure_sql_db_gateway(
            "sql-contoso-prod.database.windows.net"
        ));
        assert!(is_azure_sql_db_gateway(
            "SQL-CONTOSO-PROD.DATABASE.WINDOWS.NET"
        ));
        assert!(!is_azure_sql_db_gateway("localhost"));
        assert!(!is_azure_sql_db_gateway(".database.windows.net"));
        // The bare gateway suffix with an empty label is not a server.
        assert!(!is_azure_sql_db_gateway("database.windows.net"));
    }
}
