//! Parse connection URLs, JDBC strings, ADO.NET key/value DSNs, and SQLite paths
//! into editor fields. Passwords are returned separately and must never be
//! written to persisted profiles or export bundles by callers.

use crate::connection::TlsMode;
use percent_encoding::percent_decode_str;
use sqlly_lang::sqlmodel::DatabaseType;
use std::path::PathBuf;

#[derive(Clone, PartialEq, Eq, Default)]
pub struct ParsedConnectionUrl {
    pub engine: DatabaseType,
    /// Host (or SQLite file path). Includes `\instance` for SQL Server named instances.
    pub server: String,
    /// `None` means "use the engine default port".
    pub port: Option<u16>,
    pub database: Option<String>,
    pub username: Option<String>,
    /// Present only if the string embedded one. Callers push this into the
    /// password input; it must never be written to a profile or a file.
    pub password: Option<String>,
    pub encrypt: Option<bool>,
    pub trust_server_certificate: Option<bool>,
    /// Explicit TLS mode when the string carried one (`sslmode=verify-ca`,
    /// `ssl-mode=REQUIRED`, …). `encrypt` is still set for older readers.
    pub tls_mode: Option<TlsMode>,
    /// CA bundle path from `sslrootcert` (PG) / `ssl-ca` (MySQL).
    pub tls_ca_file: Option<PathBuf>,
    /// Client certificate path from `sslcert` (PG) / `ssl-cert` (MySQL).
    pub tls_client_cert_file: Option<PathBuf>,
    /// Client key path from `sslkey` (PG) / `ssl-key` (MySQL).
    pub tls_client_key_file: Option<PathBuf>,
    /// PostgreSQL `sslnegotiation`: `direct` is `Some(true)`, `postgres`
    /// `Some(false)`.
    pub tls_direct: Option<bool>,
    pub integrated_security: bool,
    /// Redis-family URLs only: every host of a comma host list
    /// (`redis://a:6379,b:6380`), in order, when it named more than one.
    /// `server`/`port` still hold the first. Empty for a single host and for
    /// every other engine, whose drivers dial one host.
    pub hosts: Vec<(String, Option<u16>)>,
    /// `redis+sentinel://` (and `rediss+sentinel://`, `valkey+sentinel://`):
    /// the master name from the first path segment and the sentinels from the
    /// host list. `sentinel_password_ref` is always `None` here; the
    /// password itself travels in [`Self::sentinel_password`].
    pub redis_sentinel: Option<Box<crate::redis_sentinel::RedisSentinelConfig>>,
    /// `?sentinelPassword=` on a Sentinel URL. Like `password`, callers push
    /// it into the editor's field; it must never be written to a profile.
    pub sentinel_password: Option<String>,
    /// Remaining query/option pairs the form does not model, for the status line.
    pub extra_options: Vec<(String, String)>,
}

/// Debug output masks both passwords, so a parsed URL can reach a log or a
/// panic message without leaking them.
impl std::fmt::Debug for ParsedConnectionUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mask = |secret: &Option<String>| secret.as_ref().map(|_| "<redacted>");
        f.debug_struct("ParsedConnectionUrl")
            .field("engine", &self.engine)
            .field("server", &self.server)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("username", &self.username)
            .field("password", &mask(&self.password))
            .field("encrypt", &self.encrypt)
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("tls_mode", &self.tls_mode)
            .field("tls_ca_file", &self.tls_ca_file)
            .field("tls_client_cert_file", &self.tls_client_cert_file)
            .field("tls_client_key_file", &self.tls_client_key_file)
            .field("tls_direct", &self.tls_direct)
            .field("integrated_security", &self.integrated_security)
            .field("hosts", &self.hosts)
            .field("redis_sentinel", &self.redis_sentinel)
            .field("sentinel_password", &mask(&self.sentinel_password))
            .field("extra_options", &self.extra_options)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseUrlError {
    #[error("empty connection string")]
    Empty,
    #[error("unrecognized connection string format")]
    Unrecognized,
    #[error("{0} is not a supported engine")]
    UnsupportedEngine(String),
    #[error("invalid port {0}")]
    InvalidPort(String),
    #[error("invalid URL: {0}")]
    Url(String),
}

impl ParseUrlError {
    pub fn message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        match self {
            Self::Empty => {
                DisplayMessage::new("services.connection_url.errors.empty", self.to_string())
            }
            Self::Unrecognized => DisplayMessage::new(
                "services.connection_url.errors.unrecognized",
                self.to_string(),
            ),
            Self::UnsupportedEngine(engine) => DisplayMessage::new(
                "services.connection_url.errors.unsupported_engine",
                self.to_string(),
            )
            .with_arg("engine_name", engine),
            Self::InvalidPort(port) => DisplayMessage::new(
                "services.connection_url.errors.invalid_port",
                self.to_string(),
            )
            .with_arg("port", port),
            Self::Url(detail) => DisplayMessage::new(
                "services.connection_url.errors.invalid_url",
                self.to_string(),
            )
            .with_arg("detail", detail),
        }
    }
}

pub fn parse_connection_url(input: &str) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let input = input.trim();
    if input.is_empty() {
        return Err(ParseUrlError::Empty);
    }

    let mut working = input;
    let mut is_jdbc = false;
    if working.len() >= 5 && working[..5].eq_ignore_ascii_case("jdbc:") {
        working = &working[5..];
        is_jdbc = true;
    }

    if is_ado_net(working) {
        return parse_ado_net(working);
    }

    if working.starts_with("oracle")
        || working.starts_with("Oracle")
        || working.starts_with("db2")
        || working.starts_with("DB2")
    {
        let scheme = working
            .split(':')
            .next()
            .unwrap_or("unknown")
            .to_ascii_lowercase();
        return Err(ParseUrlError::UnsupportedEngine(scheme));
    }

    if is_jdbc && working.len() >= 10 && working[..10].eq_ignore_ascii_case("sqlserver:") {
        return parse_jdbc_sqlserver(&working[10..]);
    }

    if (is_jdbc || working.starts_with("sqlite:") || working.starts_with("SQLite:"))
        && working.len() >= 7
        && working[..7].eq_ignore_ascii_case("sqlite:")
    {
        return parse_sqlite_value(&working[7..], is_jdbc);
    }

    // `duckdb:/path/to/file.duckdb`, `duckdb:///path`, `jdbc:duckdb:/path`,
    // `duckdb::memory:`.
    if working.len() >= 7 && working[..7].eq_ignore_ascii_case("duckdb:") {
        return parse_duckdb_value(&working[7..]);
    }

    if let Some(pos) = working.find("://") {
        let scheme = &working[..pos];
        let rest = &working[pos + 3..];
        return parse_schemed(scheme, rest, is_jdbc);
    }

    if working.len() >= 5 && working[..5].eq_ignore_ascii_case("file:") {
        return parse_file_value(&working[5..], false);
    }

    if is_bare_duckdb_path(working) {
        return Ok(duckdb_profile(working));
    }

    if is_bare_sqlite_path(working) {
        return Ok(sqlite_profile(working));
    }

    Err(ParseUrlError::Unrecognized)
}

