//! Why a MySQL / MariaDB connection attempt failed, in the user's terms.
//!
//! The driver reports a failed handshake as a rustls, socket or server error
//! ("invalid peer certificate: UnknownIssuer", "ERROR 1698 (28000)"), which
//! names no setting the user can change. Each cause a stock server produces
//! gets its own keyed message saying what happened and what to change; the
//! raw text stays at the end as `{detail}`. Anything unrecognized keeps the
//! driver's wording.
//!
//! Every template below is the English catalog value of its key, verbatim.

use sqlly_core::{ConnectionProfile, DisplayMessage, StudioError};

/// What the user asked to connect to, for naming in a message.
pub(super) struct ConnectAttempt {
    pub server: String,
    pub port: u16,
    pub user: String,
    pub database: String,
}

impl ConnectAttempt {
    pub(super) fn new(profile: &ConnectionProfile, opts: &mysql_async::Opts) -> Self {
        Self {
            server: profile.server.trim().to_string(),
            port: profile.port,
            user: opts.user().unwrap_or_default().to_string(),
            database: opts.db_name().unwrap_or_default().to_string(),
        }
    }
}

const TLS_UNTRUSTED_CERTIFICATE: (&str, &str) = (
    "services.mysql.connect_tls_untrusted_certificate",
    "Connection error: the TLS certificate from {server} is not trusted. It is self-signed or was issued by a certificate authority this computer does not know, and this connection's TLS mode checks the certificate. MariaDB 11.4 and later and MySQL 5.7 and later create a self-signed certificate on their own, so this is normal for a local or test server. To connect, edit the connection and set TLS to \"Require (no verification)\" (still encrypted), or keep verification and choose the server's CA certificate file in the TLS section. Driver detail: {detail}",
);
const TLS_HOSTNAME_MISMATCH: (&str, &str) = (
    "services.mysql.connect_tls_hostname_mismatch",
    "Connection error: the TLS certificate from {server} was issued for a different host name. It does not list the name or address you are connecting with, and the TLS mode \"Verify full\" checks that. To connect, use the host name the certificate was issued for, or edit the connection and set TLS to \"Verify CA\" (checks the certificate authority but not the host name). Driver detail: {detail}",
);
const TLS_CERTIFICATE_EXPIRED: (&str, &str) = (
    "services.mysql.connect_tls_certificate_expired",
    "Connection error: the TLS certificate from {server} has expired or is not valid yet. Check that the date and time on this computer are correct; if they are, the certificate must be renewed on the server. To connect without checking the certificate, edit the connection and set TLS to \"Require (no verification)\". Driver detail: {detail}",
);
const TLS_FAILED: (&str, &str) = (
    "services.mysql.connect_tls_failed",
    "Connection error: the encrypted (TLS) connection to {server} could not be set up. The server and SQLLY could not agree on TLS settings, or a certificate was rejected. Check the TLS section of the connection: the TLS mode, the CA certificate file, and the client certificate and key. Driver detail: {detail}",
);
const TLS_NOT_ENABLED_ON_SERVER: (&str, &str) = (
    "services.mysql.connect_tls_not_enabled_on_server",
    "Connection error: {server} does not have TLS (SSL) turned on, but this connection's TLS mode requires an encrypted connection, and SQLLY never falls back to an unencrypted one on its own. To connect, edit the connection and set TLS to \"Disable\" (the connection will not be encrypted), or turn on TLS on the server. MariaDB 11.3 and earlier has TLS off unless it was configured. Driver detail: {detail}",
);
const SOCKET_ONLY_ACCOUNT: (&str, &str) = (
    "services.mysql.connect_socket_only_account",
    "Authentication failed: the account {user} can only sign in through the server's local Unix socket, not over the network. SQLLY connects over the network (TCP), so the server refuses this account whatever password you enter. This is the default for root and for your operating-system user on Homebrew, package and installer setups of MariaDB and MySQL. To connect, give the account a password in the mysql or mariadb command-line client, for example ALTER USER '{user}'@'localhost' IDENTIFIED BY 'a-new-password'; or create another user that has a password, then enter that password here. Server detail: {detail}",
);
const ACCESS_DENIED: (&str, &str) = (
    "services.mysql.connect_access_denied",
    "Authentication failed: the server rejected the user name or password for {user}. Check both for typos. MySQL and MariaDB accounts are tied to the address you connect from, so an account created for 'localhost' may not accept a connection from another address, and the same user name can have a different password for each address. Server detail: {detail}",
);
const ACCESS_DENIED_NO_PASSWORD: (&str, &str) = (
    "services.mysql.connect_access_denied_no_password",
    "Authentication failed: no password was sent for {user}, and the server requires one for this account. Edit the connection, enter the password, and try again. Server detail: {detail}",
);
const HOST_NOT_ALLOWED: (&str, &str) = (
    "services.mysql.connect_host_not_allowed",
    "Connection error: the server does not accept sign-ins from this computer's address. No account on the server is set up to connect from here; accounts are often created for 'localhost' only. On the server, create or change an account so its host part matches this computer (for example 'user'@'%'), or connect through an SSH tunnel. Server detail: {detail}",
);
const UNKNOWN_DATABASE: (&str, &str) = (
    "services.mysql.connect_unknown_database",
    "Connection error: the database \"{database}\" does not exist on this server. The user name and password were accepted. Edit the connection and correct the Database field, or clear it to connect without a default database. Server detail: {detail}",
);
const DATABASE_ACCESS_DENIED: (&str, &str) = (
    "services.mysql.connect_database_access_denied",
    "Connection error: the account {user} signed in, but it is not allowed to use the database \"{database}\". Edit the connection and choose a database this account can use or clear the Database field, or grant the account access on the server. Server detail: {detail}",
);
const TOO_MANY_CONNECTIONS: (&str, &str) = (
    "services.mysql.connect_too_many_connections",
    "Connection error: {server} has reached its limit of open connections and is refusing new ones. Close connections that are no longer needed, or raise max_connections on the server, then try again. Server detail: {detail}",
);
const REFUSED: (&str, &str) = (
    "services.mysql.connect_refused",
    "Connection error: nothing is accepting connections at {server} on port {port}. Either the server is not running, it listens on a different port, or it is configured to accept only local socket connections (skip-networking) or connections on another network address (bind-address). Start the server and check the Server and Port fields. Driver detail: {detail}",
);
const HOST_NOT_FOUND: (&str, &str) = (
    "services.mysql.connect_host_not_found",
    "Connection error: the host name \"{server}\" could not be found. Check the Server field for typos, and check that your network or VPN connection is up. Driver detail: {detail}",
);
const TIMED_OUT: (&str, &str) = (
    "services.mysql.connect_timed_out",
    "Connection error: timed out connecting to {server} on port {port}; there was no complete reply within the {seconds}-second timeout. The address or port may be wrong, a firewall may be blocking the port, or the server may only be reachable through a VPN or SSH tunnel.",
);
const AUTH_PLUGIN_UNSUPPORTED: (&str, &str) = (
    "services.mysql.connect_auth_plugin_unsupported",
    "Authentication failed: the account {user} signs in with the \"{plugin}\" authentication plugin, which SQLLY does not support. On the server, change the account to mysql_native_password (or caching_sha2_password on MySQL), or connect with a different account.",
);

