//! Profile → Oracle connect parameters, plus the one error every Oracle user
//! hits first: the Instant Client library is not installed.

use sqlly_core::{AuthenticationType, ConnectionProfile, StudioError, StudioResult};

/// Prefix in the Database field that selects a SID instead of a service
/// name (`SID:ORCL`). Case-insensitive.
pub const SID_PREFIX: &str = "SID:";

/// Environment variable naming the folder that holds the Instant Client
/// shared library, for machines where it cannot go on the default search
/// path. Read once, before the first connection.
pub const CLIENT_DIR_ENV: &str = "SQLLY_ORACLE_CLIENT_DIR";

/// Administrative session role parsed off the username (`sys AS SYSDBA`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Normal,
    Sysdba,
    Sysoper,
}

/// Everything the driver needs to dial: the username with any `AS SYSDBA`
/// suffix stripped, the role that suffix named, and the connect string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectParams {
    pub username: String,
    pub role: SessionRole,
    pub connect_string: String,
}

/// The connect string for a profile.
///
/// - A Server value that starts with `(` is a full connect descriptor and
///   is passed through verbatim (this is how TCPS / wallet connections are
///   expressed).
/// - A Server value containing `/` is already an EZConnect string
///   (`host:1521/service`) and is passed through verbatim.
/// - Otherwise the Database field supplies the service name
///   (`//host:port/service`), or with the `SID:` prefix a SID, rendered as a
///   descriptor because EZConnect has no SID form.
/// - With no Database at all the Server value is handed to the client as-is,
///   so a `tnsnames.ora` alias keeps working.
pub fn connect_string(profile: &ConnectionProfile) -> String {
    connect_string_at(profile, None)
}

/// Whether the profile dials a plain `host` + `port` + service/SID (the
/// only shape a tunnel can rewrite). Connect descriptors, EZConnect strings
/// and `tnsnames.ora` aliases carry their own address and pass through.
pub fn uses_plain_endpoint(profile: &ConnectionProfile) -> bool {
    let server = profile.server.trim();
    !(server.starts_with('(') || server.contains('/'))
        && profile
            .database
            .as_deref()
            .map(str::trim)
            .is_some_and(|d| !d.is_empty())
}

/// [`connect_string`] with the host and port replaced by `endpoint` when the
/// profile dials a plain endpoint — how an SSH tunnel, proxy hop or
/// Kubernetes port-forward reroutes the session through its local listener.
/// Descriptors, EZConnect strings and aliases ignore the override.
pub fn connect_string_at(profile: &ConnectionProfile, endpoint: Option<(&str, u16)>) -> String {
    let server = profile.server.trim();
    if server.starts_with('(') || server.contains('/') {
        return server.to_string();
    }
    let database = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let Some(database) = database else {
        return server.to_string();
    };
    let (server, port) = match endpoint {
        Some((host, port)) => (host, port),
        None => (server, profile.port),
    };
    let port = if port == 0 { 1521 } else { port };
    if let Some(sid) = strip_sid_prefix(database) {
        return format!(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST={server})(PORT={port}))(CONNECT_DATA=(SID={sid})))"
        );
    }
    format!("//{server}:{port}/{database}")
}

/// `SID:ORCL` → `Some("ORCL")`; anything else → `None`.
pub fn strip_sid_prefix(database: &str) -> Option<&str> {
    let trimmed = database.trim();
    if trimmed.len() > SID_PREFIX.len()
        && trimmed[..SID_PREFIX.len()].eq_ignore_ascii_case(SID_PREFIX)
    {
        let sid = trimmed[SID_PREFIX.len()..].trim();
        (!sid.is_empty()).then_some(sid)
    } else {
        None
    }
}

/// Split `scott AS SYSDBA` into the login name and the requested role.
pub fn parse_username(raw: &str) -> (String, SessionRole) {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    for (suffix, role) in [
        (" as sysdba", SessionRole::Sysdba),
        (" as sysoper", SessionRole::Sysoper),
    ] {
        if let Some(stripped) = lower.strip_suffix(suffix) {
            let name = trimmed[..stripped.len()].trim_end();
            return (name.to_string(), role);
        }
    }
    (trimmed.to_string(), SessionRole::Normal)
}