fn is_ado_net(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("server=")
        || lower.starts_with("data source=")
        || lower.starts_with("host=")
    {
        return true;
    }
    if let Some(scheme_end) = s.find("://") {
        let before = &s[..scheme_end];
        before.contains('=') && before.contains(';')
    } else {
        s.contains('=') && s.contains(';')
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn parse_port(raw: &str) -> Result<u16, ParseUrlError> {
    let port: u16 = raw
        .parse()
        .map_err(|_| ParseUrlError::InvalidPort(raw.to_string()))?;
    if port == 0 {
        return Err(ParseUrlError::InvalidPort(raw.to_string()));
    }
    Ok(port)
}

fn strip_ipv6_brackets(host: &str) -> String {
    let host = host.trim();
    if host.starts_with('[') && host.ends_with(']') {
        host[1..host.len() - 1].to_string()
    } else {
        host.to_string()
    }
}

fn parse_bool_key(key: &str, value: &str) -> Option<bool> {
    let key = key.to_ascii_lowercase();
    let value = value.trim();
    match key.as_str() {
        "sslmode" | "ssl-mode" | "ssl" => match value.to_ascii_lowercase().as_str() {
            "require" | "verify-ca" | "verify-full" | "verify_ca" | "verify_full"
            | "verify-identity" | "verify_identity" | "required" | "true" => Some(true),
            "disable" | "disabled" | "false" => Some(false),
            _ => None,
        },
        "encrypt" => Some(parse_bool_value(value)),
        "trustservercertificate" | "trust_server_certificate" => Some(parse_bool_value(value)),
        _ => None,
    }
}

/// Maps an explicit `sslmode` / `ssl-mode` word onto a [`TlsMode`]. Bare
/// booleans (`true` / `false`) stay `None` so they keep the legacy
/// encrypt-flag meaning (full verification when on).
fn parse_tls_mode_value(value: &str) -> Option<TlsMode> {
    match value.trim().to_ascii_lowercase().as_str() {
        "disable" | "disabled" => Some(TlsMode::Disable),
        "require" | "required" => Some(TlsMode::Require),
        "verify-ca" | "verify_ca" => Some(TlsMode::VerifyCa),
        "verify-full" | "verify_full" | "verify-identity" | "verify_identity" => {
            Some(TlsMode::VerifyFull)
        }
        _ => None,
    }
}

fn parse_bool_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "yes" | "1" | "mandatory" | "strict" | "sspi"
    )
}

fn sqlite_profile(path: &str) -> ParsedConnectionUrl {
    ParsedConnectionUrl {
        engine: DatabaseType::Sqlite,
        server: path.to_string(),
        port: None,
        ..Default::default()
    }
}

fn parse_sqlite_value(path: &str, is_jdbc: bool) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let path = path.trim();
    if path.eq_ignore_ascii_case("memory:") || path.eq_ignore_ascii_case(":memory:") {
        return Err(ParseUrlError::Unrecognized);
    }
    let server = if is_jdbc {
        if path.starts_with("//") {
            path[1..].to_string()
        } else {
            path.to_string()
        }
    } else if path.starts_with("///") {
        path[2..].to_string()
    } else if path.starts_with("//") {
        path.to_string()
    } else {
        path.trim_start_matches('/').to_string()
    };
    if server.is_empty() {
        return Err(ParseUrlError::Unrecognized);
    }
    Ok(sqlite_profile(&server))
}

fn duckdb_profile(path: &str) -> ParsedConnectionUrl {
    ParsedConnectionUrl {
        engine: DatabaseType::DuckDb,
        server: path.to_string(),
        port: None,
        ..Default::default()
    }
}

/// `duckdb:` values: `///abs/path` and `/abs/path` both mean the absolute
/// path, anything else is taken as written (a relative file), and
/// `:memory:` is DuckDB's own spelling of a scratch database, which the
/// connection editor accepts as-is.
fn parse_duckdb_value(path: &str) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let path = path.trim();
    if path.eq_ignore_ascii_case(":memory:") || path.eq_ignore_ascii_case("memory:") {
        return Ok(duckdb_profile(":memory:"));
    }
    let server = match path.strip_prefix("//") {
        Some(rest) if rest.starts_with('/') => rest.to_string(),
        Some(rest) => rest.to_string(),
        None => path.to_string(),
    };
    if server.is_empty() {
        return Err(ParseUrlError::Unrecognized);
    }
    Ok(duckdb_profile(&server))
}

/// `file:` values pick the engine from the extension: `.duckdb` / `.ddb`
/// are DuckDB, everything else is SQLite as before.
fn parse_file_value(path: &str, is_jdbc: bool) -> Result<ParsedConnectionUrl, ParseUrlError> {
    if is_bare_duckdb_path(path.trim()) {
        parse_duckdb_value(path)
    } else {
        parse_sqlite_value(path, is_jdbc)
    }
}

fn is_bare_duckdb_path(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.ends_with(".duckdb") || lower.ends_with(".ddb")
}

fn is_bare_sqlite_path(s: &str) -> bool {
    let lower = s.to_ascii_lowercase();
    lower.ends_with(".db")
        || lower.ends_with(".sqlite")
        || lower.ends_with(".sqlite3")
        || s.contains('/')
        || s.contains('\\')
}

