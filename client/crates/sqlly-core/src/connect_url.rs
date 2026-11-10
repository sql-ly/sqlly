//! A raw connection URL handed to the app (`mysql://user:pw@host/db` as a
//! startup argument or OS open request, Gridex #18): which arguments are
//! one, how to strip the password before it leaves the process, which saved
//! connection it names, and the unsaved profile to use when none matches.
//!
//! Pure: no file system, keyring, or network access.

use crate::connection::ConnectionProfile;
use crate::connection_url::ParsedConnectionUrl;
use sqlly_lang::sqlmodel::DatabaseType;

/// URL schemes accepted as a raw connection URL. File engines (`sqlite:`,
/// `duckdb:`, `file://`) arrive as paths instead, and plain `http(s)://`
/// is a web address far more often than a libSQL endpoint.
pub const RAW_CONNECTION_SCHEMES: &[&str] = &[
    "postgres",
    "postgresql",
    "mysql",
    "mariadb",
    "sqlserver",
    "mssql",
    "clickhouse",
    "libsql",
    "redis",
    "rediss",
    "redis+sentinel",
    "rediss+sentinel",
    "valkey",
    "valkeys",
    "valkey+sentinel",
    "valkeys+sentinel",
];

/// Longest raw URL accepted; anything longer is not a connection string.
pub const MAX_RAW_URL_BYTES: usize = 8 * 1024;

/// Query parameters whose value is a secret.
const SECRET_PARAMS: &[&str] = &[
    "password",
    "pwd",
    "sentinelpassword",
    "sentinel_password",
    "authtoken",
    "auth_token",
];

/// The `scheme://` of `input` (after an optional `jdbc:` prefix) when it is
/// one of [`RAW_CONNECTION_SCHEMES`].
fn raw_scheme(input: &str) -> Option<&str> {
    let input = input.trim();
    let body = match input.get(..5) {
        Some(prefix) if prefix.eq_ignore_ascii_case("jdbc:") => &input[5..],
        _ => input,
    };
    let (scheme, _) = body.split_once("://")?;
    RAW_CONNECTION_SCHEMES
        .iter()
        .any(|known| known.eq_ignore_ascii_case(scheme))
        .then_some(scheme)
}

/// Whether `input` is a raw connection URL (see [`RAW_CONNECTION_SCHEMES`]).
pub fn is_raw_connection_uri(input: &str) -> bool {
    raw_scheme(input).is_some()
}

/// `input` with its password removed: the `:password` of the user info and
/// the value of any secret query parameter (`password=`, `sentinelPassword=`,
/// `authToken=`, …). Used before a URL is written anywhere (the second
/// instance hand-off) or shown.
pub fn redact_connection_uri(input: &str) -> String {
    let input = input.trim();
    let Some(scheme_end) = input.find("://") else {
        return input.to_string();
    };
    let (head, rest) = input.split_at(scheme_end + 3);
    let (query_start, query) = match rest.find('?') {
        Some(q) => (q, Some(&rest[q + 1..])),
        None => (rest.len(), None),
    };
    let before_query = &rest[..query_start];
    // Split at the last `@`: an unencoded `@` inside the password must not
    // leave the rest of the password behind as part of the "host".
    let authority = match before_query.rfind('@') {
        Some(at) => {
            let userinfo = &before_query[..at];
            let user = userinfo.split(':').next().unwrap_or_default();
            format!("{user}{}", &before_query[at..])
        }
        None => before_query.to_string(),
    };
    let mut out = format!("{head}{authority}");
    if let Some(query) = query {
        let pairs: Vec<String> = query
            .split('&')
            .filter(|pair| {
                let key = pair.split('=').next().unwrap_or_default();
                !SECRET_PARAMS
                    .iter()
                    .any(|secret| secret.eq_ignore_ascii_case(key))
            })
            .map(str::to_string)
            .collect();
        if !pairs.is_empty() {
            out.push('?');
            out.push_str(&pairs.join("&"));
        }
    }
    out
}