/// The keyed message for `template`, with each `{name}` filled from `args`.
fn message((key, template): (&'static str, &str), args: &[(&str, &str)]) -> DisplayMessage {
    let mut fallback = template.to_string();
    for (name, value) in args {
        fallback = fallback.replace(&format!("{{{name}}}"), value);
    }
    args.iter().fold(
        DisplayMessage::new(key, fallback),
        |message, (name, value)| message.with_arg(*name, *value),
    )
}

fn connection(template: (&'static str, &str), args: &[(&str, &str)]) -> StudioError {
    StudioError::ConnectionMessage(message(template, args))
}

fn authentication(template: (&'static str, &str), args: &[(&str, &str)]) -> StudioError {
    StudioError::AuthenticationMessage(message(template, args))
}

/// The error for a failed connect. `attempt` is `None` where no profile is
/// in reach; causes that need it then keep the driver's wording.
pub(super) fn explain(error: mysql_async::Error, attempt: Option<&ConnectAttempt>) -> StudioError {
    if matches!(
        error,
        mysql_async::Error::Driver(mysql_async::DriverError::CleartextPluginDisabled)
    ) {
        return StudioError::AuthenticationMessage(DisplayMessage::new(
            "services.mysql.cleartext_guidance",
            "This MySQL account signs in with a cleartext password (PAM or LDAP authentication). SQLLY only sends it over an encrypted connection with a verified server certificate: set TLS to Verify CA or Verify full in the connection editor.",
        ));
    }
    let detail = error.to_string();
    if let Some(attempt) = attempt
        && let Some(explained) = explain_known(&error, &detail, attempt)
    {
        return explained;
    }
    if detail.contains("Access denied") {
        StudioError::Authentication(detail)
    } else {
        crate::connect_hint::connection_with_local_network_hint(detail)
    }
}

fn explain_known(
    error: &mysql_async::Error,
    detail: &str,
    attempt: &ConnectAttempt,
) -> Option<StudioError> {
    use mysql_async::{DriverError, Error, IoError};
    let server = ("server", attempt.server.as_str());
    let user = ("user", attempt.user.as_str());
    let database = ("database", attempt.database.as_str());
    let detail_arg = ("detail", detail);
    Some(match error {
        Error::Server(server_error) => match server_error.code {
            // ER_ACCESS_DENIED_NO_PASSWORD_ERROR: unix_socket / auth_socket
            // accounts answer a TCP login with this, never with 1045.
            1698 => authentication(SOCKET_ONLY_ACCOUNT, &[user, detail_arg]),
            1045 if server_error.message.contains("using password: NO") => {
                authentication(ACCESS_DENIED_NO_PASSWORD, &[user, detail_arg])
            }
            1045 => authentication(ACCESS_DENIED, &[user, detail_arg]),
            1130 => connection(HOST_NOT_ALLOWED, &[detail_arg]),
            1049 => connection(UNKNOWN_DATABASE, &[database, detail_arg]),
            1044 => connection(DATABASE_ACCESS_DENIED, &[user, database, detail_arg]),
            1040 => connection(TOO_MANY_CONNECTIONS, &[server, detail_arg]),
            _ => return None,
        },
        Error::Driver(DriverError::NoClientSslFlagFromServer) => {
            connection(TLS_NOT_ENABLED_ON_SERVER, &[server, detail_arg])
        }
        Error::Driver(DriverError::UnknownAuthPlugin { name }) => {
            unsupported_auth_plugin(name, attempt)
        }
        Error::Io(IoError::Tls(_)) => connection(tls_template(detail), &[server, detail_arg]),
        Error::Io(IoError::Io(io)) => {
            // rustls hands a rejected handshake to the driver as a plain
            // I/O error, so a certificate failure arrives here, not as
            // `IoError::Tls`.
            if is_tls_failure(detail) {
                connection(tls_template(detail), &[server, detail_arg])
            } else if io.kind() == std::io::ErrorKind::ConnectionRefused {
                let port = attempt.port.to_string();
                connection(REFUSED, &[server, ("port", &port), detail_arg])
            } else if is_name_lookup_failure(detail) {
                connection(HOST_NOT_FOUND, &[server, detail_arg])
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

/// Which certificate problem a TLS failure is. rustls renders some
/// certificate errors as a sentence and some as the variant name, depending
/// on its version, so both spellings are matched.
fn tls_template(detail: &str) -> (&'static str, &'static str) {
    let text = detail.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));
    if has(&[
        "unknownissuer",
        "unknown issuer",
        "causedasendentity",
        "self signed",
        "self-signed",
    ]) {
        TLS_UNTRUSTED_CERTIFICATE
    } else if has(&["notvalidforname", "not valid for name"]) {
        TLS_HOSTNAME_MISMATCH
    } else if has(&["expired", "notvalidyet", "not valid yet"]) {
        TLS_CERTIFICATE_EXPIRED
    } else {
        TLS_FAILED
    }
}

/// Whether an I/O error's text is rustls rejecting the handshake.
fn is_tls_failure(detail: &str) -> bool {
    let text = detail.to_ascii_lowercase();
    ["peer certificate", "fatal alert", "peer is incompatible"]
        .iter()
        .any(|needle| text.contains(needle))
}

/// Whether a socket error is the resolver failing to find the host. The
/// resolver's error has no dedicated kind, only the platform's wording.
fn is_name_lookup_failure(detail: &str) -> bool {
    let text = detail.to_ascii_lowercase();
    [
        "failed to lookup address",
        "nodename nor servname",
        "name or service not known",
        "no such host is known",
        "temporary failure in name resolution",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

pub(super) fn timed_out(attempt: &ConnectAttempt, timeout: std::time::Duration) -> StudioError {
    let port = attempt.port.to_string();
    let seconds = timeout.as_secs().to_string();
    connection(
        TIMED_OUT,
        &[
            ("server", &attempt.server),
            ("port", &port),
            ("seconds", &seconds),
        ],
    )
}

fn unsupported_auth_plugin(plugin: &str, attempt: &ConnectAttempt) -> StudioError {
    authentication(
        AUTH_PLUGIN_UNSUPPORTED,
        &[("user", &attempt.user), ("plugin", plugin)],
    )
}

/// The error for a driver panic during the handshake. The driver panics
/// rather than erroring when the server asks for an authentication plugin it
/// was built without (`ed25519`, `parsec`); that is a sign-in the user can
/// fix on the server, not a crash. Any other panic is handed back.
pub(super) fn explain_panic(
    panic: Box<dyn std::any::Any + Send>,
    attempt: &ConnectAttempt,
) -> Result<StudioError, Box<dyn std::any::Any + Send>> {
    let text = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    match text.split_once("authentication plugin") {
        Some((before, _)) => {
            let plugin = before.rsplit('`').nth(1).unwrap_or("unknown");
            Ok(unsupported_auth_plugin(plugin, attempt))
        }
        None => Err(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt() -> ConnectAttempt {
        ConnectAttempt {
            server: "localhost".into(),
            port: 3306,
            user: "root".into(),
            database: "appdb".into(),
        }
    }

    fn server_error(code: u16, message: &str) -> mysql_async::Error {
        mysql_async::Error::Server(mysql_async::ServerError {
            code,
            message: message.into(),
            state: "28000".into(),
        })
    }

    fn explained(error: mysql_async::Error) -> (StudioError, DisplayMessage) {
        let error = explain(error, Some(&attempt()));
        let message = error.message();
        (error, message)
    }

    #[track_caller]
    fn assert_filled(message: &DisplayMessage) {
        let text = message.fallback();
        assert!(!text.contains('{') && !text.contains('}'), "{text}");
    }

    #[test]
    fn socket_only_account_says_it_cannot_sign_in_over_the_network() {
        let (error, message) = explained(server_error(
            1698,
            "Access denied for user 'root'@'192.168.65.1'",
        ));
        assert!(matches!(error, StudioError::AuthenticationMessage(_)));
        assert_eq!(
            message.key(),
            Some("services.mysql.connect_socket_only_account")
        );
        let text = message.fallback();
        assert!(
            text.starts_with("Authentication failed: the account root"),
            "{text}"
        );
        assert!(text.contains("ALTER USER 'root'@'localhost'"), "{text}");
        assert!(text.contains("ERROR 1698"), "{text}");
        assert_filled(&message);
    }

    #[test]
    fn wrong_password_and_missing_password_are_told_apart() {
        let (_, wrong) = explained(server_error(
            1045,
            "Access denied for user 'root'@'localhost' (using password: YES)",
        ));
        assert_eq!(wrong.key(), Some("services.mysql.connect_access_denied"));
        assert_filled(&wrong);

        let (error, missing) = explained(server_error(
            1045,
            "Access denied for user 'root'@'localhost' (using password: NO)",
        ));
        assert!(matches!(error, StudioError::AuthenticationMessage(_)));
        assert_eq!(
            missing.key(),
            Some("services.mysql.connect_access_denied_no_password")
        );
        assert_filled(&missing);
    }

    #[test]
    fn server_refusals_name_their_cause() {
        for (code, key) in [
            (1130, "services.mysql.connect_host_not_allowed"),
            (1049, "services.mysql.connect_unknown_database"),
            (1044, "services.mysql.connect_database_access_denied"),
            (1040, "services.mysql.connect_too_many_connections"),
        ] {
            let (error, message) = explained(server_error(code, "server said no"));
            assert!(matches!(error, StudioError::ConnectionMessage(_)), "{code}");
            assert_eq!(message.key(), Some(key));
            assert!(message.fallback().contains("server said no"), "{message}");
            assert_filled(&message);
        }
    }

    #[test]
    fn unknown_server_errors_keep_the_driver_wording() {
        let error = explain(server_error(1227, "Access denied; you need SUPER"), None);
        assert!(matches!(error, StudioError::Authentication(_)));
        let (error, _) = explained(server_error(1226, "User has exceeded max_user_connections"));
        assert!(matches!(error, StudioError::Connection(_)));
    }

    #[test]
    fn server_without_tls_points_at_the_disable_mode() {
        let (error, message) =
            explained(mysql_async::DriverError::NoClientSslFlagFromServer.into());
        assert!(matches!(error, StudioError::ConnectionMessage(_)));
        assert_eq!(
            message.key(),
            Some("services.mysql.connect_tls_not_enabled_on_server")
        );
        assert!(
            message.fallback().contains("set TLS to \"Disable\""),
            "{message}"
        );
        assert_filled(&message);
    }

    #[test]
    fn certificate_problems_are_told_apart() {
        for (detail, template) in [
            (
                "invalid peer certificate: UnknownIssuer",
                TLS_UNTRUSTED_CERTIFICATE,
            ),
            (
                "invalid peer certificate: CaUsedAsEndEntity",
                TLS_UNTRUSTED_CERTIFICATE,
            ),
            (
                "invalid peer certificate: NotValidForName",
                TLS_HOSTNAME_MISMATCH,
            ),
            (
                "invalid peer certificate: certificate not valid for name \"db\"; certificate is only valid for DnsName(\"x\")",
                TLS_HOSTNAME_MISMATCH,
            ),
            ("invalid peer certificate: Expired", TLS_CERTIFICATE_EXPIRED),
            (
                "invalid peer certificate: certificate expired: verification time 1 (UNIX), but certificate is not valid after 0",
                TLS_CERTIFICATE_EXPIRED,
            ),
            ("received fatal alert: HandshakeFailure", TLS_FAILED),
        ] {
            assert_eq!(tls_template(detail).0, template.0, "{detail}");
        }
    }

    #[test]
    fn self_signed_certificate_points_at_the_require_mode() {
        // What a MariaDB 11.4+ server with its generated certificate
        // produces under Verify full, as the driver reports it.
        let rejected = std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid peer certificate: UnknownIssuer",
        );
        let (error, message) = explained(rejected.into());
        assert!(matches!(error, StudioError::ConnectionMessage(_)));
        assert_eq!(
            message.key(),
            Some("services.mysql.connect_tls_untrusted_certificate")
        );
        let text = message.fallback();
        assert!(
            text.contains("certificate from localhost is not trusted"),
            "{text}"
        );
        assert!(
            text.contains("set TLS to \"Require (no verification)\""),
            "{text}"
        );
        assert!(text.contains("UnknownIssuer"), "{text}");
        assert_filled(&message);
    }

    #[test]
    fn refused_connection_names_host_and_port() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        let (error, message) = explained(refused.into());
        assert!(matches!(error, StudioError::ConnectionMessage(_)));
        assert_eq!(message.key(), Some("services.mysql.connect_refused"));
        assert!(
            message.fallback().contains("at localhost on port 3306"),
            "{message}"
        );
        assert_filled(&message);
    }

    #[test]
    fn unresolvable_host_names_the_server_field() {
        let lookup = std::io::Error::other(
            "failed to lookup address information: nodename nor servname provided, or not known",
        );
        let (_, message) = explained(lookup.into());
        assert_eq!(message.key(), Some("services.mysql.connect_host_not_found"));
        assert_filled(&message);
    }

    #[test]
    fn timeout_names_host_port_and_duration() {
        let error = timed_out(&attempt(), std::time::Duration::from_secs(15));
        let message = error.message();
        assert_eq!(message.key(), Some("services.mysql.connect_timed_out"));
        assert!(
            message
                .fallback()
                .contains("timed out connecting to localhost on port 3306"),
            "{message}"
        );
        assert!(
            message.fallback().contains("15-second timeout"),
            "{message}"
        );
        assert_filled(&message);
    }

    #[test]
    fn unsupported_plugin_panic_becomes_an_authentication_error() {
        let panic: Box<dyn std::any::Any + Send> = Box::new(String::from(
            "Can't create response for `ed25519` authentication plugin — `mysql_common/client_ed25519` feature is disabled.",
        ));
        let Ok(error) = explain_panic(panic, &attempt()) else {
            panic!("an unsupported plugin is a sign-in failure");
        };
        let message = error.message();
        assert!(matches!(error, StudioError::AuthenticationMessage(_)));
        assert_eq!(
            message.key(),
            Some("services.mysql.connect_auth_plugin_unsupported")
        );
        assert!(message.fallback().contains("\"ed25519\""), "{message}");
        assert_filled(&message);
    }

    #[test]
    fn unrelated_panic_is_handed_back() {
        let panic: Box<dyn std::any::Any + Send> = Box::new("index out of bounds");
        assert!(explain_panic(panic, &attempt()).is_err());
    }
}