fn engine_from_scheme(scheme: &str) -> Result<DatabaseType, ParseUrlError> {
    match scheme.to_ascii_lowercase().as_str() {
        "postgres" | "postgresql" => Ok(DatabaseType::Postgres),
        "mysql" => Ok(DatabaseType::MySql),
        "mariadb" => Ok(DatabaseType::MariaDb),
        "sqlserver" | "mssql" => Ok(DatabaseType::SqlServer),
        "sqlite" | "file" => Ok(DatabaseType::Sqlite),
        // Turso's own scheme, plus the plain HTTP(S) endpoint a self-hosted
        // `sqld` (or a Turso database URL) is reached at.
        "libsql" | "http" | "https" => Ok(DatabaseType::LibSql),
        "clickhouse" => Ok(DatabaseType::ClickHouse),
        "duckdb" => Ok(DatabaseType::DuckDb),
        "redis" | "rediss" | "redis+sentinel" | "rediss+sentinel" => Ok(DatabaseType::Redis),
        "valkey" | "valkeys" | "valkey+sentinel" | "valkeys+sentinel" => Ok(DatabaseType::Valkey),
        "oracle" | "db2" => Err(ParseUrlError::UnsupportedEngine(scheme.to_string())),
        other => Err(ParseUrlError::UnsupportedEngine(other.to_string())),
    }
}

fn split_userinfo_host_correct(rest: &str) -> (Option<&str>, &str) {
    if let Some(at) = rest.find('@') {
        let (userinfo, hostpart) = rest.split_at(at);
        (Some(userinfo), &hostpart[1..])
    } else {
        (None, rest)
    }
}

fn parse_userinfo(userinfo: &str) -> (Option<String>, Option<String>) {
    if let Some(colon) = userinfo.find(':') {
        (
            Some(decode(&userinfo[..colon])),
            Some(decode(&userinfo[colon + 1..])),
        )
    } else {
        (Some(decode(userinfo)), None)
    }
}

fn parse_host_port_first(hostpart: &str) -> Result<(String, Option<u16>), ParseUrlError> {
    let hostpart = hostpart.trim();
    if hostpart.is_empty() {
        return Ok((String::new(), None));
    }
    let first = hostpart.split(',').next().unwrap_or(hostpart).trim();
    if first.starts_with('[')
        && let Some(end) = first.find(']')
    {
        let host = first[1..end].to_string();
        let after = first[end + 1..].trim_start_matches(':');
        let port = if after.is_empty() {
            None
        } else {
            Some(parse_port(after)?)
        };
        return Ok((host, port));
    }
    if let Some(colon) = first.rfind(':') {
        let maybe_port = &first[colon + 1..];
        if !maybe_port.is_empty() {
            if maybe_port.chars().all(|c| c.is_ascii_digit()) {
                return Ok((first[..colon].to_string(), Some(parse_port(maybe_port)?)));
            }
            if !first.starts_with('[') && !maybe_port.contains(',') {
                return Err(ParseUrlError::InvalidPort(maybe_port.to_string()));
            }
        }
    }
    Ok((first.to_string(), None))
}

fn split_path_query(rest: &str) -> (&str, &str) {
    match rest.find('?') {
        Some(q) => (&rest[..q], &rest[q + 1..]),
        None => (rest, ""),
    }
}

fn apply_query_pairs(
    parsed: &mut ParsedConnectionUrl,
    query: &str,
    is_sqlserver: bool,
) -> Vec<(String, String)> {
    let mut extra = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = match pair.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(pair), String::new()),
        };
        let key_lower = key.to_ascii_lowercase();
        if matches!(key_lower.as_str(), "database" | "dbname") && is_sqlserver {
            if parsed.database.is_none() && !value.is_empty() {
                parsed.database = Some(value);
            }
            continue;
        }
        if matches!(key_lower.as_str(), "user" | "username") && parsed.username.is_none() {
            if !value.is_empty() {
                parsed.username = Some(value);
            }
            continue;
        }
        if key_lower == "password" && parsed.password.is_none() {
            if !value.is_empty() {
                parsed.password = Some(value);
            }
            continue;
        }
        if matches!(key_lower.as_str(), "sslrootcert" | "ssl-ca" | "sslca") && !value.is_empty() {
            parsed.tls_ca_file = Some(PathBuf::from(value));
            continue;
        }
        if matches!(key_lower.as_str(), "sslcert" | "ssl-cert") && !value.is_empty() {
            parsed.tls_client_cert_file = Some(PathBuf::from(value));
            continue;
        }
        if matches!(key_lower.as_str(), "sslkey" | "ssl-key") && !value.is_empty() {
            parsed.tls_client_key_file = Some(PathBuf::from(value));
            continue;
        }
        if key_lower == "sslnegotiation" {
            match value.to_ascii_lowercase().as_str() {
                "direct" => parsed.tls_direct = Some(true),
                "postgres" => parsed.tls_direct = Some(false),
                _ => parsed.extra_options.push((key, value)),
            }
            continue;
        }
        if let Some(b) = parse_bool_key(&key_lower, &value) {
            if key_lower.contains("encrypt")
                || key_lower == "sslmode"
                || key_lower == "ssl-mode"
                || key_lower == "ssl"
            {
                parsed.encrypt = Some(b);
                if let Some(mode) = parse_tls_mode_value(&value) {
                    parsed.tls_mode = Some(mode);
                }
            } else if key_lower.contains("trust") {
                parsed.trust_server_certificate = Some(b);
            }
            continue;
        }
        extra.push((key, value));
    }
    extra
}