/// `user@host:port/database` for display and as the unsaved connection's
/// name; never includes the password.
pub fn connection_url_label(parsed: &ParsedConnectionUrl) -> String {
    let mut label = String::new();
    if let Some(user) = parsed.username.as_deref().filter(|u| !u.is_empty()) {
        label.push_str(user);
        label.push('@');
    }
    if parsed.server.contains(':') && !parsed.server.starts_with('[') {
        label.push_str(&format!("[{}]", parsed.server));
    } else {
        label.push_str(&parsed.server);
    }
    if let Some(port) = parsed.port {
        label.push_str(&format!(":{port}"));
    }
    if let Some(database) = parsed.database.as_deref().filter(|d| !d.is_empty()) {
        label.push('/');
        label.push_str(database);
    }
    label
}

/// Engines that answer the same URL scheme family: a `mysql://` URL may
/// name a saved MariaDB connection, a `redis://` one a saved Valkey.
fn same_engine_family(a: DatabaseType, b: DatabaseType) -> bool {
    let base = |engine: DatabaseType| match engine.compatible_with().unwrap_or(engine) {
        DatabaseType::MariaDb => DatabaseType::MySql,
        other => other,
    };
    base(a) == base(b)
}

/// The saved connection `parsed` points at: same engine family, host
/// (case-insensitive), and port (the engine default when either leaves it
/// out); the same user when the URL names one; and the same database when
/// both name one. A profile whose database matches exactly wins over one
/// with none.
pub fn find_matching_profile<'a>(
    parsed: &ParsedConnectionUrl,
    profiles: &'a [ConnectionProfile],
) -> Option<&'a ConnectionProfile> {
    let default_port = crate::credential_proxy::default_port(parsed.engine);
    let url_port = parsed.port.unwrap_or(default_port);
    let url_user = parsed.username.as_deref().filter(|u| !u.is_empty());
    let url_database = parsed.database.as_deref().filter(|d| !d.is_empty());
    let candidates = profiles.iter().filter(|profile| {
        let profile_port = if profile.port == 0 {
            crate::credential_proxy::default_port(profile.database_type)
        } else {
            profile.port
        };
        let profile_database = profile.database.as_deref().filter(|d| !d.is_empty());
        same_engine_family(profile.database_type, parsed.engine)
            && profile
                .server
                .trim()
                .eq_ignore_ascii_case(parsed.server.trim())
            && profile_port == url_port
            && url_user.is_none_or(|user| profile.authentication.username() == Some(user))
            && match (url_database, profile_database) {
                (Some(url), Some(saved)) => url == saved,
                _ => true,
            }
    });
    let mut fallback = None;
    for profile in candidates {
        let profile_database = profile.database.as_deref().filter(|d| !d.is_empty());
        if url_database.is_some() && profile_database == url_database {
            return Some(profile);
        }
        fallback.get_or_insert(profile);
    }
    fallback
}