/// Resolve the login for a profile. Only SQL logins apply: Oracle has no
/// Entra, cloud-IAM, or Windows-integrated path in this driver.
pub fn connect_params(profile: &ConnectionProfile) -> StudioResult<(ConnectParams, String)> {
    connect_params_at(profile, None)
}

/// [`connect_params`] dialing `endpoint` instead of the profile's own host
/// and port (see [`connect_string_at`]).
pub fn connect_params_at(
    profile: &ConnectionProfile,
    endpoint: Option<(&str, u16)>,
) -> StudioResult<(ConnectParams, String)> {
    let (raw_username, password) = match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => (username.clone(), secret_ref.resolve()?),
        AuthenticationType::Integrated => {
            return Err(StudioError::NotImplemented(
                "Integrated authentication is not supported for Oracle connections",
            ));
        }
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser => {
            return Err(StudioError::InvalidProfile(
                "Microsoft Entra authentication applies to SQL Server, not Oracle Database".into(),
            ));
        }
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. } => {
            return Err(StudioError::InvalidProfile(
                "Cloud IAM sign-in is not available for Oracle Database connections".into(),
            ));
        }
        AuthenticationType::AuthToken { .. } => {
            return Err(StudioError::InvalidProfile(
                "Auth-token sign-in applies to libSQL / Turso, not Oracle Database".into(),
            ));
        }
    };
    if profile.server.trim().is_empty() {
        return Err(StudioError::InvalidProfile(
            "Oracle profile needs a server host (or a connect descriptor) in Server".into(),
        ));
    }
    let (username, role) = parse_username(&raw_username);
    if username.is_empty() {
        return Err(StudioError::InvalidProfile(
            "Oracle profile needs a username".into(),
        ));
    }
    Ok((
        ConnectParams {
            username,
            role,
            connect_string: connect_string_at(profile, endpoint),
        },
        password,
    ))
}

/// The client library file name on this platform.
pub fn client_library_name() -> &'static str {
    if cfg!(target_os = "macos") {
        "libclntsh.dylib"
    } else if cfg!(target_os = "windows") {
        "oci.dll"
    } else {
        "libclntsh.so"
    }
}

/// Where the client library is looked for on this platform, for the error
/// text.
pub fn client_search_paths() -> &'static str {
    if cfg!(target_os = "macos") {
        "~/lib, /usr/local/lib, or DYLD_LIBRARY_PATH"
    } else if cfg!(target_os = "windows") {
        "a folder on PATH"
    } else {
        "LD_LIBRARY_PATH or an ldconfig directory"
    }
}

/// ODPI-C's "cannot locate the Oracle Client library" code.
const DPI_CLIENT_NOT_FOUND: i32 = 1047;

/// Whether an error from the client means the Instant Client library could
/// not be loaded (as opposed to a listener, credential, or SQL problem).
pub fn is_client_library_missing(error: &oracle::Error) -> bool {
    error.dpi_code() == Some(DPI_CLIENT_NOT_FOUND) || error.to_string().contains("DPI-1047")
}

/// The actionable message for a missing Instant Client, with the platform's
/// library name and search paths and the client's own reason appended.
pub fn client_library_missing_message(detail: &str) -> String {
    let mut message = format!(
        "Oracle Instant Client not found — SQLLY loads Oracle's client library ({lib}) when it connects. \
         Install Oracle Instant Client (Basic or Basic Light) from \
         https://www.oracle.com/database/technologies/instant-client/downloads.html, \
         put {lib} where it can be found ({paths}) or set {env} to its folder, and restart SQLLY.",
        lib = client_library_name(),
        paths = client_search_paths(),
        env = CLIENT_DIR_ENV,
    );
    let detail = detail.trim();
    if !detail.is_empty() {
        message.push_str(" (");
        message.push_str(detail);
        message.push(')');
    }
    message
}