fn parse_schemed(
    scheme: &str,
    rest: &str,
    is_jdbc: bool,
) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let engine = engine_from_scheme(scheme)?;
    if engine == DatabaseType::Sqlite {
        return if scheme.eq_ignore_ascii_case("file") {
            parse_file_value(rest, is_jdbc)
        } else {
            parse_sqlite_value(rest, is_jdbc)
        };
    }
    if engine == DatabaseType::DuckDb {
        return parse_duckdb_value(rest);
    }

    let (userinfo, after_user) = split_userinfo_host_correct(rest);
    let (path_query, _) = split_path_query(after_user);
    let (hostpart, path_part) = match path_query.find('/') {
        Some(slash) => (&path_query[..slash], &path_query[slash + 1..]),
        None => (path_query, ""),
    };
    let (_, query) = split_path_query(after_user);

    let (username, password) = userinfo.map(parse_userinfo).unwrap_or((None, None));

    let (server, port) = parse_host_port_first(hostpart)?;
    let mut server = strip_ipv6_brackets(&server);
    // A Redis URL may list several seed hosts (`redis://h1:7000,h2:7001`):
    // keep them all in the server field, the first host's port in `port`.
    if engine.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Resp {
        let rest: Vec<&str> = hostpart
            .split(',')
            .skip(1)
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .collect();
        if !rest.is_empty() {
            server = std::iter::once(server.as_str())
                .chain(rest)
                .collect::<Vec<_>>()
                .join(",");
        }
    }

    let mut parsed = ParsedConnectionUrl {
        engine,
        server,
        port,
        username,
        password,
        ..Default::default()
    };

    if !path_part.is_empty() {
        let segments: Vec<&str> = path_part.split('/').filter(|s| !s.is_empty()).collect();
        match engine {
            DatabaseType::SqlServer if segments.len() >= 2 => {
                parsed.server = format!("{}\\{}", parsed.server, segments[0]);
                parsed.database = Some(decode(segments[1]));
            }
            DatabaseType::SqlServer if segments.len() == 1 => {
                parsed.database = Some(decode(segments[0]));
            }
            _ if !segments.is_empty() => {
                parsed.database = Some(decode(segments[0]));
            }
            _ => {}
        }
    }

    parsed.extra_options = apply_query_pairs(&mut parsed, query, engine == DatabaseType::SqlServer);
    if engine == DatabaseType::LibSql {
        apply_libsql_scheme(&mut parsed, scheme);
    }
    // `rediss://` / `valkeys://` mean TLS; the plain schemes mean plaintext.
    // Both arms are explicit so a pasted URL replaces whatever the form held.
    // Require rather than a verifying mode: a self-hosted TLS port usually
    // presents a self-signed certificate, and the paste must never claim to
    // have checked a chain.
    if engine.connection_kind() == sqlly_lang::sqlmodel::ConnectionKind::Resp {
        let scheme_lower = scheme.to_ascii_lowercase();
        let sentinel_scheme = scheme_lower.ends_with("+sentinel");
        let base_scheme = scheme_lower.trim_end_matches("+sentinel");
        apply_resp_host_list(&mut parsed, hostpart, path_part, sentinel_scheme)?;
        let tls = matches!(base_scheme, "rediss" | "valkeys");
        parsed.encrypt = Some(tls);
        parsed.trust_server_certificate = Some(tls);
        parsed.tls_mode = Some(if tls {
            TlsMode::Require
        } else {
            TlsMode::Disable
        });
        // `?db=N` is the other common spelling of the database index.
        if parsed.database.is_none()
            && let Some(index) = parsed
                .extra_options
                .iter()
                .position(|(k, _)| k.eq_ignore_ascii_case("db"))
        {
            let (_, value) = parsed.extra_options.remove(index);
            if !value.is_empty() {
                parsed.database = Some(value);
            }
        }
    }
    Ok(parsed)
}

/// One `host[:port]` entry of a comma host list (`[v6]:port` allowed).
fn parse_one_host(entry: &str) -> Result<(String, Option<u16>), ParseUrlError> {
    let (host, port) = parse_host_port_first(entry)?;
    Ok((strip_ipv6_brackets(&host), port))
}

/// Redis-family specifics that depend on the whole host list: keep every
/// host of `redis://a,b,c` (cluster seeds, WS-B), and for
/// `redis+sentinel://s1,s2/mymaster/2?sentinelPassword=…` turn the hosts into
/// the sentinel list, the first path segment into the master name and the
/// second into the database index.
fn apply_resp_host_list(
    parsed: &mut ParsedConnectionUrl,
    hostpart: &str,
    path_part: &str,
    sentinel: bool,
) -> Result<(), ParseUrlError> {
    let entries: Vec<&str> = hostpart
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let hosts = entries
        .iter()
        .map(|e| parse_one_host(e))
        .collect::<Result<Vec<_>, _>>()?;
    if !sentinel {
        if hosts.len() > 1 {
            parsed.hosts = hosts;
        }
        return Ok(());
    }
    let segments: Vec<String> = path_part
        .split('/')
        .filter(|s| !s.is_empty())
        .map(decode)
        .collect();
    let master_name = segments.first().cloned().unwrap_or_default();
    parsed.database = segments.get(1).cloned();
    let sentinels = hosts
        .iter()
        .map(|(host, port)| {
            let port = port.unwrap_or(crate::redis_sentinel::DEFAULT_SENTINEL_PORT);
            if host.contains(':') {
                format!("[{host}]:{port}")
            } else {
                format!("{host}:{port}")
            }
        })
        .collect();
    // The form's host/port show the first sentinel (the cluster seed-list
    // joining above does not apply to a sentinel list).
    if let Some((first, _)) = hosts.first() {
        parsed.server = first.clone();
    }
    if parsed.port.is_none() && !parsed.server.is_empty() {
        parsed.port = Some(crate::redis_sentinel::DEFAULT_SENTINEL_PORT);
    }
    let mut prefer_replica_reads = false;
    let mut extra = Vec::new();
    for (key, value) in std::mem::take(&mut parsed.extra_options) {
        match key.to_ascii_lowercase().as_str() {
            "sentinelpassword" | "sentinel_password" => {
                if !value.is_empty() {
                    parsed.sentinel_password = Some(value);
                }
            }
            "preferreplica" | "prefer_replica" => prefer_replica_reads = parse_bool_value(&value),
            "readfrom" | "read_from" => {
                prefer_replica_reads = matches!(
                    value.to_ascii_lowercase().as_str(),
                    "replica" | "replicapreferred" | "replica_preferred" | "slave"
                )
            }
            _ => extra.push((key, value)),
        }
    }
    parsed.extra_options = extra;
    if hosts.len() > 1 {
        parsed.hosts = hosts;
    }
    parsed.redis_sentinel = Some(Box::new(crate::redis_sentinel::RedisSentinelConfig {
        master_name,
        sentinels,
        sentinel_password_ref: None,
        prefer_replica_reads,
    }));
    Ok(())
}