/// The unsaved profile for `parsed`, named by [`connection_url_label`].
/// Secrets stay `InMemory`; the caller must not persist it.
pub fn unsaved_profile(parsed: &ParsedConnectionUrl) -> Result<ConnectionProfile, String> {
    crate::provisioning::profile_from_parsed_url(&connection_url_label(parsed), parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{AuthenticationType, SecretProvider, SecretRef};
    use crate::connection_url::parse_connection_url;

    fn saved(
        name: &str,
        engine: DatabaseType,
        server: &str,
        port: u16,
        user: &str,
    ) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            server,
            user,
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: format!("credential.{name}"),
            },
        );
        profile.database_type = engine;
        profile.port = port;
        profile
    }

    #[test]
    fn recognises_raw_connection_schemes_only() {
        for url in [
            "mysql://user:pw@server:3306/db",
            "MariaDB://server",
            "postgres://u@h/db",
            "postgresql://h",
            "sqlserver://h:1433",
            "jdbc:mysql://h/db",
            "clickhouse://h:8123",
            "redis://h:6379/0",
            "rediss+sentinel://s1/mymaster",
            "valkey://h",
            "libsql://db.turso.io",
        ] {
            assert!(is_raw_connection_uri(url), "{url}");
        }
        for not_url in [
            "sqlly://open?connection=x",
            "file:///tmp/a.sqlite",
            "https://example.com",
            "/tmp/a.db",
            "mysql:",
            "ftp://h",
            "--some-flag",
        ] {
            assert!(!is_raw_connection_uri(not_url), "{not_url}");
        }
    }

    #[test]
    fn redaction_drops_every_password_form() {
        assert_eq!(
            redact_connection_uri("mysql://user:s3cret@server:3306/database"),
            "mysql://user@server:3306/database"
        );
        assert_eq!(
            redact_connection_uri(
                "postgres://u:pw@h/db?sslmode=require&password=x&application_name=a"
            ),
            "postgres://u@h/db?sslmode=require&application_name=a"
        );
        assert_eq!(
            redact_connection_uri("redis+sentinel://:pw@s1/mymaster?sentinelPassword=topsecret"),
            "redis+sentinel://@s1/mymaster"
        );
        assert_eq!(
            redact_connection_uri("libsql://db.turso.io?authToken=abc"),
            "libsql://db.turso.io"
        );
        assert_eq!(
            redact_connection_uri("mysql://server/db"),
            "mysql://server/db"
        );
        assert_eq!(
            redact_connection_uri("mysql://user:p%40ss@server/db"),
            "mysql://user@server/db"
        );
        // An unencoded `@` inside the password leaves none of it behind.
        assert_eq!(
            redact_connection_uri("mysql://user:p@ss@server/db"),
            "mysql://user@server/db"
        );
    }

    #[test]
    fn label_never_shows_the_password() {
        let parsed = parse_connection_url("mysql://user:s3cret@server:3306/database").unwrap();
        assert_eq!(connection_url_label(&parsed), "user@server:3306/database");
        let parsed = parse_connection_url("postgres://h").unwrap();
        assert_eq!(connection_url_label(&parsed), "h");
    }

    #[test]
    fn matches_saved_connection_by_host_port_user_and_database() {
        let mut with_db = saved("orders", DatabaseType::MySql, "Server", 3306, "app");
        with_db.database = Some("orders".into());
        let other_user = saved("admin", DatabaseType::MySql, "server", 3306, "root");
        let no_db = saved("app any", DatabaseType::MariaDb, "server", 0, "app");
        let profiles = vec![other_user.clone(), no_db.clone(), with_db.clone()];

        let parsed = parse_connection_url("mysql://app:pw@server:3306/orders").unwrap();
        assert_eq!(find_matching_profile(&parsed, &profiles), Some(&with_db));

        // Another database: the saved one without a database still fits.
        let parsed = parse_connection_url("mysql://app@server/billing").unwrap();
        assert_eq!(find_matching_profile(&parsed, &profiles), Some(&no_db));

        let parsed = parse_connection_url("mysql://root@server").unwrap();
        assert_eq!(find_matching_profile(&parsed, &profiles), Some(&other_user));

        for miss in [
            "mysql://app@server:3307/orders",
            "mysql://nobody@server/orders",
            "mysql://app@elsewhere/orders",
            "postgres://app@server:3306/orders",
        ] {
            let parsed = parse_connection_url(miss).unwrap();
            assert_eq!(find_matching_profile(&parsed, &profiles), None, "{miss}");
        }
    }

    #[test]
    fn unsaved_profile_carries_the_url_and_keeps_the_password_in_memory() {
        let parsed = parse_connection_url("mysql://user:s3cret@server:3307/database").unwrap();
        let profile = unsaved_profile(&parsed).unwrap();
        assert_eq!(profile.name, "user@server:3307/database");
        assert_eq!(profile.database_type, DatabaseType::MySql);
        assert_eq!(profile.server, "server");
        assert_eq!(profile.port, 3307);
        assert_eq!(profile.database.as_deref(), Some("database"));
        assert_eq!(profile.provenance, crate::ConnectionProvenance::Manual);
        match &profile.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "user");
                assert_eq!(secret_ref.provider, SecretProvider::InMemory);
                assert_eq!(secret_ref.key, "s3cret");
            }
            other => panic!("expected a SQL login, got {other:?}"),
        }

        // No password and no port: the engine default and an empty secret.
        let parsed = parse_connection_url("postgres://app@db.internal/sales").unwrap();
        let profile = unsaved_profile(&parsed).unwrap();
        assert_eq!(profile.port, 5432);
        match &profile.authentication {
            AuthenticationType::SqlLogin { secret_ref, .. } => {
                assert_eq!(secret_ref.provider, SecretProvider::InMemory);
                assert!(secret_ref.key.is_empty());
            }
            other => panic!("expected a SQL login, got {other:?}"),
        }
        // Two unsaved profiles never share an id.
        assert_ne!(profile.id, unsaved_profile(&parsed).unwrap().id);
    }
}
