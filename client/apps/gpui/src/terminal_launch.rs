//! Open in Terminal: launch the engine's native CLI (psql, mysql, sqlcmd,
//! redis-cli, clickhouse-client, duckdb, sqlite3, sqlplus/SQLcl) in a terminal
//! window with the connection's settings applied.
//!
//! The launch spec is built in two halves so the secret-handling stays
//! auditable: `argv` is always secret-free (it shows in `ps`), credentials
//! travel in the spawned process's environment (`PGPASSWORD`, `MYSQL_PWD`,
//! `SQLCMDPASSWORD`, `REDISCLI_AUTH`, `CLICKHOUSE_PASSWORD`) where the CLI
//! supports one, and engines without an env route (Oracle) prompt inside the
//! terminal instead. [`CliLaunch::display_command`] and the `Debug` impls
//! redact secret env values, and a test proves no secret ever reaches those
//! logged forms.
//!
//! Untrusted values (host, user, and database names) never pass through a
//! shell's parser: Linux hands them to `sh -c` as positional parameters,
//! macOS single-quotes them into a `bash` script, and Windows writes a
//! `.cmd` launcher whose command line is caret-escaped for `cmd.exe` and
//! MSVC-quoted for the CLI (see [`windows_launcher_script`]).
//!
//! Platform spawning is a capability: the pure builders run everywhere
//! (tests included); only the actual spawn is OS-gated.

#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use sqlly_core::ConnectionProfile;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::connection::TlsMode;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_core::{AuthenticationType, SecretProvider};
#[cfg(not(target_arch = "wasm32"))]
use sqlly_lang::sqlmodel::ConnectionKind;
use sqlly_lang::sqlmodel::DatabaseType;
#[cfg(not(target_arch = "wasm32"))]
use sqlly_services::NativeTool;

/// How long the macOS launcher script may wait for Terminal.app before SQLLY
/// deletes it (the script deletes itself first thing when it runs; this only
/// covers a Terminal that never runs it). Generous, because a cold Terminal
/// launch on a busy machine can take far longer than a few seconds.
#[cfg(not(target_arch = "wasm32"))]
pub const LAUNCHER_SWEEP_MINUTES: u64 = 5;

#[cfg(not(target_arch = "wasm32"))]
/// One environment variable for the spawned CLI. `secret` marks values that
/// must never appear in argv, logs, or the display form.
#[derive(Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub key: String,
    pub value: String,
    pub secret: bool,
}