/// libSQL specifics: `libsql://` and `https://` mean TLS (Turso serves every
/// database on 443), `http://` is a plaintext self-hosted `sqld`, and the
/// `authToken` query parameter is the bearer token, which travels in the
/// password slot so the editor can push it into the secret field.
fn apply_libsql_scheme(parsed: &mut ParsedConnectionUrl, scheme: &str) {
    let tls = !scheme.eq_ignore_ascii_case("http");
    if parsed.encrypt.is_none() {
        parsed.encrypt = Some(tls);
        parsed.tls_mode = Some(if tls {
            TlsMode::VerifyFull
        } else {
            TlsMode::Disable
        });
    }
    if parsed.port.is_none() && tls {
        parsed.port = Some(443);
    }
    let mut remaining = Vec::with_capacity(parsed.extra_options.len());
    for (key, value) in parsed.extra_options.drain(..) {
        if key.eq_ignore_ascii_case("authtoken") || key.eq_ignore_ascii_case("auth_token") {
            if parsed.password.is_none() && !value.is_empty() {
                parsed.password = Some(value);
            }
        } else {
            remaining.push((key, value));
        }
    }
    parsed.extra_options = remaining;
    // libSQL has no user names: a `user@` part is not a login.
    parsed.username = None;
}

fn parse_jdbc_sqlserver(remainder: &str) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let remainder = remainder.trim_start_matches("//");
    let (authority, props) = match remainder.find(';') {
        Some(semi) => (&remainder[..semi], Some(&remainder[semi + 1..])),
        None => (remainder, None),
    };

    let authority = authority.trim();
    let (server, port) = if let Some(colon) = authority.rfind(':') {
        let maybe_port = &authority[colon + 1..];
        if maybe_port.chars().all(|c| c.is_ascii_digit()) && !maybe_port.is_empty() {
            (
                authority[..colon].to_string(),
                Some(parse_port(maybe_port)?),
            )
        } else {
            (authority.to_string(), None)
        }
    } else {
        (authority.to_string(), None)
    };

    let mut parsed = ParsedConnectionUrl {
        engine: DatabaseType::SqlServer,
        server,
        port,
        ..Default::default()
    };

    if let Some(props) = props {
        for part in props.split(';').filter(|p| !p.is_empty()) {
            let (key, value) = match part.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (part.trim(), ""),
            };
            let key_lower = key.to_ascii_lowercase();
            match key_lower.as_str() {
                "databasename" | "database" => {
                    if !value.is_empty() {
                        parsed.database = Some(value.to_string());
                    }
                }
                "user" | "username" => {
                    if !value.is_empty() {
                        parsed.username = Some(value.to_string());
                    }
                }
                "password" | "pwd" => {
                    if !value.is_empty() {
                        parsed.password = Some(value.to_string());
                    }
                }
                "integratedsecurity" => {
                    if parse_bool_value(value) {
                        parsed.integrated_security = true;
                    }
                }
                "encrypt" => parsed.encrypt = Some(parse_bool_value(value)),
                "trustservercertificate" => {
                    parsed.trust_server_certificate = Some(parse_bool_value(value));
                }
                _ => {
                    parsed
                        .extra_options
                        .push((key.to_string(), value.to_string()));
                }
            }
        }
    }

    Ok(parsed)
}