pub fn client_library_missing_display_message(detail: &str) -> sqlly_core::DisplayMessage {
    let key = if detail.trim().is_empty() {
        "services.oracle.client_missing"
    } else {
        "services.oracle.client_missing_detail"
    };
    let message = sqlly_core::DisplayMessage::new(key, client_library_missing_message(detail))
        .with_arg("library_name", client_library_name())
        .with_arg("search_paths", client_search_paths())
        .with_arg("environment_variable", CLIENT_DIR_ENV);
    if detail.trim().is_empty() {
        message
    } else {
        message.with_arg("detail", detail.trim())
    }
}

/// Map a client error raised while connecting onto the studio's error
/// vocabulary: a missing library gets the install instructions, bad
/// credentials (ORA-01017) become an authentication failure, everything else
/// is a connection error carrying Oracle's own text.
pub fn to_connection_error(error: oracle::Error) -> StudioError {
    if is_client_library_missing(&error) {
        return StudioError::ConnectionMessage(client_library_missing_display_message(
            &error.to_string(),
        ));
    }
    let text = error.to_string();
    if error.oci_code() == Some(1017) || text.contains("ORA-01017") {
        StudioError::Authentication(text)
    } else {
        StudioError::Connection(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{DatabaseType, SecretProvider, SecretRef};

    fn profile(server: &str, port: u16, database: Option<&str>, user: &str) -> ConnectionProfile {
        let mut p = ConnectionProfile::sql_login(
            "oracle",
            server,
            user,
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        p.port = port;
        p.database = database.map(str::to_string);
        p.database_type = DatabaseType::Oracle;
        p
    }

    #[test]
    fn service_name_renders_ezconnect() {
        let p = profile("db.example.com", 1521, Some("FREEPDB1"), "app");
        assert_eq!(connect_string(&p), "//db.example.com:1521/FREEPDB1");
    }

    #[test]
    fn zero_port_falls_back_to_1521() {
        let p = profile("host", 0, Some("orcl"), "app");
        assert_eq!(connect_string(&p), "//host:1521/orcl");
    }

    #[test]
    fn sid_prefix_renders_a_descriptor() {
        let p = profile("host", 1522, Some("sid:ORCL"), "app");
        assert_eq!(
            connect_string(&p),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=host)(PORT=1522))(CONNECT_DATA=(SID=ORCL)))"
        );
        assert_eq!(strip_sid_prefix("SID: ORCL "), Some("ORCL"));
        assert_eq!(strip_sid_prefix("SID:"), None);
        assert_eq!(strip_sid_prefix("FREEPDB1"), None);
    }

    #[test]
    fn descriptor_and_ezconnect_servers_pass_through() {
        let p = profile(
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=h)(PORT=2484))(CONNECT_DATA=(SERVICE_NAME=s)))",
            1521,
            Some("ignored"),
            "app",
        );
        assert!(connect_string(&p).starts_with("(DESCRIPTION="));
        let p = profile("host:1521/svc", 1521, None, "app");
        assert_eq!(connect_string(&p), "host:1521/svc");
        // A tnsnames alias: no database, no slash, no descriptor.
        let p = profile("PRODDB", 1521, None, "app");
        assert_eq!(connect_string(&p), "PRODDB");
    }

    /// A tunnel rewrites only the plain host + port shape; descriptors,
    /// EZConnect strings and aliases carry their own address and stay put.
    #[test]
    fn tunnel_endpoint_rewrites_only_plain_host_and_port() {
        let p = profile("db.example.com", 1521, Some("FREEPDB1"), "app");
        assert!(uses_plain_endpoint(&p));
        assert_eq!(
            connect_string_at(&p, Some(("127.0.0.1", 40123))),
            "//127.0.0.1:40123/FREEPDB1"
        );
        let p = profile("db.example.com", 1521, Some("SID:ORCL"), "app");
        assert_eq!(
            connect_string_at(&p, Some(("127.0.0.1", 40123))),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=127.0.0.1)(PORT=40123))(CONNECT_DATA=(SID=ORCL)))"
        );
        let p = profile("host:1521/svc", 1521, None, "app");
        assert!(!uses_plain_endpoint(&p));
        assert_eq!(
            connect_string_at(&p, Some(("127.0.0.1", 40123))),
            "host:1521/svc"
        );
        let p = profile("(DESCRIPTION=(ADDRESS=(HOST=h)))", 1521, Some("x"), "app");
        assert!(!uses_plain_endpoint(&p));
        assert!(connect_string_at(&p, Some(("127.0.0.1", 1))).starts_with("(DESCRIPTION="));
        let p = profile("PRODDB", 1521, None, "app");
        assert!(!uses_plain_endpoint(&p));
        assert_eq!(connect_string_at(&p, Some(("127.0.0.1", 1))), "PRODDB");
        let (params, _) = connect_params_at(
            &profile("db.example.com", 1521, Some("FREEPDB1"), "app"),
            Some(("localhost", 9)),
        )
        .unwrap();
        assert_eq!(params.connect_string, "//localhost:9/FREEPDB1");
    }

    #[test]
    fn username_suffix_selects_the_role() {
        assert_eq!(
            parse_username("sys AS SYSDBA"),
            ("sys".to_string(), SessionRole::Sysdba)
        );
        assert_eq!(
            parse_username("SYS as sysoper "),
            ("SYS".to_string(), SessionRole::Sysoper)
        );
        assert_eq!(
            parse_username("scott"),
            ("scott".to_string(), SessionRole::Normal)
        );
    }

    #[test]
    fn connect_params_reject_non_sql_logins_and_blank_servers() {
        let mut p = profile("host", 1521, Some("svc"), "sys as sysdba");
        let (params, password) = connect_params(&p).unwrap();
        assert_eq!(params.username, "sys");
        assert_eq!(params.role, SessionRole::Sysdba);
        assert_eq!(params.connect_string, "//host:1521/svc");
        assert_eq!(password, "pw");

        p.authentication = AuthenticationType::Integrated;
        assert!(matches!(
            connect_params(&p).unwrap_err(),
            StudioError::NotImplemented(_)
        ));
        let blank = profile("   ", 1521, Some("svc"), "app");
        assert!(matches!(
            connect_params(&blank).unwrap_err(),
            StudioError::InvalidProfile(_)
        ));
    }

    #[test]
    fn missing_client_message_names_library_paths_and_env() {
        let message = client_library_missing_message(
            "DPI-1047: Cannot locate a 64-bit Oracle Client library",
        );
        assert!(message.starts_with("Oracle Instant Client not found"));
        assert!(message.contains(client_library_name()));
        assert!(message.contains(CLIENT_DIR_ENV));
        assert!(message.contains("restart SQLLY"));
        assert!(message.ends_with("(DPI-1047: Cannot locate a 64-bit Oracle Client library)"));
        assert!(client_library_missing_message("").ends_with("restart SQLLY."));
    }

    #[test]
    fn dpi_1047_maps_to_the_install_instructions() {
        let error = oracle::Error::new(
            oracle::ErrorKind::DpiError,
            "DPI-1047: Cannot locate a 64-bit Oracle Client library: \"libclntsh.dylib\"",
        );
        assert!(is_client_library_missing(&error));
        match to_connection_error(error) {
            StudioError::ConnectionMessage(message) => {
                assert!(
                    message
                        .fallback()
                        .starts_with("Oracle Instant Client not found")
                );
                assert_eq!(message.key(), Some("services.oracle.client_missing_detail"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let listener =
            oracle::Error::new(oracle::ErrorKind::OciError, "ORA-12541: TNS:no listener");
        assert!(!is_client_library_missing(&listener));
        assert!(matches!(
            to_connection_error(listener),
            StudioError::Connection(text) if text.contains("ORA-12541")
        ));
        let denied = oracle::Error::new(
            oracle::ErrorKind::OciError,
            "ORA-01017: invalid username/password; logon denied",
        );
        assert!(matches!(
            to_connection_error(denied),
            StudioError::Authentication(_)
        ));
    }
}