#[cfg(not(target_arch = "wasm32"))]
impl EnvVar {
    fn secret(key: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
            secret: true,
        }
    }

    fn plain(key: &str, value: &str) -> Self {
        Self {
            key: key.to_string(),
            value: value.to_string(),
            secret: false,
        }
    }

    fn shown(&self) -> String {
        if self.secret {
            format!("{}=<redacted>", self.key)
        } else {
            format!("{}={}", self.key, self.value)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for EnvVar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.shown())
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// A fully built CLI invocation: secret-free argv, env carrying the
/// credentials, and a display form safe to log or show.
#[derive(Clone, PartialEq, Eq)]
pub struct CliLaunch {
    pub tool: NativeTool,
    pub binary: PathBuf,
    /// Arguments only (binary is separate) — never contains a secret.
    pub argv: Vec<String>,
    pub env: Vec<EnvVar>,
    /// Plain-language caveats the user should see after the launch (a port
    /// mapped to the CLI's protocol, a TLS check the CLI cannot make through
    /// a tunnel). Empty when the CLI gets the connection exactly as SQLLY
    /// uses it. Built on the runtime, so they are keyed messages the UI
    /// thread words with [`crate::localization::message`].
    pub notes: Vec<sqlly_core::DisplayMessage>,
}

#[cfg(not(target_arch = "wasm32"))]
impl CliLaunch {
    /// The command as it is safe to display or log: secret env values appear
    /// as `<redacted>`, everything else verbatim.
    #[cfg(test)]
    pub fn display_command(&self) -> String {
        let mut parts: Vec<String> = self.env.iter().map(EnvVar::shown).collect();
        parts.push(crate::external_editor::shell_quote(
            &self.binary.display().to_string(),
        ));
        parts.extend(
            self.argv
                .iter()
                .map(|arg| crate::external_editor::shell_quote(arg)),
        );
        parts.join(" ")
    }

    /// Every secret value this launch carries (for the redaction test).
    #[cfg(test)]
    fn secret_values(&self) -> Vec<&str> {
        self.env
            .iter()
            .filter(|e| e.secret && !e.value.is_empty())
            .map(|e| e.value.as_str())
            .collect()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for CliLaunch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CliLaunch")
            .field("tool", &self.tool)
            .field("binary", &self.binary)
            .field("argv", &self.argv)
            .field("env", &self.env)
            .field("notes", &self.notes)
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Whether a connection can open a terminal, and the honest reason when it
/// cannot — the menu renders the reason as a disabled item (the Engine-Aware
/// Action Availability pattern).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAvailability {
    /// The engine has a CLI route and the binary was found.
    Ready(NativeTool),
    /// The engine has a CLI route but the binary is not installed/discovered;
    /// [`terminal_install_key`] words what to install.
    Missing { tool: NativeTool },
    /// No honest route: in-memory file engine, Entra-only auth, libSQL over
    /// HTTP, a relay-routed connection, or an engine with no CLI mapping yet.
    Unsupported(TerminalBlocker),
}

#[cfg(not(target_arch = "wasm32"))]
/// Why a connection has no terminal route; [`terminal_blocker_key`] words it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalBlocker {
    /// The CLI dials the database directly, which a relay profile cannot do
    /// from this machine.
    RelayRoute,
    EntraSignIn,
    DuckDbInMemory,
    SqliteInMemory,
    LibSqlHttp,
    UnsupportedEngine,
}

#[cfg(not(target_arch = "wasm32"))]
/// Catalog key of the disabled menu item / status error for `blocker`.
pub fn terminal_blocker_key(blocker: TerminalBlocker) -> &'static str {
    match blocker {
        TerminalBlocker::RelayRoute => "shell.terminal_launch.unsupported_relay_route",
        TerminalBlocker::EntraSignIn => "shell.terminal_launch.unsupported_entra_sign_in",
        TerminalBlocker::DuckDbInMemory => "shell.terminal_launch.unsupported_duckdb_in_memory",
        TerminalBlocker::SqliteInMemory => "shell.terminal_launch.unsupported_sqlite_in_memory",
        TerminalBlocker::LibSqlHttp => "shell.terminal_launch.unsupported_libsql_http",
        TerminalBlocker::UnsupportedEngine => "shell.terminal_launch.unsupported_engine",
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Catalog key naming what to install when `tool` is missing.
pub fn terminal_install_key(tool: NativeTool) -> &'static str {
    match tool {
        NativeTool::Psql => "shell.terminal_launch.install_psql",
        NativeTool::Mysql => "shell.terminal_launch.install_mysql",
        NativeTool::Sqlcmd => "shell.terminal_launch.install_sqlcmd",
        NativeTool::RedisCli => "shell.terminal_launch.install_redis_cli",
        NativeTool::ClickHouseClient => "shell.terminal_launch.install_clickhouse_client",
        NativeTool::DuckDb => "shell.terminal_launch.install_duckdb",
        NativeTool::Sqlite3 => "shell.terminal_launch.install_sqlite3",
        NativeTool::Sqlplus | NativeTool::Sqlcl => "shell.terminal_launch.install_oracle",
        // Dump tools never back a terminal route.
        NativeTool::PgDump | NativeTool::PgRestore | NativeTool::MysqlDump => {
            "shell.terminal_launch.unsupported_engine"
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// The file path a SQLite/DuckDB profile opens, the same resolution the
/// drivers use (`server`, else `database`); `:memory:` and blank are no file.
fn file_path_for(profile: &ConnectionProfile) -> Option<String> {
    let server = profile.server.trim();
    let path = if server.is_empty() {
        profile
            .database
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    } else {
        Some(server)
    }?;
    (path != ":memory:").then(|| path.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
/// Whether this profile authenticates in a way a terminal CLI can take over.
fn entra_only(profile: &ConnectionProfile) -> bool {
    matches!(
        profile.authentication,
        AuthenticationType::EntraDeviceCode | AuthenticationType::EntraBrowser
    )
}

#[cfg(not(target_arch = "wasm32"))]
fn via_relay(profile: &ConnectionProfile) -> bool {
    matches!(
        profile.route,
        sqlly_core::connection::ConnectionRoute::ViaRelay { .. }
    )
}

#[cfg(not(target_arch = "wasm32"))]
/// Which CLI a connection would launch, or the reason it cannot. `found`
/// answers "is this tool installed" so the function stays pure (the menu
/// passes real discovery, tests pass a fake).
pub fn availability(
    db: DatabaseType,
    profile: &ConnectionProfile,
    found: &dyn Fn(NativeTool) -> bool,
) -> TerminalAvailability {
    // A relay profile's host is the bridge's view of the network; the CLI
    // would dial it from this machine, which is exactly what the relay route
    // exists to avoid.
    if via_relay(profile) {
        return TerminalAvailability::Unsupported(TerminalBlocker::RelayRoute);
    }
    let route = |tool: NativeTool| {
        if found(tool) {
            TerminalAvailability::Ready(tool)
        } else {
            TerminalAvailability::Missing { tool }
        }
    };
    match db.connection_kind() {
        ConnectionKind::PostgresWire => route(NativeTool::Psql),
        ConnectionKind::MySqlWire => route(NativeTool::Mysql),
        ConnectionKind::Tds => {
            if entra_only(profile) {
                return TerminalAvailability::Unsupported(TerminalBlocker::EntraSignIn);
            }
            route(NativeTool::Sqlcmd)
        }
        ConnectionKind::Resp => route(NativeTool::RedisCli),
        ConnectionKind::ClickHouseHttp => route(NativeTool::ClickHouseClient),
        ConnectionKind::DuckDbFile => {
            if file_path_for(profile).is_none() {
                return TerminalAvailability::Unsupported(TerminalBlocker::DuckDbInMemory);
            }
            route(NativeTool::DuckDb)
        }
        ConnectionKind::SqliteFile => {
            if file_path_for(profile).is_none() {
                return TerminalAvailability::Unsupported(TerminalBlocker::SqliteInMemory);
            }
            route(NativeTool::Sqlite3)
        }
        ConnectionKind::HranaHttp => TerminalAvailability::Unsupported(TerminalBlocker::LibSqlHttp),
        ConnectionKind::OracleNet => {
            if found(NativeTool::Sqlplus) {
                TerminalAvailability::Ready(NativeTool::Sqlplus)
            } else if found(NativeTool::Sqlcl) {
                TerminalAvailability::Ready(NativeTool::Sqlcl)
            } else {
                TerminalAvailability::Missing {
                    tool: NativeTool::Sqlplus,
                }
            }
        }
        _ => TerminalAvailability::Unsupported(TerminalBlocker::UnsupportedEngine),
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// The (username, password) a hydrated profile hands to a CLI. Passwords are
/// only taken from in-memory (already hydrated) secret refs — a profile that
/// still points at the keyring yields no password and the CLI will prompt.
/// Cloud IAM tokens are minted at connect time, so they ride along only when
/// the profile already carries one; otherwise the CLI prompts.
fn credentials(profile: &ConnectionProfile) -> (Option<String>, Option<String>) {
    let resolved = |secret_ref: &sqlly_core::SecretRef| {
        (secret_ref.provider == SecretProvider::InMemory && !secret_ref.key.is_empty())
            .then(|| secret_ref.key.clone())
    };
    match &profile.authentication {
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => (Some(username.clone()), resolved(secret_ref)),
        AuthenticationType::AwsRdsIam { username, .. }
        | AuthenticationType::GcpCloudSqlIam { username } => (
            Some(username.clone()),
            profile.runtime_cloud_iam_token.clone(),
        ),
        AuthenticationType::AuthToken { secret_ref } => (None, resolved(secret_ref)),
        AuthenticationType::Integrated
        | AuthenticationType::EntraDeviceCode
        | AuthenticationType::EntraBrowser => (None, None),
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// Facts about this machine the launch depends on, probed once by
/// [`HostFacts::probe`] and faked in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostFacts {
    /// The OS CA bundle (PEM), for CLIs that cannot fall back to the system
    /// trust store on their own (the MySQL client needs an explicit
    /// `--ssl-ca` to verify at all).
    pub system_ca_bundle: Option<PathBuf>,
    /// Which sqlcmd is installed (go-sqlcmd or ODBC, and its version), from
    /// its `-?` banner; `None` when unknown. Decides the `-N` spelling and
    /// whether `-F` (host name in certificate) exists.
    pub sqlcmd_flavor: Option<sqlly_services::SqlcmdFlavor>,
    /// Tunneled ClickHouse only: the local port of the extra forward to the
    /// server's native port that [`prepare_launch`] opened on the
    /// connection's tunnel (see `TunnelManager::endpoint_for_port`).
    pub clickhouse_native_forward: Option<u16>,
}

#[cfg(not(target_arch = "wasm32"))]
impl HostFacts {
    /// The well-known CA bundle locations (macOS/Alpine, Debian/Ubuntu,
    /// Fedora/RHEL, openSUSE), in probe order.
    const CA_BUNDLES: [&'static str; 5] = [
        "/etc/ssl/cert.pem",
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/pki/tls/certs/ca-bundle.crt",
        "/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem",
        "/etc/ssl/ca-bundle.pem",
    ];

    /// Filesystem probe; call off the UI thread.
    pub fn probe() -> Self {
        Self::probe_with(&|p: &Path| p.is_file())
    }

    fn probe_with(exists: &dyn Fn(&Path) -> bool) -> Self {
        let system_ca_bundle = if cfg!(windows) {
            None
        } else {
            Self::CA_BUNDLES
                .iter()
                .map(PathBuf::from)
                .find(|path| exists(path))
        };
        Self {
            system_ca_bundle,
            sqlcmd_flavor: None,
            clickhouse_native_forward: None,
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// clickhouse-client speaks ClickHouse's native TCP protocol, but a SQLLY
/// profile stores the HTTP port. When the profile has no explicit
/// `clickhouse_native_port`, map the conventional HTTP ports to their native
/// counterparts (8123 → 9000, 8443 → 9440); a profile whose port already is
/// a native one keeps it. Returns the port plus whether the mapping is an
/// assumption (a non-standard HTTP port).
fn clickhouse_native_port(http_port: u16, tls: bool) -> (u16, bool) {
    let native_default = if tls { 9440 } else { 9000 };
    match http_port {
        // 0 is "the driver's default", which is HTTP 8123.
        0 | 8123 => (native_default, false),
        8443 => (9440, false),
        9000 | 9440 => (http_port, false),
        _ => (native_default, true),
    }
}

/// sqlcmd's encryption switches for `tls`, spelled for the installed
/// flavor (verified against go-sqlcmd 1.10 `-?` and the Microsoft Learn
/// sqlcmd reference):
///
/// * go-sqlcmd 1.4+, ODBC sqlcmd 18+, and an unknown sqlcmd: the mode rides
///   attached to `-N` (`-Nm`, `-No`) — go-sqlcmd reads a detached value after
///   a bare `-N`, so `-N -C` would swallow `-C` as the mode; ODBC sqlcmd 18
///   defaults to mandatory encryption, so a plaintext profile says `-No`.
/// * go-sqlcmd before 1.4: `-N` takes only `true`/`false`/`disable`.
/// * ODBC sqlcmd before 18: `-N` is a bare switch and plaintext is the
///   default, so a plaintext profile passes nothing.
///
/// `-C` (trust the server certificate) is the same everywhere.
#[cfg(not(target_arch = "wasm32"))]
fn sqlcmd_encryption_args(
    flavor: Option<sqlly_services::SqlcmdFlavor>,
    tls: TlsMode,
) -> Vec<String> {
    use sqlly_services::SqlcmdFlavor;
    let (off, on): (Option<&str>, &str) = match flavor {
        Some(flavor) if flavor.takes_encryption_mode() => (Some("-No"), "-Nm"),
        None => (Some("-No"), "-Nm"),
        Some(SqlcmdFlavor::Go { .. }) => (Some("-Nfalse"), "-Ntrue"),
        Some(SqlcmdFlavor::Odbc { .. }) => (None, "-N"),
    };
    match tls {
        TlsMode::Disable => off.map(|flag| vec![flag.to_string()]).unwrap_or_default(),
        TlsMode::Require => vec![on.to_string(), "-C".to_string()],
        TlsMode::VerifyCa | TlsMode::VerifyFull => vec![on.to_string()],
        TlsMode::Legacy => unreachable!("effective_tls_mode never returns Legacy"),
    }
}

/// The native-protocol port clickhouse-client should reach for `profile`:
/// its explicit Native port, else the conventional mapping of its HTTP port.
/// Returns the port plus whether it is an assumption.
#[cfg(not(target_arch = "wasm32"))]
pub fn clickhouse_native_target(profile: &ConnectionProfile) -> (u16, bool) {
    match profile.clickhouse_native_port.filter(|port| *port != 0) {
        Some(port) => (port, false),
        None => clickhouse_native_port(
            profile.port,
            profile.effective_tls_mode() != TlsMode::Disable,
        ),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(not(target_arch = "wasm32"))]
/// Build the CLI invocation for a hydrated profile through `endpoint` (the
/// tunnel-aware `(host, port)` from `TunnelManager::endpoint_for`; a port of
/// 0 means the engine's default). The result never carries a secret in
/// `argv`. A tunneled endpoint (one that differs from the profile's own
/// host) keeps TLS host-name checks aimed at the real server where the CLI
/// allows it, and says so in `notes` where it cannot.
pub fn build_launch(
    profile: &ConnectionProfile,
    tool: NativeTool,
    binary: &Path,
    endpoint: (&str, u16),
    host_facts: &HostFacts,
) -> Result<CliLaunch, sqlly_core::DisplayMessage> {
    use crate::localization::{deferred, deferred_text};
    let db = profile.database_type;
    let (host, port_num) = endpoint;
    let real_host = profile.server.trim();
    let tunneled = !real_host.is_empty() && host != real_host;
    let port = port_num.to_string();
    let (username, password) = credentials(profile);
    let tls = profile.effective_tls_mode();
    let ca_file = profile.tls_ca_file.as_deref();
    let client_cert = profile.tls_client_cert_file.as_deref();
    let client_key = profile.tls_client_key_file.as_deref();
    let database = profile
        .database
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let mut argv: Vec<String> = Vec::new();
    let mut env: Vec<EnvVar> = Vec::new();
    let mut notes: Vec<sqlly_core::DisplayMessage> = Vec::new();
    if matches!(
        profile.authentication,
        AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. }
    ) && password.is_none()
    {
        notes.push(deferred(
            "shell.terminal_launch.note_cloud_iam_password_prompt",
            &[("tool", tool.binary_name())],
        ));
    }
    match tool {
        NativeTool::Psql => {
            // Through a tunnel, dial the local forward (PGHOSTADDR) while -h
            // keeps naming the real server, so verify-full checks the
            // certificate against the name it was issued for.
            let host_arg = if tunneled { real_host } else { host };
            argv.extend(["-h".into(), host_arg.to_string()]);
            if port_num != 0 {
                argv.extend(["-p".into(), port]);
            }
            if tunneled {
                env.push(EnvVar::plain("PGHOSTADDR", host));
            }
            if let Some(user) = username {
                argv.extend(["-U".into(), user]);
            }
            if let Some(db) = database {
                argv.extend(["-d".into(), db.to_string()]);
            }
            if let Some(password) = password {
                env.push(EnvVar::secret("PGPASSWORD", &password));
            }
            let sslmode = match tls {
                TlsMode::Disable => "disable",
                TlsMode::Require => "require",
                TlsMode::VerifyCa => "verify-ca",
                TlsMode::VerifyFull => "verify-full",
                TlsMode::Legacy => unreachable!("effective_tls_mode never returns Legacy"),
            };
            env.push(EnvVar::plain("PGSSLMODE", sslmode));
            if tls != TlsMode::Disable && profile.tls_direct {
                // libpq 17+; older psql rejects the unknown setting.
                env.push(EnvVar::plain("PGSSLNEGOTIATION", "direct"));
            }
            if tls != TlsMode::Disable {
                match ca_file {
                    Some(ca) => env.push(EnvVar::plain("PGSSLROOTCERT", &path_arg(ca))),
                    // SQLLY verifies against the system roots when no CA
                    // file is set; libpq 16+ does the same with "system"
                    // (older libpq would look for ~/.postgresql/root.crt).
                    None if matches!(tls, TlsMode::VerifyCa | TlsMode::VerifyFull) => {
                        env.push(EnvVar::plain("PGSSLROOTCERT", "system"))
                    }
                    None => {}
                }
                if let Some(cert) = client_cert {
                    env.push(EnvVar::plain("PGSSLCERT", &path_arg(cert)));
                }
                if let Some(key) = client_key {
                    env.push(EnvVar::plain("PGSSLKEY", &path_arg(key)));
                }
            }
        }
        NativeTool::Mysql => {
            argv.extend(["-h".into(), host.to_string()]);
            if port_num != 0 {
                argv.extend(["-P".into(), port]);
            }
            if let Some(user) = username {
                argv.extend(["-u".into(), user]);
            }
            let maria = db == DatabaseType::MariaDb;
            // Oracle's mysql client verifies only against an explicit CA
            // file, so fall back to the OS bundle; MariaDB's client reads
            // the system store itself.
            let ca = if maria {
                ca_file.map(Path::to_path_buf)
            } else {
                ca_file
                    .map(Path::to_path_buf)
                    .or_else(|| host_facts.system_ca_bundle.clone())
            };
            let mut tls = tls;
            if tunneled && tls == TlsMode::VerifyFull {
                // The certificate names the real server, not 127.0.0.1.
                // MariaDB's client has no chain-only mode, so it keeps TLS
                // but skips the check.
                tls = TlsMode::VerifyCa;
                notes.push(if maria {
                    deferred_text("shell.terminal_launch.note_mariadb_tunnel_unverified")
                } else {
                    deferred_text("shell.terminal_launch.note_mysql_tunnel_chain_only")
                });
            }
            if !maria && matches!(tls, TlsMode::VerifyCa | TlsMode::VerifyFull) && ca.is_none() {
                tls = TlsMode::Require;
                notes.push(deferred_text("shell.terminal_launch.note_mysql_missing_ca"));
            }
            match (tls, maria) {
                (TlsMode::Disable, _) => {}
                (TlsMode::Require, false) => argv.push("--ssl-mode=REQUIRED".into()),
                (TlsMode::VerifyCa, false) => argv.push("--ssl-mode=VERIFY_CA".into()),
                (TlsMode::VerifyFull, false) => argv.push("--ssl-mode=VERIFY_IDENTITY".into()),
                // MariaDB's client spells TLS differently: --ssl plus the
                // server-cert check for full verification.
                (TlsMode::VerifyFull, true) => {
                    argv.push("--ssl".into());
                    argv.push("--ssl-verify-server-cert".into());
                }
                (_, true) => argv.push("--ssl".into()),
                (TlsMode::Legacy, _) => unreachable!("effective_tls_mode never returns Legacy"),
            }
            if tls != TlsMode::Disable {
                if let Some(ca) = &ca {
                    argv.push(format!("--ssl-ca={}", path_arg(ca)));
                }
                if let Some(cert) = client_cert {
                    argv.push(format!("--ssl-cert={}", path_arg(cert)));
                }
                if let Some(key) = client_key {
                    argv.push(format!("--ssl-key={}", path_arg(key)));
                }
            }
            if let Some(password) = password {
                env.push(EnvVar::secret("MYSQL_PWD", &password));
            }
            if let Some(db) = database {
                argv.push(db.to_string());
            }
        }
        NativeTool::Sqlcmd => {
            let server = if port_num == 0 {
                format!("tcp:{host}")
            } else {
                format!("tcp:{host},{port}")
            };
            argv.extend(["-S".into(), server]);
            if matches!(profile.authentication, AuthenticationType::Integrated) {
                argv.push("-E".into());
            } else {
                if let Some(user) = username {
                    argv.extend(["-U".into(), user]);
                }
                if let Some(password) = password {
                    env.push(EnvVar::secret("SQLCMDPASSWORD", &password));
                }
            }
            if let Some(db) = database {
                argv.extend(["-d".into(), db.to_string()]);
            }
            argv.extend(sqlcmd_encryption_args(host_facts.sqlcmd_flavor, tls));
            if tunneled && matches!(tls, TlsMode::VerifyCa | TlsMode::VerifyFull) {
                if host_facts
                    .sqlcmd_flavor
                    .is_some_and(|flavor| flavor.takes_host_name_in_certificate())
                {
                    // Through the tunnel sqlcmd dials 127.0.0.1; -F makes it
                    // check the certificate against the real server name.
                    argv.extend(["-F".into(), real_host.to_string()]);
                } else {
                    notes.push(deferred_text(
                        "shell.terminal_launch.note_sqlcmd_tunnel_certificate",
                    ));
                }
            }
        }
        NativeTool::RedisCli => {
            // A unix-socket Redis profile keeps the socket path in `server`.
            if host.starts_with('/') {
                argv.extend(["-s".into(), host.to_string()]);
            } else {
                argv.extend(["-h".into(), host.to_string()]);
                if port_num != 0 {
                    argv.extend(["-p".into(), port]);
                }
            }
            if let Some(n) = database.filter(|n| n.parse::<i64>().is_ok()) {
                argv.extend(["-n".into(), n.to_string()]);
            }
            if let Some(user) = username {
                argv.extend(["--user".into(), user]);
            }
            if let Some(password) = password {
                env.push(EnvVar::secret("REDISCLI_AUTH", &password));
            }
            match tls {
                TlsMode::Disable => {}
                TlsMode::Require => argv.extend(["--tls".into(), "--insecure".into()]),
                TlsMode::VerifyCa | TlsMode::VerifyFull => argv.push("--tls".into()),
                TlsMode::Legacy => unreachable!("effective_tls_mode never returns Legacy"),
            }
            if tls != TlsMode::Disable {
                if tunneled {
                    // SNI names the real server, not the local forward.
                    argv.extend(["--sni".into(), real_host.to_string()]);
                }
                if let Some(ca) = ca_file {
                    argv.extend(["--cacert".into(), path_arg(ca)]);
                }
                if let Some(cert) = client_cert {
                    argv.extend(["--cert".into(), path_arg(cert)]);
                }
                if let Some(key) = client_key {
                    argv.extend(["--key".into(), path_arg(key)]);
                }
            }
        }
        NativeTool::ClickHouseClient => {
            let secure = tls != TlsMode::Disable;
            // A native port set on the connection wins; otherwise map the
            // profile's HTTP port to its conventional native counterpart.
            let configured = profile.clickhouse_native_port.filter(|port| *port != 0);
            let (native, assumed) = clickhouse_native_target(profile);
            let port_num = profile.port;
            // Through a tunnel the CLI dials a second forward that
            // `prepare_launch` opened to the native port; without one there
            // is no honest route.
            let (dial_host, dial_port) = if tunneled {
                let Some(forward) = host_facts.clickhouse_native_forward else {
                    return Err(deferred_text(
                        "shell.terminal_launch.clickhouse_native_forward_missing",
                    ));
                };
                notes.push(deferred(
                    "shell.terminal_launch.note_clickhouse_native_forward",
                    &[("native_port", native), ("forward_port", forward)],
                ));
                if matches!(tls, TlsMode::VerifyCa | TlsMode::VerifyFull) {
                    notes.push(deferred_text(
                        "shell.terminal_launch.note_clickhouse_tunnel_certificate",
                    ));
                }
                (host, forward)
            } else {
                (host, native)
            };
            if native != port_num {
                let http_port = if port_num == 0 { 8123 } else { port_num };
                notes.push(if assumed {
                    deferred(
                        "shell.terminal_launch.note_clickhouse_native_port_assumed",
                        &[("native_port", native), ("http_port", port_num)],
                    )
                } else if configured.is_some() {
                    deferred(
                        "shell.terminal_launch.note_clickhouse_native_port_configured",
                        &[("native_port", native), ("http_port", http_port)],
                    )
                } else {
                    deferred(
                        "shell.terminal_launch.note_clickhouse_native_port_default",
                        &[("native_port", native), ("http_port", http_port)],
                    )
                });
            }
            argv.extend([
                "--host".into(),
                dial_host.to_string(),
                "--port".into(),
                dial_port.to_string(),
            ]);
            if let Some(user) = username {
                argv.extend(["--user".into(), user]);
            }
            if let Some(db) = database {
                argv.extend(["--database".into(), db.to_string()]);
            }
            if secure {
                argv.push("--secure".into());
                if tls == TlsMode::Require {
                    argv.push("--accept-invalid-certificate".into());
                }
                if ca_file.is_some() || client_cert.is_some() {
                    notes.push(deferred_text(
                        "shell.terminal_launch.note_clickhouse_custom_certificates",
                    ));
                }
            }
            if let Some(password) = password {
                env.push(EnvVar::secret("CLICKHOUSE_PASSWORD", &password));
            }
        }
        NativeTool::DuckDb | NativeTool::Sqlite3 => {
            let path = file_path_for(profile)
                .ok_or_else(|| deferred_text("shell.terminal_launch.in_memory_no_file"))?;
            argv.push(path);
        }
        NativeTool::Sqlplus | NativeTool::Sqlcl => {
            let user = username
                .ok_or_else(|| deferred_text("shell.terminal_launch.oracle_username_missing"))?;
            let service = database.unwrap_or_default();
            let port = if port_num == 0 { 1521 } else { port_num };
            // No env route exists: SQL*Plus and SQLcl prompt for the password
            // inside the terminal, which is exactly where a secret belongs.
            argv.push(format!("{user}@//{host}:{port}/{service}"));
            if tool == NativeTool::Sqlplus {
                argv.insert(0, "-L".into());
            }
        }
        NativeTool::PgDump | NativeTool::PgRestore | NativeTool::MysqlDump => {
            return Err(deferred(
                "shell.terminal_launch.dump_tool_not_interactive",
                &[("tool", tool.label())],
            ));
        }
    }
    Ok(CliLaunch {
        tool,
        binary: binary.to_path_buf(),
        argv,
        env,
        notes,
    })
}

#[cfg(not(target_arch = "wasm32"))]
/// The terminal the host OS opens. Tests pick the kind explicitly; production
/// uses [`host_terminal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    MacOs,
    Linux,
    Windows,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn host_terminal() -> TerminalKind {
    if cfg!(target_os = "macos") {
        TerminalKind::MacOs
    } else if cfg!(windows) {
        TerminalKind::Windows
    } else {
        TerminalKind::Linux
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// A script written to a private temp file at spawn time (see
/// [`crate::fs_safe::private_temp_file`]) whose path then joins the command.
#[derive(Clone, PartialEq, Eq)]
pub struct Launcher {
    pub script: String,
    /// File suffix: `.command` (Terminal.app runs it) or `.cmd`.
    pub suffix: &'static str,
    /// Whether the script text itself holds a secret (macOS only).
    pub holds_secret: bool,
    /// Env var the spawned program reads the script path from (Windows,
    /// where the path must not pass through `cmd`'s parser); `None` appends
    /// the path to `args` instead.
    pub path_env: Option<&'static str>,
}

#[cfg(not(target_arch = "wasm32"))]
/// One spawn step: program + argv + env for the spawned process, plus an
/// optional launcher script.
#[derive(Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Windows only: the verbatim command line handed to `cmd.exe`, which
    /// parses its own quoting (Rust's per-argument escaping does not match
    /// cmd's rules, so the line is passed raw). It never holds a
    /// user-controlled value.
    pub raw_command_line: Option<String>,
    pub env: Vec<(String, String)>,
    pub launcher: Option<Launcher>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for TerminalCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Env values may be passwords and a macOS launcher embeds them, so
        // only the env keys and the script size are ever printed.
        let env_keys: Vec<&str> = self.env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("TerminalCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("raw_command_line", &self.raw_command_line)
            .field("env_keys", &env_keys)
            .field(
                "launcher",
                &self
                    .launcher
                    .as_ref()
                    .map(|l| format!("<{} bytes{}>", l.script.len(), l.suffix)),
            )
            .finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn env_pairs(launch: &CliLaunch) -> Vec<(String, String)> {
    launch
        .env
        .iter()
        .map(|e| (e.key.clone(), e.value.clone()))
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
/// The known Linux terminal emulators, each with the argument(s) that
/// separate its own options from the command it runs.
const LINUX_TERMINALS: [(&str, &[&str]); 8] = [
    ("x-terminal-emulator", &["-e"]),
    ("gnome-terminal", &["--"]),
    ("konsole", &["-e"]),
    ("xfce4-terminal", &["-x"]),
    ("kitty", &["--"]),
    ("alacritty", &["-e"]),
    ("wezterm", &["start", "--"]),
    ("xterm", &["-e"]),
];

#[cfg(not(target_arch = "wasm32"))]
/// The Linux terminal emulators tried in order, each with its command
/// separator. `$TERMINAL` (the user's stated preference) goes first when
/// set; one naming a known emulator (by path or name) takes that emulator's
/// separator instead of a blanket `-e`, and replaces its built-in entry.
fn linux_terminals(preferred: Option<&str>) -> Vec<(String, Vec<&'static str>)> {
    let basename = |program: &str| {
        Path::new(program)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(program)
            .to_string()
    };
    let mut list: Vec<(String, Vec<&'static str>)> = Vec::new();
    let mut preferred_name = None;
    if let Some(term) = preferred.map(str::trim).filter(|t| !t.is_empty()) {
        let name = basename(term);
        let flags = LINUX_TERMINALS
            .iter()
            .find(|(known, _)| *known == name)
            .map(|(_, flags)| flags.to_vec())
            .unwrap_or_else(|| vec!["-e"]);
        list.push((term.to_string(), flags));
        preferred_name = Some(name);
    }
    for (program, flags) in LINUX_TERMINALS {
        if preferred_name.as_deref() != Some(program) {
            list.push((program.to_string(), flags.to_vec()));
        }
    }
    list
}

/// Run by `sh -c` inside the Linux terminal: the CLI arrives as `$0 "$@"`
/// (positional parameters, never parsed as shell text), and the window stays
/// open after it exits so a connection error can be read.
#[cfg(not(target_arch = "wasm32"))]
const LINUX_KEEP_OPEN: &str = r#""$0" "$@"; status=$?; printf '\n[%s exited with status %s. Press Enter to close this window.]\n' "${0##*/}" "$status"; read -r _"#;

#[cfg(not(target_arch = "wasm32"))]
/// Every platform spawn plan for a launch, in the order to try them (Linux
/// has several emulators; macOS and Windows have one). Pure — nothing here
/// touches the filesystem or spawns a process.
pub fn terminal_commands(
    kind: TerminalKind,
    launch: &CliLaunch,
) -> Result<Vec<TerminalCommand>, sqlly_core::DisplayMessage> {
    match kind {
        TerminalKind::Linux => {
            let preferred = std::env::var("TERMINAL").ok();
            Ok(linux_commands(preferred.as_deref(), launch))
        }
        _ => terminal_command(kind, launch).map(|command| vec![command]),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn linux_commands(preferred: Option<&str>, launch: &CliLaunch) -> Vec<TerminalCommand> {
    let binary = launch.binary.display().to_string();
    linux_terminals(preferred)
        .into_iter()
        .map(|(program, flags)| TerminalCommand {
            program,
            args: flags
                .into_iter()
                .map(str::to_string)
                .chain(["sh".to_string(), "-c".to_string(), LINUX_KEEP_OPEN.into()])
                .chain(std::iter::once(binary.clone()))
                .chain(launch.argv.iter().cloned())
                .collect(),
            raw_command_line: None,
            // The emulator inherits our env and hands it to the CLI: the
            // secret never touches argv or disk.
            env: env_pairs(launch),
            launcher: None,
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
/// Quote one argument the way the MSVC runtime (and `CommandLineToArgvW`)
/// splits a command line: wrap in quotes, backslash-escape embedded quotes,
/// and double the backslashes that precede a quote or the closing quote.
fn msvc_quote(arg: &str) -> String {
    let mut out = String::from("\"");
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(not(target_arch = "wasm32"))]
/// Escape text for a line of a `.cmd` batch file so `cmd.exe` passes it
/// through literally: `%` doubles (percent expansion runs before carets are
/// read), and every other metacharacter — including `"`, so cmd never enters
/// quote mode — gets a caret.
fn batch_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for c in text.chars() {
        match c {
            '%' => out.push_str("%%"),
            '^' | '&' | '|' | '<' | '>' | '(' | ')' | '"' | '!' => {
                out.push('^');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
/// The `.cmd` launcher Windows runs in a new console: the CLI line (program
/// quoted, every argument MSVC-quoted then caret-escaped), then a pause so a
/// connection error stays readable, then self-deletion. Credentials are not
/// in it — they ride in the inherited environment.
fn windows_launcher_script(launch: &CliLaunch) -> Result<String, sqlly_core::DisplayMessage> {
    let binary = launch.binary.display().to_string();
    for value in std::iter::once(&binary).chain(launch.argv.iter()) {
        if value.contains(['\r', '\n', '\0']) {
            return Err(crate::localization::deferred_text(
                "shell.terminal_launch.windows_line_break",
            ));
        }
    }
    if binary.contains('"') {
        return Err(crate::localization::deferred(
            "shell.terminal_launch.invalid_program_path",
            &[("path", &binary)],
        ));
    }
    // The program token keeps real quotes (a path can't contain one) so a
    // space in "Program Files" doesn't split it; inside them only `%` is
    // still special in a batch file.
    let mut line = format!("\"{}\"", binary.replace('%', "%%"));
    for arg in &launch.argv {
        line.push(' ');
        line.push_str(&batch_escape(&msvc_quote(arg)));
    }
    Ok(format!(
        "@echo off\r\nchcp 65001 >nul\r\nsetlocal DisableDelayedExpansion\r\nset \"SQLLY_LAUNCHER=\"\r\n{line}\r\necho.\r\necho [SQLLY] {} exited. Press any key to close this window.\r\npause >nul\r\n(goto) 2>nul & del \"%~f0\"\r\n",
        launch.tool.binary_name()
    ))
}

#[cfg(not(target_arch = "wasm32"))]
/// The primary platform spawn plan for a launch (Linux: the first emulator
/// candidate). Pure.
pub fn terminal_command(
    kind: TerminalKind,
    launch: &CliLaunch,
) -> Result<TerminalCommand, sqlly_core::DisplayMessage> {
    let binary = launch.binary.display().to_string();
    match kind {
        TerminalKind::Linux => Ok(linux_commands(None, launch)
            .into_iter()
            .next()
            .expect("the Linux candidate list is never empty")),
        TerminalKind::Windows => {
            // `start` opens a new console window; its first quoted argument
            // is the window title. The launcher path arrives through
            // delayed expansion (/V:ON, `!SQLLY_LAUNCHER!`), which runs
            // after cmd has parsed the line, so nothing in the path — or in
            // any connection value, which lives only in the escaped script —
            // is ever read as a cmd metacharacter. The new console inherits
            // our env, so the password rides there, never on disk.
            Ok(TerminalCommand {
                program: "cmd".to_string(),
                args: Vec::new(),
                raw_command_line: Some(
                    "/D /V:ON /C start \"SQLLY\" \"!SQLLY_LAUNCHER!\"".to_string(),
                ),
                env: env_pairs(launch),
                launcher: Some(Launcher {
                    script: windows_launcher_script(launch)?,
                    suffix: ".cmd",
                    holds_secret: false,
                    path_env: Some("SQLLY_LAUNCHER"),
                }),
            })
        }
        TerminalKind::MacOs => {
            // Terminal.app inherits nothing from us, so env travels in an
            // owner-only (0700) script that deletes itself before exec'ing
            // the CLI: the secret then lives only in the CLI process's
            // environment, never in argv (visible in `ps`) and not on disk
            // once the CLI starts.
            let mut script = String::from("#!/bin/bash\nrm -f -- \"$0\"\n");
            for var in &launch.env {
                script.push_str(&format!(
                    "export {}={}\n",
                    var.key,
                    crate::external_editor::shell_quote(&var.value)
                ));
            }
            let exec = std::iter::once(binary)
                .chain(launch.argv.iter().cloned())
                .map(|arg| crate::external_editor::shell_quote(&arg))
                .collect::<Vec<_>>()
                .join(" ");
            script.push_str(&format!("exec {exec}\n"));
            // A `.command` file opened through LaunchServices runs in a new
            // Terminal window without Apple Events, so no Automation
            // permission prompt (which a hardened app without an Apple
            // Events usage string would have silently denied).
            Ok(TerminalCommand {
                program: "open".to_string(),
                args: vec!["-a".into(), "Terminal".into()],
                raw_command_line: None,
                env: Vec::new(),
                launcher: Some(Launcher {
                    holds_secret: launch.env.iter().any(|e| e.secret && !e.value.is_empty()),
                    script,
                    suffix: ".command",
                    path_env: None,
                }),
            })
        }
    }
}

/// Write a launcher script through the shared private-temp helper
/// (exclusive create in an owner-only `sqlly-terminal` dir), then make it
/// owner-executable (0700) where Terminal.app has to run it directly.
#[cfg(not(target_arch = "wasm32"))]
fn write_launcher(launcher: &Launcher) -> Result<PathBuf, sqlly_core::DisplayMessage> {
    use std::io::Write;
    let (mut file, path) = crate::fs_safe::private_temp_file("terminal", "launch", launcher.suffix)
        .map_err(|e| {
            crate::localization::deferred(
                "shell.terminal_launch.launcher_create_failed",
                &[("detail", e)],
            )
        })?;
    let written = file
        .write_all(launcher.script.as_bytes())
        .and_then(|()| file.sync_all());
    #[cfg(unix)]
    let written = written.and_then(|()| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&path);
        return Err(crate::localization::deferred(
            "shell.terminal_launch.launcher_write_failed",
            &[("detail", e)],
        ));
    }
    Ok(path)
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_one(command: &TerminalCommand, launcher_path: Option<&Path>) -> std::io::Result<()> {
    let mut cmd = std::process::Command::new(&command.program);
    cmd.args(&command.args);
    if let (Some(path), Some(launcher)) = (launcher_path, &command.launcher) {
        match launcher.path_env {
            Some(key) => {
                cmd.env(key, path);
            }
            None => {
                cmd.arg(path);
            }
        }
    }
    #[cfg(windows)]
    if let Some(line) = &command.raw_command_line {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(line);
    }
    for (key, value) in &command.env {
        cmd.env(key, value);
    }
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd.spawn().map(|_| ())
}

/// Spawn the launch in a new terminal window. Fire-and-forget: the terminal
/// outlives this call by design. Does file I/O — call off the UI thread.
#[cfg(not(target_arch = "wasm32"))]
pub fn spawn_terminal(launch: &CliLaunch) -> Result<(), sqlly_core::DisplayMessage> {
    if !crate::distribution::can_launch_user_programs() {
        return Err(crate::localization::deferred_text(
            "shell.terminal_launch.sandbox_blocked",
        ));
    }
    let commands = terminal_commands(host_terminal(), launch)?;
    let mut failures = Vec::new();
    for command in &commands {
        if let Some(launcher) = &command.launcher {
            let path = write_launcher(launcher)?;
            if let Err(e) = spawn_one(command, Some(&path)) {
                let _ = std::fs::remove_file(&path);
                return Err(crate::localization::deferred(
                    "shell.terminal_launch.terminal_start_failed",
                    &[("detail", e)],
                ));
            }
            // A secret-bearing script deletes itself as its first line; this
            // sweep covers a Terminal that never ran it (app closed, launch
            // denied), so the secret never lingers on disk. Scripts without
            // a secret delete themselves when their window closes.
            if launcher.holds_secret {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(LAUNCHER_SWEEP_MINUTES * 60));
                    let _ = std::fs::remove_file(path);
                });
            }
            return Ok(());
        }
        match spawn_one(command, None) {
            Ok(()) => return Ok(()),
            Err(e) => failures.push(format!("{}: {e}", command.program)),
        }
    }
    Err(crate::localization::deferred(
        "shell.terminal_launch.no_terminal_emulator",
        &[("detail", failures.join("; "))],
    ))
}

/// How long a fallback discovery probe (disk + PATH scan) is reused before
/// the next context-menu build probes again.
#[cfg(not(target_arch = "wasm32"))]
const FALLBACK_PROBE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(not(target_arch = "wasm32"))]
type ProbedPaths = std::collections::HashMap<NativeTool, PathBuf>;

#[cfg(not(target_arch = "wasm32"))]
static FALLBACK_PROBE: std::sync::Mutex<Option<(std::time::Instant, ProbedPaths)>> =
    std::sync::Mutex::new(None);

/// The discovered path for a CLI: the preferences cache first (it honors the
/// user's path overrides), else a PATH/well-known probe that is memoized for
/// [`FALLBACK_PROBE_TTL`] — the explorer context menu asks on every build, so
/// the disk and PATH scan must not run on the UI thread each time.
#[cfg(not(target_arch = "wasm32"))]
pub fn tool_path(tool: NativeTool) -> Option<PathBuf> {
    let locations = crate::preferences::native_tool_locations();
    if !locations.is_empty() {
        return sqlly_services::find_tool(&locations, tool).map(|loc| loc.path.clone());
    }
    let now = std::time::Instant::now();
    if let Ok(guard) = FALLBACK_PROBE.lock()
        && let Some((at, paths)) = guard.as_ref()
        && now.duration_since(*at) < FALLBACK_PROBE_TTL
    {
        return paths.get(&tool).cloned();
    }
    let prefs = crate::preferences::NativeToolPreferences::load();
    let path_var = std::env::var("PATH").ok();
    let resolved: ProbedPaths = sqlly_services::tool_discovery::resolve_paths(
        &prefs.override_map(),
        path_var.as_deref(),
        &sqlly_services::tool_discovery::well_known_dirs(),
        &|p| p.is_file(),
    )
    .into_iter()
    .map(|(tool, (path, _))| (tool, path))
    .collect();
    let found = resolved.get(&tool).cloned();
    if let Ok(mut guard) = FALLBACK_PROBE.lock() {
        *guard = Some((now, resolved));
    }
    found
}

/// Whether launching `launch` on `kind` briefly writes a secret to disk: only
/// macOS does, because Terminal.app inherits no environment and the password
/// has to ride in the self-deleting launcher script. The app explains this
/// once before the first such launch (see `PendingTerminalNotice`).
#[cfg(not(target_arch = "wasm32"))]
pub fn launch_writes_secret_file(kind: TerminalKind, launch: &CliLaunch) -> bool {
    kind == TerminalKind::MacOs && launch.env.iter().any(|e| e.secret && !e.value.is_empty())
}

/// Build the launch for `profile`'s native CLI without spawning anything, so
/// the caller can show the macOS notice first. The profile must already be
/// hydrated. Does discovery I/O and may open an SSH tunnel, so run it on the
/// tokio runtime off the UI thread; SSH-tunneled connections land on the
/// tunnel's local endpoint.
#[cfg(not(target_arch = "wasm32"))]
pub async fn prepare_launch(
    profile: ConnectionProfile,
) -> Result<CliLaunch, sqlly_core::DisplayMessage> {
    use crate::localization::{deferred, deferred_text};
    // Runs on the runtime, where the UI language is not visible: every
    // failure is a keyed message the UI thread words. The caller refuses
    // relay routes on the UI thread first; `availability` re-checks them.
    let availability = availability(profile.database_type, &profile, &|tool| {
        tool_path(tool).is_some()
    });
    let tool = match availability {
        TerminalAvailability::Ready(tool) => tool,
        TerminalAvailability::Missing { tool } => {
            return Err(deferred_text(terminal_install_key(tool)));
        }
        TerminalAvailability::Unsupported(blocker) => {
            return Err(deferred_text(terminal_blocker_key(blocker)));
        }
    };
    // DuckDB lets one process hold a file read-write; while SQLLY has it
    // open, the CLI (even with -readonly) fails to take its lock.
    if tool == NativeTool::DuckDb
        && let Some(path) = file_path_for(&profile)
        && sqlly_engines::duckdb::file_in_use(&path)
    {
        return Err(deferred_text("shell.terminal_launch.duckdb_file_in_use"));
    }
    let binary = tool_path(tool).ok_or_else(|| {
        deferred(
            "shell.terminal_launch.tool_not_found",
            &[("tool", tool.binary_name())],
        )
    })?;
    let (host, port) = sqlly_engines::ssh::TunnelManager::global()
        .endpoint_for(&profile)
        .await
        .map_err(|e| deferred("shell.terminal_launch.tunnel_failed", &[("detail", e)]))?;
    let mut host_facts = HostFacts::probe();
    if tool == NativeTool::Sqlcmd {
        host_facts.sqlcmd_flavor = sqlly_services::probe_sqlcmd_flavor(&binary).await;
    }
    // A tunneled ClickHouse profile forwards only its HTTP port; open a
    // second forward, through the same tunnel, to the native port the CLI
    // speaks. It is kept, like the connection's own forward, until the
    // connection is disconnected.
    if tool == NativeTool::ClickHouseClient && !profile.inline_transport().is_empty() {
        let (native, _) = clickhouse_native_target(&profile);
        let (_, forward) = sqlly_engines::ssh::TunnelManager::global()
            .endpoint_for_port(&profile, native)
            .await
            .map_err(|e| {
                deferred(
                    "shell.terminal_launch.clickhouse_native_forward_failed",
                    &[
                        ("native_port", native.to_string()),
                        ("detail", e.to_string()),
                    ],
                )
            })?;
        host_facts.clickhouse_native_forward = Some(forward);
    }
    build_launch(&profile, tool, &binary, (&host, port), &host_facts)
}

/// The menu entry for a connection node: label + enabled state. Unavailable
/// routes surface their reason as the disabled label.
pub fn menu_item(profile: Option<&ConnectionProfile>, db: DatabaseType) -> (String, bool) {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (profile, db);
        let key = if crate::browser_preview::served() {
            "explorer.menus.server_open_in_terminal_served_browser"
        } else {
            "explorer.menus.server_open_in_terminal_browser_preview"
        };
        return (crate::localization::text(key), false);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        if !crate::distribution::can_launch_user_programs() {
            let _ = (profile, db);
            return (
                crate::localization::text("explorer.menus.server_open_in_terminal_app_store"),
                false,
            );
        }
        let ready =
            profile.map(|profile| availability(db, profile, &|tool| tool_path(tool).is_some()));
        match ready {
            Some(TerminalAvailability::Missing { tool }) => {
                (crate::localization::text(terminal_install_key(tool)), false)
            }
            Some(TerminalAvailability::Unsupported(blocker)) => (
                crate::localization::text(terminal_blocker_key(blocker)),
                false,
            ),
            // No profile yet: the plain label, disabled.
            ready => (
                crate::localization::text("explorer.menus.server_open_in_terminal"),
                matches!(ready, Some(TerminalAvailability::Ready(_))),
            ),
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use sqlly_core::SecretRef;

    fn profile_for(db: DatabaseType) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            "test",
            "db.internal",
            "app_user",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "hunter2".to_string(),
            },
        );
        profile.database_type = db;
        profile.port = 0;
        profile
    }

    fn no_host() -> HostFacts {
        HostFacts::default()
    }

    fn found_none(_: NativeTool) -> bool {
        false
    }
    fn found_all(_: NativeTool) -> bool {
        true
    }

    fn env_value<'a>(launch: &'a CliLaunch, key: &str) -> Option<&'a str> {
        launch
            .env
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    fn psql_launch() -> CliLaunch {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.port = 5432;
        profile.database = Some("sales".into());
        profile.encrypt = false;
        build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/usr/bin/psql"),
            ("db.internal", 5432),
            &no_host(),
        )
        .unwrap()
    }

    #[test]
    fn only_macos_launches_with_a_password_need_the_disk_notice() {
        let launch = psql_launch();
        assert!(launch.env.iter().any(|e| e.secret && !e.value.is_empty()));
        assert!(launch_writes_secret_file(TerminalKind::MacOs, &launch));
        assert!(!launch_writes_secret_file(TerminalKind::Linux, &launch));
        assert!(!launch_writes_secret_file(TerminalKind::Windows, &launch));

        let mut no_password = launch.clone();
        no_password.env.retain(|e| !e.secret);
        assert!(!launch_writes_secret_file(
            TerminalKind::MacOs,
            &no_password
        ));
    }

    #[test]
    fn psql_argv_is_secret_free_and_env_carries_the_password() {
        let launch = psql_launch();
        assert_eq!(
            launch.argv,
            vec![
                "-h",
                "db.internal",
                "-p",
                "5432",
                "-U",
                "app_user",
                "-d",
                "sales"
            ]
        );
        assert!(
            !launch.argv.iter().any(|a| a.contains("hunter2")),
            "no secret on argv: {:?}",
            launch.argv
        );
        assert!(
            launch
                .env
                .iter()
                .any(|e| e.key == "PGPASSWORD" && e.value == "hunter2" && e.secret)
        );
        assert_eq!(env_value(&launch, "PGSSLMODE"), Some("disable"));
        assert_eq!(env_value(&launch, "PGHOSTADDR"), None, "not tunneled");
        assert!(launch.notes.is_empty());
    }

    #[test]
    fn psql_direct_tls_sets_sslnegotiation() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.port = 5432;
        profile.tls_mode = TlsMode::VerifyFull;
        profile.tls_direct = true;
        let launch = |profile: &ConnectionProfile| {
            build_launch(
                profile,
                NativeTool::Psql,
                Path::new("/psql"),
                ("db.internal", 1),
                &no_host(),
            )
            .unwrap()
        };
        assert_eq!(
            env_value(&launch(&profile), "PGSSLNEGOTIATION"),
            Some("direct")
        );
        profile.tls_mode = TlsMode::Disable;
        assert_eq!(env_value(&launch(&profile), "PGSSLNEGOTIATION"), None);
        profile.tls_mode = TlsMode::VerifyFull;
        profile.tls_direct = false;
        assert_eq!(env_value(&launch(&profile), "PGSSLNEGOTIATION"), None);
    }

    #[test]
    fn psql_tls_modes_map_to_sslmode() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.port = 5432;
        profile.encrypt = true;
        profile.trust_server_certificate = false;
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert_eq!(env_value(&launch, "PGSSLMODE"), Some("verify-full"));
        profile.trust_server_certificate = true;
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert_eq!(env_value(&launch, "PGSSLMODE"), Some("require"));
        assert_eq!(
            env_value(&launch, "PGSSLROOTCERT"),
            None,
            "require does not verify, so no root cert is needed"
        );
    }

    /// Verifying profiles without a CA file used to fail in psql (libpq looks
    /// for ~/.postgresql/root.crt); SQLLY verifies against the system roots,
    /// and so must the terminal. A CA file and client cert/key pass through.
    #[test]
    fn psql_passes_ca_and_client_certs_and_defaults_to_the_system_roots() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.tls_mode = TlsMode::VerifyFull;
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 5432),
            &no_host(),
        )
        .unwrap();
        assert_eq!(env_value(&launch, "PGSSLROOTCERT"), Some("system"));

        profile.tls_ca_file = Some(PathBuf::from("/certs/ca.pem"));
        profile.tls_client_cert_file = Some(PathBuf::from("/certs/client.pem"));
        profile.tls_client_key_file = Some(PathBuf::from("/certs/client.key"));
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 5432),
            &no_host(),
        )
        .unwrap();
        let shown = |p: &str| Path::new(p).display().to_string();
        assert_eq!(
            env_value(&launch, "PGSSLROOTCERT"),
            Some(shown("/certs/ca.pem").as_str())
        );
        assert_eq!(
            env_value(&launch, "PGSSLCERT"),
            Some(shown("/certs/client.pem").as_str())
        );
        assert_eq!(
            env_value(&launch, "PGSSLKEY"),
            Some(shown("/certs/client.key").as_str())
        );
    }

    /// Through an SSH tunnel the endpoint is 127.0.0.1; psql must dial it
    /// while still naming (and verifying) the real server.
    #[test]
    fn psql_through_a_tunnel_dials_the_forward_but_verifies_the_real_host() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.tls_mode = TlsMode::VerifyFull;
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("127.0.0.1", 50123),
            &no_host(),
        )
        .unwrap();
        assert_eq!(&launch.argv[..4], ["-h", "db.internal", "-p", "50123"]);
        assert_eq!(env_value(&launch, "PGHOSTADDR"), Some("127.0.0.1"));
        assert_eq!(env_value(&launch, "PGSSLMODE"), Some("verify-full"));
    }

    #[test]
    fn psql_integrated_auth_omits_the_user_flag() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.authentication = AuthenticationType::Integrated;
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert!(!launch.argv.iter().any(|a| a == "-U"));
        assert!(launch.env.iter().all(|e| e.key != "PGPASSWORD"));
    }

    #[test]
    fn a_default_port_is_left_to_the_cli() {
        let profile = profile_for(DatabaseType::Postgres);
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 0),
            &no_host(),
        )
        .unwrap();
        assert!(!launch.argv.iter().any(|a| a == "-p" || a == "0"));
        let sql = profile_for(DatabaseType::SqlServer);
        let launch = build_launch(
            &sql,
            NativeTool::Sqlcmd,
            Path::new("/s"),
            ("db.internal", 0),
            &no_host(),
        )
        .unwrap();
        assert_eq!(launch.argv[1], "tcp:db.internal");
    }

    #[test]
    fn mysql_builds_host_port_user_db_with_pwd_env() {
        let mut profile = profile_for(DatabaseType::MySql);
        profile.database = Some("shop".into());
        profile.encrypt = false;
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/usr/bin/mysql"),
            ("db.internal", 3306),
            &no_host(),
        )
        .unwrap();
        assert_eq!(
            launch.argv,
            vec!["-h", "db.internal", "-P", "3306", "-u", "app_user", "shop"]
        );
        assert!(
            launch
                .env
                .iter()
                .any(|e| e.key == "MYSQL_PWD" && e.value == "hunter2" && e.secret)
        );
        // TLS variants stay on argv (they are not secrets).
        profile.encrypt = true;
        profile.tls_ca_file = Some(PathBuf::from("/certs/ca.pem"));
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/m"),
            ("db.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert!(
            launch
                .argv
                .contains(&"--ssl-mode=VERIFY_IDENTITY".to_string())
        );
        assert!(
            launch
                .argv
                .iter()
                .any(|a| a.starts_with("--ssl-ca=") && a.ends_with("ca.pem"))
        );
        profile.database_type = DatabaseType::MariaDb;
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/m"),
            ("db.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert!(
            launch
                .argv
                .contains(&"--ssl-verify-server-cert".to_string())
        );
        assert!(!launch.argv.iter().any(|a| a.starts_with("--ssl-mode")));
    }

    /// The mysql client can't verify without a CA file: use the OS bundle
    /// when there is one, else downgrade visibly instead of failing to start.
    #[test]
    fn mysql_verification_uses_the_system_bundle_or_says_it_cannot_verify() {
        let mut profile = profile_for(DatabaseType::MySql);
        profile.tls_mode = TlsMode::VerifyFull;
        let facts =
            HostFacts::probe_with(&|p| p == Path::new("/etc/ssl/certs/ca-certificates.crt"));
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/m"),
            ("db.internal", 3306),
            &facts,
        )
        .unwrap();
        if cfg!(windows) {
            assert_eq!(facts.system_ca_bundle, None);
        } else {
            assert!(launch.argv.contains(&"--ssl-mode=VERIFY_IDENTITY".into()));
            assert!(
                launch
                    .argv
                    .iter()
                    .any(|a| a.starts_with("--ssl-ca=") && a.ends_with("ca-certificates.crt"))
            );
        }
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/m"),
            ("db.internal", 3306),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.contains(&"--ssl-mode=REQUIRED".into()));
        assert!(launch.notes.iter().any(|n| n.contains("CA file")));
    }

    #[test]
    fn mysql_through_a_tunnel_verifies_the_chain_not_the_host_name() {
        let mut profile = profile_for(DatabaseType::MySql);
        profile.tls_mode = TlsMode::VerifyFull;
        profile.tls_ca_file = Some(PathBuf::from("/certs/ca.pem"));
        let launch = build_launch(
            &profile,
            NativeTool::Mysql,
            Path::new("/m"),
            ("127.0.0.1", 40001),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.contains(&"--ssl-mode=VERIFY_CA".into()));
        assert!(!launch.argv.contains(&"--ssl-mode=VERIFY_IDENTITY".into()));
        assert!(launch.notes.iter().any(|n| n.contains("host name")));
    }

    #[test]
    fn sqlcmd_uses_tcp_address_and_password_env() {
        let mut profile = profile_for(DatabaseType::SqlServer);
        profile.encrypt = true;
        profile.trust_server_certificate = true;
        profile.database = Some("Sales".into());
        let launch = build_launch(
            &profile,
            NativeTool::Sqlcmd,
            Path::new("/opt/sqlcmd"),
            ("sql.internal", 1433),
            &no_host(),
        )
        .unwrap();
        assert_eq!(
            launch.argv,
            vec![
                "-S",
                "tcp:sql.internal,1433",
                "-U",
                "app_user",
                "-d",
                "Sales",
                "-Nm",
                "-C"
            ]
        );
        assert!(
            launch
                .env
                .iter()
                .any(|e| e.key == "SQLCMDPASSWORD" && e.value == "hunter2" && e.secret)
        );
        // Integrated auth switches to -E and drops the password route.
        profile.authentication = AuthenticationType::Integrated;
        let launch = build_launch(
            &profile,
            NativeTool::Sqlcmd,
            Path::new("/s"),
            ("sql.internal", 1433),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.contains(&"-E".to_string()));
        assert!(launch.env.iter().all(|e| e.key != "SQLCMDPASSWORD"));
    }

    /// `-N` never stands alone (go-sqlcmd would read the next flag as its
    /// value) and a plaintext profile asks ODBC 18 for optional encryption.
    #[test]
    fn sqlcmd_encryption_mode_is_attached_to_n() {
        let mut profile = profile_for(DatabaseType::SqlServer);
        for (mode, expected) in [
            (TlsMode::Disable, "-No"),
            (TlsMode::Require, "-Nm"),
            (TlsMode::VerifyFull, "-Nm"),
        ] {
            profile.tls_mode = mode;
            let launch = build_launch(
                &profile,
                NativeTool::Sqlcmd,
                Path::new("/s"),
                ("sql.internal", 1433),
                &no_host(),
            )
            .unwrap();
            assert!(launch.argv.contains(&expected.to_string()), "{mode:?}");
            assert!(!launch.argv.iter().any(|a| a == "-N"), "{mode:?}");
        }
    }

    /// Each sqlcmd flavor gets the `-N` spelling it actually accepts
    /// (checked against go-sqlcmd 1.10 and the go-sqlcmd/ODBC references).
    #[test]
    fn sqlcmd_encryption_flags_follow_the_installed_flavor() {
        use sqlly_services::SqlcmdFlavor;
        let modern_go = Some(SqlcmdFlavor::Go {
            major: 1,
            minor: 10,
        });
        let old_go = Some(SqlcmdFlavor::Go { major: 1, minor: 2 });
        let odbc18 = Some(SqlcmdFlavor::Odbc { major: 18 });
        let odbc17 = Some(SqlcmdFlavor::Odbc { major: 17 });
        let args = |flavor, tls| sqlcmd_encryption_args(flavor, tls);
        for flavor in [modern_go, odbc18, None] {
            assert_eq!(args(flavor, TlsMode::Disable), vec!["-No"]);
            assert_eq!(args(flavor, TlsMode::Require), vec!["-Nm", "-C"]);
            assert_eq!(args(flavor, TlsMode::VerifyFull), vec!["-Nm"]);
        }
        assert_eq!(args(old_go, TlsMode::Disable), vec!["-Nfalse"]);
        assert_eq!(args(old_go, TlsMode::Require), vec!["-Ntrue", "-C"]);
        assert_eq!(args(old_go, TlsMode::VerifyCa), vec!["-Ntrue"]);
        assert!(args(odbc17, TlsMode::Disable).is_empty());
        assert_eq!(args(odbc17, TlsMode::Require), vec!["-N", "-C"]);
        assert_eq!(args(odbc17, TlsMode::VerifyFull), vec!["-N"]);
    }

    /// Through a tunnel, a sqlcmd with `-F` verifies the certificate against
    /// the real server name; one without it (or an unknown one) gets no `-F`
    /// — go-sqlcmd before 1.9 reads `-F` as the output format — and a note.
    #[test]
    fn sqlcmd_through_a_tunnel_names_the_real_host_where_it_can() {
        use sqlly_services::SqlcmdFlavor;
        let mut profile = profile_for(DatabaseType::SqlServer);
        profile.server = "sql.internal".into();
        profile.tls_mode = TlsMode::VerifyFull;
        let launch_with = |flavor| {
            build_launch(
                &profile,
                NativeTool::Sqlcmd,
                Path::new("/s"),
                ("127.0.0.1", 40001),
                &HostFacts {
                    system_ca_bundle: None,
                    sqlcmd_flavor: flavor,
                    clickhouse_native_forward: None,
                },
            )
            .unwrap()
        };
        for flavor in [
            SqlcmdFlavor::Go { major: 1, minor: 9 },
            SqlcmdFlavor::Odbc { major: 18 },
        ] {
            let launch = launch_with(Some(flavor));
            assert!(
                launch.argv.windows(2).any(|w| w == ["-F", "sql.internal"]),
                "{flavor:?}: {:?}",
                launch.argv
            );
            assert!(launch.argv.contains(&"tcp:127.0.0.1,40001".to_string()));
            assert!(launch.notes.is_empty(), "{:?}", launch.notes);
        }
        for flavor in [
            Some(SqlcmdFlavor::Go { major: 1, minor: 8 }),
            Some(SqlcmdFlavor::Odbc { major: 17 }),
            None,
        ] {
            let launch = launch_with(flavor);
            assert!(!launch.argv.iter().any(|a| a == "-F"), "{flavor:?}");
            assert!(
                launch.notes.iter().any(|n| n.contains("127.0.0.1")),
                "{flavor:?}"
            );
        }
        // Not tunneled: no -F, no note.
        let direct = build_launch(
            &profile,
            NativeTool::Sqlcmd,
            Path::new("/s"),
            ("sql.internal", 1433),
            &HostFacts {
                system_ca_bundle: None,
                sqlcmd_flavor: Some(SqlcmdFlavor::Go {
                    major: 1,
                    minor: 10,
                }),
                clickhouse_native_forward: None,
            },
        )
        .unwrap();
        assert!(!direct.argv.iter().any(|a| a == "-F"));
    }

    #[test]
    fn redis_cli_covers_socket_database_user_auth_and_tls() {
        let mut profile = profile_for(DatabaseType::Redis);
        profile.database = Some("3".into());
        profile.encrypt = false;
        profile.server = "redis.internal".into();
        let launch = build_launch(
            &profile,
            NativeTool::RedisCli,
            Path::new("/usr/bin/redis-cli"),
            ("redis.internal", 6379),
            &no_host(),
        )
        .unwrap();
        assert_eq!(
            launch.argv,
            vec![
                "-h",
                "redis.internal",
                "-p",
                "6379",
                "-n",
                "3",
                "--user",
                "app_user"
            ]
        );
        assert!(
            launch
                .env
                .iter()
                .any(|e| e.key == "REDISCLI_AUTH" && e.value == "hunter2" && e.secret)
        );
        // Unix-socket profiles switch to -s.
        profile.server = "/var/run/redis.sock".into();
        let launch = build_launch(
            &profile,
            NativeTool::RedisCli,
            Path::new("/r"),
            ("/var/run/redis.sock", 0),
            &no_host(),
        )
        .unwrap();
        assert_eq!(launch.argv.first().map(String::as_str), Some("-s"));
        assert_eq!(
            launch.argv.get(1).map(String::as_str),
            Some("/var/run/redis.sock")
        );
        // TLS by default on ElastiCache: --tls, with --insecure only for Require.
        profile.server = "redis.internal".into();
        profile.encrypt = true;
        profile.trust_server_certificate = true;
        let launch = build_launch(
            &profile,
            NativeTool::RedisCli,
            Path::new("/r"),
            ("redis.internal", 1),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.windows(2).any(|w| w == ["--tls", "--insecure"]));
        // CA + client certs pass through; through a tunnel SNI names the
        // real server.
        profile.tls_mode = TlsMode::VerifyFull;
        profile.tls_ca_file = Some(PathBuf::from("/certs/ca.pem"));
        profile.tls_client_cert_file = Some(PathBuf::from("/certs/c.pem"));
        profile.tls_client_key_file = Some(PathBuf::from("/certs/c.key"));
        let launch = build_launch(
            &profile,
            NativeTool::RedisCli,
            Path::new("/r"),
            ("127.0.0.1", 40000),
            &no_host(),
        )
        .unwrap();
        assert!(
            launch
                .argv
                .windows(2)
                .any(|w| w == ["--sni", "redis.internal"])
        );
        for flag in ["--cacert", "--cert", "--key"] {
            assert!(launch.argv.iter().any(|a| a == flag), "{flag}");
        }
    }

    /// The profile port is ClickHouse's HTTP port; clickhouse-client speaks
    /// the native protocol, so the launch must map to 9000/9440.
    #[test]
    fn clickhouse_client_maps_the_http_port_to_the_native_port() {
        let mut profile = profile_for(DatabaseType::ClickHouse);
        profile.database = Some("analytics".into());
        profile.encrypt = false;
        profile.server = "ch.internal".into();
        profile.port = 8123;
        let launch = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/usr/bin/clickhouse-client"),
            ("ch.internal", 8123),
            &no_host(),
        )
        .unwrap();
        assert_eq!(
            launch.argv,
            vec![
                "--host",
                "ch.internal",
                "--port",
                "9000",
                "--user",
                "app_user",
                "--database",
                "analytics"
            ]
        );
        assert!(launch.notes.iter().any(|n| n.contains("9000")));
        assert!(
            launch
                .env
                .iter()
                .any(|e| e.key == "CLICKHOUSE_PASSWORD" && e.value == "hunter2" && e.secret)
        );

        profile.encrypt = true;
        profile.trust_server_certificate = false;
        profile.port = 8443;
        let launch = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("ch.internal", 8443),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.windows(2).any(|w| w == ["--port", "9440"]));
        assert!(launch.argv.contains(&"--secure".to_string()));

        // A non-standard HTTP port maps to the native default and says so.
        profile.port = 18123;
        let launch = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("ch.internal", 18123),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.windows(2).any(|w| w == ["--port", "9440"]));
        assert!(launch.notes.iter().any(|n| n.contains("assumed")));
    }

    /// A native port set on the connection beats the HTTP→native mapping,
    /// including for a non-standard HTTP port that would otherwise be a guess.
    #[test]
    fn clickhouse_client_uses_the_connections_native_port() {
        let mut profile = profile_for(DatabaseType::ClickHouse);
        profile.server = "ch.internal".into();
        profile.encrypt = false;
        profile.clickhouse_native_port = Some(19000);
        profile.port = 18123;
        let launch = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("ch.internal", 18123),
            &no_host(),
        )
        .unwrap();
        assert!(launch.argv.windows(2).any(|w| w == ["--port", "19000"]));
        assert!(!launch.notes.iter().any(|n| n.contains("assumed")));
        assert!(
            launch
                .notes
                .iter()
                .any(|n| n.contains("19000") && n.contains("Native port")),
            "{:?}",
            launch.notes
        );
        // Still refused through a tunnel: the tunnel forwards only HTTP.
        let err = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("127.0.0.1", 40002),
            &no_host(),
        )
        .unwrap_err();
        assert!(err.contains("native protocol"), "{err}");
    }

    /// Through a tunnel, clickhouse-client dials the second forward that
    /// `prepare_launch` opened to the native port — never the HTTP forward —
    /// and the notes explain the forward's lifetime.
    #[test]
    fn clickhouse_through_a_tunnel_uses_the_native_port_forward() {
        let mut profile = profile_for(DatabaseType::ClickHouse);
        profile.server = "ch.internal".into();
        profile.port = 8123;
        profile.encrypt = false;
        let launch = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("127.0.0.1", 40002),
            &HostFacts {
                clickhouse_native_forward: Some(40009),
                ..HostFacts::default()
            },
        )
        .unwrap();
        assert!(launch.argv.windows(2).any(|w| w == ["--host", "127.0.0.1"]));
        assert!(launch.argv.windows(2).any(|w| w == ["--port", "40009"]));
        assert!(
            !launch.argv.iter().any(|a| a == "40002"),
            "not the HTTP forward"
        );
        assert!(
            launch
                .notes
                .iter()
                .any(|n| n.contains("native port 9000") && n.contains("disconnecting closes it")),
            "{:?}",
            launch.notes
        );
    }

    #[test]
    fn clickhouse_through_a_tunnel_is_refused_honestly() {
        let profile = profile_for(DatabaseType::ClickHouse);
        let err = build_launch(
            &profile,
            NativeTool::ClickHouseClient,
            Path::new("/c"),
            ("127.0.0.1", 40002),
            &no_host(),
        )
        .unwrap_err();
        assert!(err.contains("native protocol"), "{err}");
    }

    #[test]
    fn file_engines_pass_the_path_and_refuse_memory() {
        let mut profile = profile_for(DatabaseType::Sqlite);
        profile.server = "/data/chinook.db".into();
        let launch = build_launch(
            &profile,
            NativeTool::Sqlite3,
            Path::new("/usr/bin/sqlite3"),
            ("/data/chinook.db", 0),
            &no_host(),
        )
        .unwrap();
        assert_eq!(launch.argv, vec!["/data/chinook.db"]);
        assert!(launch.env.is_empty());
        profile.server = ":memory:".into();
        profile.database = None;
        assert!(
            build_launch(
                &profile,
                NativeTool::Sqlite3,
                Path::new("/s"),
                (":memory:", 0),
                &no_host()
            )
            .is_err()
        );
        let mut duck = profile_for(DatabaseType::DuckDb);
        duck.server = "/data/warehouse.duckdb".into();
        let launch = build_launch(
            &duck,
            NativeTool::DuckDb,
            Path::new("/d"),
            ("/data/warehouse.duckdb", 0),
            &no_host(),
        )
        .unwrap();
        assert_eq!(launch.argv, vec!["/data/warehouse.duckdb"]);
    }

    #[test]
    fn oracle_prompts_for_the_password_instead_of_carrying_it() {
        let mut profile = profile_for(DatabaseType::Oracle);
        profile.database = Some("ORCLPDB1".into());
        profile.server = "oracle.internal".into();
        let launch = build_launch(
            &profile,
            NativeTool::Sqlplus,
            Path::new("/opt/oracle/sqlplus"),
            ("oracle.internal", 1521),
            &no_host(),
        )
        .unwrap();
        assert_eq!(
            launch.argv,
            vec!["-L", "app_user@//oracle.internal:1521/ORCLPDB1"]
        );
        assert!(
            launch.env.is_empty(),
            "SQL*Plus prompts inside the terminal; nothing rides along"
        );
        let launch = build_launch(
            &profile,
            NativeTool::Sqlcl,
            Path::new("/sql"),
            ("h", 0),
            &no_host(),
        )
        .unwrap();
        assert_eq!(launch.argv, vec!["app_user@//h:1521/ORCLPDB1"]);
    }

    #[test]
    fn iam_profiles_without_a_minted_token_say_the_cli_will_prompt() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.authentication = AuthenticationType::GcpCloudSqlIam {
            username: "svc@proj.iam".into(),
        };
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new("/psql"),
            ("db.internal", 5432),
            &no_host(),
        )
        .unwrap();
        assert!(launch.env.iter().all(|e| e.key != "PGPASSWORD"));
        assert!(launch.notes.iter().any(|n| n.contains("IAM")));
    }

    #[test]
    fn availability_names_the_missing_binary_or_the_blocker() {
        let pg = profile_for(DatabaseType::Postgres);
        assert_eq!(
            availability(DatabaseType::Postgres, &pg, &found_none),
            TerminalAvailability::Missing {
                tool: NativeTool::Psql,
            }
        );
        assert_eq!(
            availability(DatabaseType::Postgres, &pg, &found_all),
            TerminalAvailability::Ready(NativeTool::Psql)
        );
        // In-memory DuckDB is honestly disabled.
        let mut duck = profile_for(DatabaseType::DuckDb);
        duck.server = ":memory:".into();
        duck.database = None;
        assert!(matches!(
            availability(DatabaseType::DuckDb, &duck, &found_all),
            TerminalAvailability::Unsupported(_)
        ));
        // Entra-only SQL Server cannot be handed to sqlcmd.
        let mut entra = profile_for(DatabaseType::SqlServer);
        entra.authentication = AuthenticationType::EntraBrowser;
        assert!(matches!(
            availability(DatabaseType::SqlServer, &entra, &found_all),
            TerminalAvailability::Unsupported(_)
        ));
        // libSQL over HTTP has no CLI route.
        let libsql = profile_for(DatabaseType::LibSql);
        assert!(matches!(
            availability(DatabaseType::LibSql, &libsql, &found_all),
            TerminalAvailability::Unsupported(_)
        ));
        // Oracle prefers SQL*Plus, falls back to SQLcl, else names both.
        let oracle = profile_for(DatabaseType::Oracle);
        let only_sqlcl = |t: NativeTool| t == NativeTool::Sqlcl;
        assert_eq!(
            availability(DatabaseType::Oracle, &oracle, &only_sqlcl),
            TerminalAvailability::Ready(NativeTool::Sqlcl)
        );
        assert!(matches!(
            availability(DatabaseType::Oracle, &oracle, &found_none),
            TerminalAvailability::Missing { .. }
        ));
    }

    /// A relay profile's host is the bridge's network view; a local CLI must
    /// not dial it.
    #[test]
    fn relay_routed_profiles_are_refused() {
        let mut pg = profile_for(DatabaseType::Postgres);
        pg.route = sqlly_core::connection::ConnectionRoute::ViaRelay {
            relay_device_id: "dev".to_string(),
            pair_id: "pair".to_string(),
            relay_hint: None,
        };
        assert_eq!(
            availability(DatabaseType::Postgres, &pg, &found_all),
            TerminalAvailability::Unsupported(TerminalBlocker::RelayRoute)
        );
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let err = rt.block_on(prepare_launch(pg)).unwrap_err();
        assert!(err.contains("relay"), "{err}");
    }

    #[test]
    fn the_logged_form_never_contains_a_secret() {
        for launch in [
            psql_launch(),
            build_launch(
                &profile_for(DatabaseType::MySql),
                NativeTool::Mysql,
                Path::new("/m"),
                ("db.internal", 3306),
                &no_host(),
            )
            .unwrap(),
            build_launch(
                &profile_for(DatabaseType::SqlServer),
                NativeTool::Sqlcmd,
                Path::new("/s"),
                ("db.internal", 1433),
                &no_host(),
            )
            .unwrap(),
            build_launch(
                &profile_for(DatabaseType::Redis),
                NativeTool::RedisCli,
                Path::new("/r"),
                ("db.internal", 6379),
                &no_host(),
            )
            .unwrap(),
            build_launch(
                &profile_for(DatabaseType::ClickHouse),
                NativeTool::ClickHouseClient,
                Path::new("/c"),
                ("db.internal", 8123),
                &no_host(),
            )
            .unwrap(),
        ] {
            let secrets = launch.secret_values();
            assert!(!secrets.is_empty(), "{launch:?} should carry a secret");
            let display = launch.display_command();
            let debug = format!("{launch:?}");
            let mut spawn_debug = Vec::new();
            for kind in [
                TerminalKind::MacOs,
                TerminalKind::Linux,
                TerminalKind::Windows,
            ] {
                spawn_debug.push(format!("{:?}", terminal_command(kind, &launch).unwrap()));
            }
            for secret in secrets {
                assert!(
                    !display.contains(secret),
                    "display leaks {secret}: {display}"
                );
                assert!(!debug.contains(secret), "debug leaks {secret}: {debug}");
                for form in &spawn_debug {
                    assert!(!form.contains(secret), "spawn debug leaks {secret}: {form}");
                }
            }
            assert!(display.contains("<redacted>"), "{display}");
        }
    }

    #[test]
    fn terminal_command_per_platform() {
        let launch = psql_launch();
        // Linux: env on the spawned terminal process; argv stays secret-free,
        // and the CLI runs through a keep-open `sh -c` as positional args.
        let linux = terminal_command(TerminalKind::Linux, &launch).unwrap();
        assert_eq!(linux.program, "x-terminal-emulator");
        assert_eq!(&linux.args[..3], ["-e", "sh", "-c"]);
        assert_eq!(linux.args[3], LINUX_KEEP_OPEN);
        assert_eq!(linux.args.get(4).map(String::as_str), Some("/usr/bin/psql"));
        assert_eq!(&linux.args[5..], launch.argv.as_slice());
        assert!(
            linux
                .env
                .iter()
                .any(|(k, v)| k == "PGPASSWORD" && v == "hunter2")
        );
        assert!(
            !linux.args.iter().any(|a| a.contains("hunter2")),
            "argv must stay secret-free: {:?}",
            linux.args
        );
        // Windows: env inheritance through cmd start; the launcher script
        // holds the (escaped) CLI line but never the password.
        let windows = terminal_command(TerminalKind::Windows, &launch).unwrap();
        assert_eq!(windows.program, "cmd");
        let line = windows
            .raw_command_line
            .clone()
            .expect("cmd gets a raw line");
        assert_eq!(line, "/D /V:ON /C start \"SQLLY\" \"!SQLLY_LAUNCHER!\"");
        assert!(windows.env.iter().any(|(k, _)| k == "PGPASSWORD"));
        let launcher = windows.launcher.expect("Windows writes a launcher");
        assert!(!launcher.holds_secret);
        assert!(!launcher.script.contains("hunter2"));
        assert_eq!(launcher.path_env, Some("SQLLY_LAUNCHER"));
        assert!(
            launcher.script.contains("\"/usr/bin/psql\" ^\"-h^\""),
            "{}",
            launcher.script
        );
        // macOS: a self-deleting launcher script carries env (Terminal.app
        // inherits nothing); the script is the ONLY place the secret lands.
        let mac = terminal_command(TerminalKind::MacOs, &launch).unwrap();
        assert_eq!(mac.program, "open");
        assert_eq!(mac.args, ["-a", "Terminal"]);
        let launcher = mac.launcher.expect("macOS writes a launcher");
        assert!(launcher.holds_secret);
        assert_eq!(launcher.suffix, ".command");
        assert_eq!(launcher.path_env, None, "the path is open's argument");
        let script = launcher.script;
        assert!(
            script.contains("rm -f -- \"$0\""),
            "self-delete first: {script}"
        );
        let rm_at = script.find("rm -f").unwrap();
        let export_at = script.find("export PGPASSWORD").unwrap();
        assert!(rm_at < export_at, "delete before anything else: {script}");
        assert!(script.contains("export PGPASSWORD='hunter2'"), "{script}");
        assert!(script.contains("exec '/usr/bin/psql'"), "{script}");
    }

    #[test]
    fn linux_tries_the_preferred_terminal_then_the_common_emulators() {
        let launch = psql_launch();
        let plans = linux_commands(Some("foot"), &launch);
        let programs: Vec<&str> = plans.iter().map(|c| c.program.as_str()).collect();
        assert_eq!(programs.first(), Some(&"foot"));
        assert_eq!(plans[0].args.first().map(String::as_str), Some("-e"));
        assert!(programs.contains(&"gnome-terminal"));
        assert_eq!(programs.last(), Some(&"xterm"));
        let gnome = plans
            .iter()
            .find(|c| c.program == "gnome-terminal")
            .unwrap();
        assert_eq!(gnome.args.first().map(String::as_str), Some("--"));
        // A preference naming a built-in candidate does not duplicate it.
        let plans = linux_commands(Some("xterm"), &launch);
        assert_eq!(plans.iter().filter(|c| c.program == "xterm").count(), 1);
        assert_eq!(plans[0].program, "xterm");
        for plan in &plans {
            assert!(!plan.args.iter().any(|a| a.contains("hunter2")));
        }
    }

    /// `$TERMINAL=gnome-terminal` (or a full path to it) must use its own
    /// `--` separator, not a blanket `-e`, and must not leave a second
    /// gnome-terminal entry behind it.
    #[test]
    fn a_preferred_known_terminal_keeps_its_own_separator() {
        let launch = psql_launch();
        let plans = linux_commands(Some("/usr/bin/gnome-terminal"), &launch);
        assert_eq!(plans[0].program, "/usr/bin/gnome-terminal");
        assert_eq!(plans[0].args.first().map(String::as_str), Some("--"));
        assert!(
            !plans[1..].iter().any(|c| c.program == "gnome-terminal"),
            "no shadowed duplicate"
        );
        let plans = linux_commands(Some("wezterm"), &launch);
        assert_eq!(&plans[0].args[..3], ["start", "--", "sh"]);
    }

    // ---- Windows command-line safety ---------------------------------

    /// What `cmd.exe` hands to CreateProcess for one batch-file line:
    /// `%%` → `%` (a lone `%` would expand a variable), carets escape the
    /// next character, `"` toggles quote mode, and an unescaped
    /// metacharacter outside quotes is a command separator or redirection —
    /// i.e. injection.
    fn simulate_cmd_batch_line(line: &str) -> Result<String, String> {
        let mut phase1 = String::new();
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '%' {
                if chars.peek() == Some(&'%') {
                    chars.next();
                    phase1.push('%');
                } else {
                    return Err(format!("unescaped % would expand: {line}"));
                }
            } else {
                phase1.push(c);
            }
        }
        let mut out = String::new();
        let mut quoted = false;
        let mut chars = phase1.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => {
                    quoted = !quoted;
                    out.push(c);
                }
                '^' if !quoted => out.push(chars.next().ok_or("dangling caret")?),
                '&' | '|' | '<' | '>' | '(' | ')' if !quoted => {
                    return Err(format!("unescaped {c} outside quotes: {line}"));
                }
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// `CommandLineToArgvW`: whitespace splits outside quotes; 2n
    /// backslashes + quote → n backslashes and a quote toggle; 2n+1 → n
    /// backslashes and a literal quote.
    fn msvc_split(line: &str) -> Vec<String> {
        let mut args = Vec::new();
        let mut current = String::new();
        let mut in_arg = false;
        let mut quoted = false;
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if c == '\\' {
                let start = i;
                while i < chars.len() && chars[i] == '\\' {
                    i += 1;
                }
                let n = i - start;
                if i < chars.len() && chars[i] == '"' {
                    current.extend(std::iter::repeat_n('\\', n / 2));
                    if n % 2 == 1 {
                        current.push('"');
                        i += 1;
                    }
                } else {
                    current.extend(std::iter::repeat_n('\\', n));
                }
                in_arg = true;
                continue;
            }
            match c {
                '"' => {
                    quoted = !quoted;
                    in_arg = true;
                }
                ' ' | '\t' if !quoted => {
                    if in_arg {
                        args.push(std::mem::take(&mut current));
                        in_arg = false;
                    }
                }
                _ => {
                    current.push(c);
                    in_arg = true;
                }
            }
            i += 1;
        }
        if in_arg {
            args.push(current);
        }
        args
    }

    /// The CLI line of the Windows launcher, round-tripped through cmd's
    /// parser and the MSVC argv splitter, must give the CLI exactly the
    /// values SQLLY meant — no command separators, no variable expansion.
    #[test]
    fn windows_launcher_survives_hostile_names() {
        let hostile = [
            "x&calc",
            "a|b",
            "%PATH%",
            "50%",
            "quo\"te",
            "car^et",
            "par(en)",
            "red>irect<",
            "bang!USERNAME!",
            "trailing\\",
            "back\\\"slash",
            "spaced name",
            "",
        ];
        for value in hostile {
            let mut profile = profile_for(DatabaseType::Postgres);
            profile.database = Some(format!("db{value}"));
            profile.authentication = AuthenticationType::SqlLogin {
                username: format!("u{value}"),
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "hunter2".into(),
                },
            };
            let launch = build_launch(
                &profile,
                NativeTool::Psql,
                Path::new(r"C:\Program Files\PostgreSQL\16\bin\psql.exe"),
                ("db.internal", 5432),
                &no_host(),
            )
            .unwrap();
            let script = windows_launcher_script(&launch).unwrap();
            let line = script
                .split("\r\n")
                .find(|l| l.starts_with("\"C:\\Program Files"))
                .expect("the CLI line");
            let parsed = simulate_cmd_batch_line(line)
                .unwrap_or_else(|e| panic!("{value:?} broke cmd parsing: {e}"));
            let argv = msvc_split(&parsed);
            let mut expected = vec![r"C:\Program Files\PostgreSQL\16\bin\psql.exe".to_string()];
            expected.extend(launch.argv.iter().cloned());
            assert_eq!(argv, expected, "{value:?} round-trip");
        }
    }

    #[test]
    fn windows_launcher_refuses_line_breaks_and_keeps_the_secret_out() {
        let mut profile = profile_for(DatabaseType::Postgres);
        profile.database = Some("a\r\ncalc".into());
        let launch = build_launch(
            &profile,
            NativeTool::Psql,
            Path::new(r"C:\pg\psql.exe"),
            ("db.internal", 5432),
            &no_host(),
        )
        .unwrap();
        assert!(windows_launcher_script(&launch).is_err());
        assert!(terminal_command(TerminalKind::Windows, &launch).is_err());
        let launch = psql_launch();
        let script = windows_launcher_script(&launch).unwrap();
        assert!(!script.contains("hunter2"));
        assert!(script.contains("pause"), "the window stays open: {script}");
        assert!(
            script.contains("setlocal DisableDelayedExpansion"),
            "{script}"
        );
    }

    #[test]
    fn msvc_quote_matches_the_documented_rules() {
        assert_eq!(msvc_quote("plain"), "\"plain\"");
        assert_eq!(msvc_quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(msvc_quote("dir\\"), "\"dir\\\\\"");
        assert_eq!(msvc_quote("a\\\\\"b"), "\"a\\\\\\\\\\\"b\"");
        assert_eq!(msvc_quote("a\\b"), "\"a\\b\"");
    }

    #[cfg(unix)]
    #[test]
    fn launcher_is_written_owner_only_and_executable() {
        use std::os::unix::fs::PermissionsExt;
        let launcher = Launcher {
            script: "echo hi\n".into(),
            suffix: ".command",
            holds_secret: true,
            path_env: None,
        };
        let path = write_launcher(&launcher).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(
            std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n"),
            "echo hi\n"
        );
        let second = write_launcher(&launcher).unwrap();
        assert_ne!(path, second, "each launch gets its own exclusive file");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(second);
    }

    #[test]
    fn dump_tools_are_not_interactive_clis() {
        let profile = profile_for(DatabaseType::Postgres);
        assert!(
            build_launch(
                &profile,
                NativeTool::PgDump,
                Path::new("/pg_dump"),
                ("db.internal", 1),
                &no_host(),
            )
            .is_err()
        );
    }

    #[test]
    fn clickhouse_native_port_table() {
        assert_eq!(clickhouse_native_port(8123, false), (9000, false));
        assert_eq!(clickhouse_native_port(0, false), (9000, false));
        assert_eq!(clickhouse_native_port(8443, true), (9440, false));
        assert_eq!(clickhouse_native_port(9000, false), (9000, false));
        assert_eq!(clickhouse_native_port(9440, true), (9440, false));
        assert_eq!(clickhouse_native_port(18123, false), (9000, true));
    }
}