fn parse_quoted_ado_value(value: &str) -> String {
    let value = value.trim();
    if (value.starts_with('\'') && value.ends_with('\''))
        || (value.starts_with('"') && value.ends_with('"'))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn normalize_ado_server(server: &str) -> String {
    let mut s = server.trim().to_string();
    if s.to_ascii_lowercase().starts_with("tcp:") {
        s = s[4..].to_string();
    }
    let lower = s.to_ascii_lowercase();
    if lower == "(local)" || lower == "." {
        return "localhost".to_string();
    }
    if s.starts_with(".\\") || s.starts_with("./") {
        return format!("localhost{}", &s[1..]);
    }
    if lower.starts_with("(localdb)\\") {
        return s.replacen("(localdb)", "localhost", 1);
    }
    s
}

fn parse_ado_server_value(
    server: &str,
) -> Result<(String, Option<u16>, DatabaseType), ParseUrlError> {
    let server = normalize_ado_server(server);
    if let Some(comma) = server.find(',') {
        let host = server[..comma].trim().to_string();
        let port = parse_port(server[comma + 1..].trim())?;
        let engine = ado_engine_for_host(&host);
        return Ok((host, Some(port), engine));
    }
    Ok((server.clone(), None, ado_engine_for_host(&server)))
}

fn ado_engine_for_host(host: &str) -> DatabaseType {
    if host.to_ascii_lowercase().ends_with(".database.windows.net") {
        DatabaseType::AzureSqlDatabase
    } else {
        DatabaseType::SqlServer
    }
}

fn split_ado_parts(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in input.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            ';' if !in_single && !in_double => {
                if !current.trim().is_empty() {
                    parts.push(current.trim().to_string());
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        parts.push(current.trim().to_string());
    }
    parts
}

fn parse_ado_net(input: &str) -> Result<ParsedConnectionUrl, ParseUrlError> {
    let mut server = None::<String>;
    let mut port = None::<u16>;
    let mut database = None::<String>;
    let mut username = None::<String>;
    let mut password = None::<String>;
    let mut encrypt = None::<bool>;
    let mut trust = None::<bool>;
    let mut integrated = false;
    let mut engine = DatabaseType::SqlServer;
    let mut extra = Vec::new();

    for part in split_ado_parts(input) {
        let (key, value) = match part.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => continue,
        };
        let key_lower = key.to_ascii_lowercase();
        let value = parse_quoted_ado_value(value);
        match key_lower.as_str() {
            "server" | "data source" | "addr" | "address" | "network address" | "host" => {
                let (s, p, e) = parse_ado_server_value(&value)?;
                server = Some(s);
                if p.is_some() {
                    port = p;
                }
                engine = e;
            }
            "port" => port = Some(parse_port(&value)?),
            "database" | "initial catalog" => database = Some(value),
            "user id" | "uid" | "user" | "username" => username = Some(value),
            "password" | "pwd" => password = Some(value),
            "encrypt" => encrypt = Some(parse_bool_value(&value)),
            "trustservercertificate" | "trust server certificate" => {
                trust = Some(parse_bool_value(&value));
            }
            "integrated security" => {
                if parse_bool_value(&value) {
                    integrated = true;
                }
            }
            _ => extra.push((key.to_string(), value)),
        }
    }

    let server = server.ok_or(ParseUrlError::Unrecognized)?;
    Ok(ParsedConnectionUrl {
        engine,
        server,
        port,
        database,
        username,
        password,
        encrypt,
        trust_server_certificate: trust,
        tls_mode: None,
        tls_ca_file: None,
        tls_client_cert_file: None,
        tls_client_key_file: None,
        integrated_security: integrated,
        extra_options: extra,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Case {
        input: &'static str,
        expect: Result<ParsedConnectionUrl, ParseUrlError>,
    }

    fn ok(parsed: ParsedConnectionUrl) -> Result<ParsedConnectionUrl, ParseUrlError> {
        Ok(parsed)
    }

    fn err(e: ParseUrlError) -> Result<ParsedConnectionUrl, ParseUrlError> {
        Err(e)
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                input: "postgres://app:s3cret@localhost:5432/appdb?sslmode=require",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Postgres,
                    server: "localhost".into(),
                    port: Some(5432),
                    database: Some("appdb".into()),
                    username: Some("app".into()),
                    password: Some("s3cret".into()),
                    encrypt: Some(true),
                    tls_mode: Some(TlsMode::Require),
                    ..Default::default()
                }),
            },
            Case {
                input: "postgresql://app@[::1]/appdb",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Postgres,
                    server: "::1".into(),
                    database: Some("appdb".into()),
                    username: Some("app".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "postgresql://u%40corp:p%25w%3Ard@h/db",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Postgres,
                    server: "h".into(),
                    database: Some("db".into()),
                    username: Some("u@corp".into()),
                    password: Some("p%w:rd".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "postgresql://h1:5432,h2:5433/db",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Postgres,
                    server: "h1".into(),
                    port: Some(5432),
                    database: Some("db".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "postgresql://h/db?application_name=x&connect_timeout=5",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Postgres,
                    server: "h".into(),
                    database: Some("db".into()),
                    extra_options: vec![
                        ("application_name".into(), "x".into()),
                        ("connect_timeout".into(), "5".into()),
                    ],
                    ..Default::default()
                }),
            },
            Case {
                input: "clickhouse://analyst:pw@ch.example.com:8443/analytics",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::ClickHouse,
                    server: "ch.example.com".into(),
                    port: Some(8443),
                    database: Some("analytics".into()),
                    username: Some("analyst".into()),
                    password: Some("pw".into()),
                    ..Default::default()
                }),
            },
            // Turso's URL: TLS on 443 with the token in the password slot.
            Case {
                input: "libsql://shop-acme.turso.io?authToken=eyJ.abc.def",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::LibSql,
                    server: "shop-acme.turso.io".into(),
                    port: Some(443),
                    password: Some("eyJ.abc.def".into()),
                    encrypt: Some(true),
                    tls_mode: Some(TlsMode::VerifyFull),
                    ..Default::default()
                }),
            },
            Case {
                input: "https://shop-acme.turso.io",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::LibSql,
                    server: "shop-acme.turso.io".into(),
                    port: Some(443),
                    encrypt: Some(true),
                    tls_mode: Some(TlsMode::VerifyFull),
                    ..Default::default()
                }),
            },
            // A self-hosted sqld on plain HTTP keeps its port and no token.
            Case {
                input: "http://127.0.0.1:8080",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::LibSql,
                    server: "127.0.0.1".into(),
                    port: Some(8080),
                    encrypt: Some(false),
                    tls_mode: Some(TlsMode::Disable),
                    ..Default::default()
                }),
            },
            Case {
                input: "redis://app:pw@cache.example.com:6380/3",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Redis,
                    server: "cache.example.com".into(),
                    port: Some(6380),
                    database: Some("3".into()),
                    username: Some("app".into()),
                    password: Some("pw".into()),
                    encrypt: Some(false),
                    trust_server_certificate: Some(false),
                    tls_mode: Some(TlsMode::Disable),
                    ..Default::default()
                }),
            },
            Case {
                input: "rediss://:secret@cache.example.com?db=2",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Redis,
                    server: "cache.example.com".into(),
                    port: None,
                    database: Some("2".into()),
                    username: Some("".into()),
                    password: Some("secret".into()),
                    encrypt: Some(true),
                    trust_server_certificate: Some(true),
                    tls_mode: Some(TlsMode::Require),
                    ..Default::default()
                }),
            },
            Case {
                input: "valkey://valkey.internal:6379",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::Valkey,
                    server: "valkey.internal".into(),
                    port: Some(6379),
                    encrypt: Some(false),
                    trust_server_certificate: Some(false),
                    tls_mode: Some(TlsMode::Disable),
                    ..Default::default()
                }),
            },
            Case {
                input: "mysql://root@127.0.0.1:3307/shop?ssl-mode=DISABLED",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::MySql,
                    server: "127.0.0.1".into(),
                    port: Some(3307),
                    database: Some("shop".into()),
                    username: Some("root".into()),
                    encrypt: Some(false),
                    tls_mode: Some(TlsMode::Disable),
                    ..Default::default()
                }),
            },
            Case {
                input: "mariadb://h/db",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::MariaDb,
                    server: "h".into(),
                    database: Some("db".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "jdbc:mysql://[2001:db8::1]:3306/shop?user=web&password=x",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::MySql,
                    server: "2001:db8::1".into(),
                    port: Some(3306),
                    database: Some("shop".into()),
                    username: Some("web".into()),
                    password: Some("x".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "jdbc:sqlserver://db01\\SQL2022:1433;databaseName=Sales;encrypt=true;trustServerCertificate=true",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "db01\\SQL2022".into(),
                    port: Some(1433),
                    database: Some("Sales".into()),
                    encrypt: Some(true),
                    trust_server_certificate: Some(true),
                    ..Default::default()
                }),
            },
            Case {
                input: "jdbc:sqlserver://h;integratedSecurity=true",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "h".into(),
                    integrated_security: true,
                    ..Default::default()
                }),
            },
            Case {
                input: "sqlserver://sa:pw@h:1434?database=Sales",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "h".into(),
                    port: Some(1434),
                    database: Some("Sales".into()),
                    username: Some("sa".into()),
                    password: Some("pw".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "mssql://h:1433/INST/Sales",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "h\\INST".into(),
                    port: Some(1433),
                    database: Some("Sales".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "Server=tcp:acme.database.windows.net,1433;Initial Catalog=Sales;User ID=app;Password=pw;Encrypt=True;TrustServerCertificate=False;",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::AzureSqlDatabase,
                    server: "acme.database.windows.net".into(),
                    port: Some(1433),
                    database: Some("Sales".into()),
                    username: Some("app".into()),
                    password: Some("pw".into()),
                    encrypt: Some(true),
                    trust_server_certificate: Some(false),
                    ..Default::default()
                }),
            },
            Case {
                input: "Data Source=.\\SQLEXPRESS;Database=Northwind;Integrated Security=SSPI",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "localhost\\SQLEXPRESS".into(),
                    database: Some("Northwind".into()),
                    integrated_security: true,
                    ..Default::default()
                }),
            },
            Case {
                input: "Server=(local);Database=x;Uid=u;Pwd='p;w'",
                expect: ok(ParsedConnectionUrl {
                    engine: DatabaseType::SqlServer,
                    server: "localhost".into(),
                    database: Some("x".into()),
                    username: Some("u".into()),
                    password: Some("p;w".into()),
                    ..Default::default()
                }),
            },
            Case {
                input: "sqlite:///Users/me/app.db",
                expect: ok(sqlite_profile("/Users/me/app.db")),
            },
            Case {
                input: "jdbc:sqlite:/Users/me/app.db",
                expect: ok(sqlite_profile("/Users/me/app.db")),
            },
            Case {
                input: "/Users/me/app.sqlite3",
                expect: ok(sqlite_profile("/Users/me/app.sqlite3")),
            },
            Case {
                input: "C:\\data\\app.db",
                expect: ok(sqlite_profile("C:\\data\\app.db")),
            },
            Case {
                input: "sqlite::memory:",
                expect: err(ParseUrlError::Unrecognized),
            },
            Case {
                input: "duckdb:///Users/me/analytics.duckdb",
                expect: ok(duckdb_profile("/Users/me/analytics.duckdb")),
            },
            Case {
                input: "duckdb:/Users/me/analytics.duckdb",
                expect: ok(duckdb_profile("/Users/me/analytics.duckdb")),
            },
            Case {
                input: "jdbc:duckdb:/Users/me/analytics.duckdb",
                expect: ok(duckdb_profile("/Users/me/analytics.duckdb")),
            },
            Case {
                input: "duckdb::memory:",
                expect: ok(duckdb_profile(":memory:")),
            },
            Case {
                input: "/Users/me/analytics.duckdb",
                expect: ok(duckdb_profile("/Users/me/analytics.duckdb")),
            },
            Case {
                input: "C:\\data\\analytics.DDB",
                expect: ok(duckdb_profile("C:\\data\\analytics.DDB")),
            },
            Case {
                input: "file:///Users/me/analytics.duckdb",
                expect: ok(duckdb_profile("/Users/me/analytics.duckdb")),
            },
            Case {
                input: "duckdb:reports.ddb",
                expect: ok(duckdb_profile("reports.ddb")),
            },
            Case {
                input: "duckdb:",
                expect: err(ParseUrlError::Unrecognized),
            },
            Case {
                input: "jdbc:oracle:thin:@//h:1521/XE",
                expect: err(ParseUrlError::UnsupportedEngine("oracle".into())),
            },
            Case {
                input: "postgres://h:notaport/db",
                expect: err(ParseUrlError::InvalidPort("notaport".into())),
            },
            Case {
                input: "",
                expect: err(ParseUrlError::Empty),
            },
            Case {
                input: "hello world",
                expect: err(ParseUrlError::Unrecognized),
            },
        ]
    }

    #[test]
    fn url_sslmode_verify_ca_sets_mode() {
        let parsed = parse_connection_url("postgres://app@h:5432/db?sslmode=verify-ca").unwrap();
        assert_eq!(parsed.tls_mode, Some(TlsMode::VerifyCa));
        // Older readers still see the encrypt flag.
        assert_eq!(parsed.encrypt, Some(true));

        let parsed = parse_connection_url("postgres://app@h/db?sslmode=verify-full").unwrap();
        assert_eq!(parsed.tls_mode, Some(TlsMode::VerifyFull));

        // MySQL spelling, including VERIFY_IDENTITY (its verify-full).
        let parsed = parse_connection_url("mysql://root@h/db?ssl-mode=VERIFY_CA").unwrap();
        assert_eq!(parsed.tls_mode, Some(TlsMode::VerifyCa));
        let parsed = parse_connection_url("mysql://root@h/db?ssl-mode=VERIFY_IDENTITY").unwrap();
        assert_eq!(parsed.tls_mode, Some(TlsMode::VerifyFull));
    }

    #[test]
    fn url_sslnegotiation_sets_direct_tls() {
        let parsed =
            parse_connection_url("postgres://app@h/db?sslmode=require&sslnegotiation=direct")
                .unwrap();
        assert_eq!(parsed.tls_direct, Some(true));
        assert!(
            parsed.extra_options.is_empty(),
            "{:?}",
            parsed.extra_options
        );
        let parsed = parse_connection_url("postgres://app@h/db?sslnegotiation=postgres").unwrap();
        assert_eq!(parsed.tls_direct, Some(false));
        let parsed = parse_connection_url("postgres://app@h/db").unwrap();
        assert_eq!(parsed.tls_direct, None);
    }

    #[test]
    fn url_sslrootcert_sets_ca_path() {
        let parsed = parse_connection_url(
            "postgres://app@h/db?sslmode=verify-full&sslrootcert=/etc/ssl/ca.pem&sslcert=/c.pem&sslkey=/k.pem",
        )
        .unwrap();
        assert_eq!(parsed.tls_ca_file, Some(PathBuf::from("/etc/ssl/ca.pem")));
        assert_eq!(parsed.tls_client_cert_file, Some(PathBuf::from("/c.pem")));
        assert_eq!(parsed.tls_client_key_file, Some(PathBuf::from("/k.pem")));

        let parsed = parse_connection_url(
            "mysql://root@h/db?ssl-ca=/ca.pem&ssl-cert=/cc.pem&ssl-key=/ck.pem",
        )
        .unwrap();
        assert_eq!(parsed.tls_ca_file, Some(PathBuf::from("/ca.pem")));
        assert_eq!(parsed.tls_client_cert_file, Some(PathBuf::from("/cc.pem")));
        assert_eq!(parsed.tls_client_key_file, Some(PathBuf::from("/ck.pem")));
    }

    #[test]
    fn table_driven_parse_cases() {
        for case in cases() {
            let got = parse_connection_url(case.input);
            assert_eq!(got, case.expect, "input: {}", case.input);
        }
    }

    #[test]
    fn ok_cases_serialize_without_password_substrings() {
        for case in cases() {
            let Ok(mut parsed) = parse_connection_url(case.input) else {
                continue;
            };
            let password = parsed.password.take();
            let probe = serde_json::json!({
                "engine": parsed.engine,
                "server": parsed.server,
                "port": parsed.port,
                "database": parsed.database,
                "username": parsed.username,
                "encrypt": parsed.encrypt,
                "trust": parsed.trust_server_certificate,
            });
            let json = serde_json::to_string(&probe).unwrap();
            if let Some(pw) = password {
                assert!(
                    !json.contains(&pw),
                    "password leaked into serializable fields for {}",
                    case.input
                );
            }
            assert!(!json.contains("\"password\""));
        }
    }

    #[test]
    fn redis_sentinel_url_names_master_sentinels_and_database() {
        let parsed = parse_connection_url(
            "redis+sentinel://app:pw@10.0.0.1:26379,10.0.0.2,[::1]:26381/mymaster/2?sentinelPassword=s%40fe&preferReplica=true",
        )
        .unwrap();
        assert_eq!(parsed.engine, DatabaseType::Redis);
        assert_eq!(parsed.server, "10.0.0.1");
        assert_eq!(parsed.port, Some(26379));
        assert_eq!(parsed.database.as_deref(), Some("2"));
        assert_eq!(parsed.username.as_deref(), Some("app"));
        assert_eq!(parsed.password.as_deref(), Some("pw"));
        assert_eq!(parsed.sentinel_password.as_deref(), Some("s@fe"));
        assert_eq!(parsed.tls_mode, Some(TlsMode::Disable));
        assert!(
            parsed.extra_options.is_empty(),
            "{:?}",
            parsed.extra_options
        );
        let sentinel = parsed.redis_sentinel.expect("sentinel config");
        assert_eq!(sentinel.master_name, "mymaster");
        assert_eq!(
            sentinel.sentinels,
            vec![
                "10.0.0.1:26379".to_string(),
                "10.0.0.2:26379".to_string(),
                "[::1]:26381".to_string(),
            ]
        );
        assert!(sentinel.prefer_replica_reads);
        assert!(sentinel.sentinel_password_ref.is_none());
        assert_eq!(
            sentinel.sentinel_addresses().unwrap()[2],
            ("::1".to_string(), 26381)
        );
    }

    #[test]
    fn redis_sentinel_url_tls_and_defaults() {
        let parsed = parse_connection_url("rediss+sentinel://sentinel.internal/cache").unwrap();
        assert_eq!(parsed.tls_mode, Some(TlsMode::Require));
        assert_eq!(parsed.port, Some(26379));
        assert_eq!(parsed.database, None);
        let sentinel = parsed.redis_sentinel.unwrap();
        assert_eq!(sentinel.master_name, "cache");
        assert_eq!(
            sentinel.sentinels,
            vec!["sentinel.internal:26379".to_string()]
        );
        assert!(!sentinel.prefer_replica_reads);
        let valkey =
            parse_connection_url("valkey+sentinel://s1:5000/m?readFrom=replica&db=4").unwrap();
        assert_eq!(valkey.engine, DatabaseType::Valkey);
        assert_eq!(valkey.database.as_deref(), Some("4"));
        assert!(valkey.redis_sentinel.unwrap().prefer_replica_reads);
    }

    #[test]
    fn redis_url_with_three_hosts_keeps_all_three() {
        let parsed = parse_connection_url("redis://:pw@node-a:7000,node-b:7001,node-c/0").unwrap();
        // The server field carries the whole seed list for cluster mode.
        assert_eq!(parsed.server, "node-a,node-b:7001,node-c");
        assert_eq!(parsed.port, Some(7000));
        assert_eq!(
            parsed.hosts,
            vec![
                ("node-a".to_string(), Some(7000)),
                ("node-b".to_string(), Some(7001)),
                ("node-c".to_string(), None),
            ]
        );
        assert!(parsed.redis_sentinel.is_none());
        // A single host leaves the list empty; other engines never fill it.
        assert!(
            parse_connection_url("redis://one:6379")
                .unwrap()
                .hosts
                .is_empty()
        );
        assert!(
            parse_connection_url("postgres://a:5432,b:5433/db")
                .unwrap()
                .hosts
                .is_empty()
        );
    }

    #[test]
    fn debug_output_masks_passwords() {
        let parsed = parse_connection_url(
            "redis+sentinel://:hunter2@s1/mymaster?sentinelPassword=topsecret",
        )
        .unwrap();
        let debug = format!("{parsed:?}");
        assert!(
            !debug.contains("hunter2") && !debug.contains("topsecret"),
            "{debug}"
        );
        assert!(debug.contains("<redacted>"), "{debug}");
        let deep_link = crate::deep_link::parse_deep_link("mysql://u:s3cret@h/db").unwrap();
        assert!(!format!("{deep_link:?}").contains("s3cret"));
    }

    #[test]
    fn redis_sentinel_password_never_serializes_into_the_config() {
        let parsed =
            parse_connection_url("redis+sentinel://s1/mymaster?sentinelPassword=topsecret")
                .unwrap();
        let json = serde_json::to_string(&parsed.redis_sentinel).unwrap();
        assert!(!json.contains("topsecret"), "{json}");
    }

    #[test]
    fn redis_cluster_url_keeps_every_seed_host() {
        let parsed =
            parse_connection_url("redis://app:pw@10.0.0.1:7000,10.0.0.2:7001,10.0.0.3/0").unwrap();
        assert_eq!(parsed.engine, DatabaseType::Redis);
        assert_eq!(parsed.server, "10.0.0.1,10.0.0.2:7001,10.0.0.3");
        assert_eq!(parsed.port, Some(7000));
        assert_eq!(parsed.database.as_deref(), Some("0"));
        let mut profile = crate::ConnectionProfile::sql_login(
            "c",
            &parsed.server,
            "app",
            crate::SecretRef {
                provider: crate::SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.port = parsed.port.unwrap();
        assert_eq!(
            crate::redis_cluster::seed_nodes(&profile),
            vec![
                ("10.0.0.1".to_string(), 7000),
                ("10.0.0.2".to_string(), 7001),
                ("10.0.0.3".to_string(), 7000),
            ]
        );
        // Other engines still take the first host only.
        let pg = parse_connection_url("postgres://h1:5432,h2:5433/db").unwrap();
        assert_eq!(pg.server, "h1");
    }
}
