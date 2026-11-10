//! Credential-file-backed connection catalog for the relay's browser proxy.
//!
//! The relay's proxy listener and the `sqlly-cli engine` subprocesses it
//! spawns both read the SAME [credential file](crate::credential_import) and
//! must agree on every connection's identity, so this module is the one
//! place that turns a parsed [`CredentialImportPlan`] into connect-ready
//! profiles:
//!
//! * shared logins (`sql_account`) and tunnel profiles are resolved **within
//!   the file** (there is no saved store on a headless host);
//! * every profile gets a **deterministic id** derived from its position and
//!   name ([`proxy_profile_id`]), so the listing the relay sends to a browser
//!   names exactly the profile the engine will resolve;
//! * secrets stay `InMemory` clear text on the host, and
//!   [`redact_for_client`] produces the secret-free copy that leaves it.
//!
//! Pure and IO-free, so it compiles on every target including wasm (the
//! browser deserializes the redacted listing with the same types).

use serde::{Deserialize, Serialize};
use sqlly_lang::sqlmodel::DatabaseType;
use uuid::Uuid;

use crate::connection::{
    ConnectionId, ConnectionProfile, ConnectionProvenance, ConnectionRoute, SecretProvider,
    SecretRef, TransportChain,
};
use crate::credential_import::{
    CredentialImportError, CredentialImportPlan, CredentialSafety, parse_credential_file,
};

/// UUIDv5 namespace for proxy profile ids. Never change it: ids derived from
/// it are how a browser session names a connection to the engine.
pub const PROXY_PROFILE_NAMESPACE: Uuid = Uuid::from_bytes([
    0x9c, 0x41, 0x7e, 0x2a, 0x5b, 0x0d, 0x4f, 0x93, 0x8a, 0x6e, 0x21, 0xd3, 0x7f, 0x55, 0x0c, 0x18,
]);

/// A client / project / environment label from the credential file, minus
/// its icon bytes (icons can be up to 1 MB each and the browser has nowhere
/// to keep them).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyLabel {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub safety: CredentialSafety,
}

/// Every connection a proxy host can serve, with labels.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyCatalog {
    /// Connect-ready profiles with `InMemory` clear-text secrets. Never send
    /// these off the host: use [`ProxyCatalog::client_listing`].
    pub profiles: Vec<ConnectionProfile>,
    #[serde(default)]
    pub clients: Vec<ProxyLabel>,
    #[serde(default)]
    pub projects: Vec<ProxyLabel>,
    #[serde(default)]
    pub environments: Vec<ProxyLabel>,
}

impl ProxyCatalog {
    /// Parse a credential file and resolve it into a catalog.
    pub fn from_credential_text(text: &str) -> Result<Self, CredentialImportError> {
        let plan = parse_credential_file(text, default_port)?;
        Self::from_plan(&plan)
    }

    /// Resolve an already-parsed plan. Shared-login and tunnel-profile names
    /// must point at entries in the same file; every dangling reference is
    /// reported and nothing is returned.
    pub fn from_plan(plan: &CredentialImportPlan) -> Result<Self, CredentialImportError> {
        let mut problems = Vec::new();
        let mut profiles = Vec::with_capacity(plan.connections.len());
        for (index, planned) in plan.connections.iter().enumerate() {
            let at = format!("connections[{index}] \"{}\"", planned.profile.name);
            let mut profile = planned.profile.clone();

            if let Some(name) = &planned.sql_account {
                match plan
                    .sql_auth_accounts
                    .iter()
                    .find(|a| a.name.eq_ignore_ascii_case(name))
                {
                    Some(account) => {
                        profile.authentication = crate::connection::AuthenticationType::SqlLogin {
                            username: account.username.clone(),
                            secret_ref: SecretRef {
                                provider: SecretProvider::InMemory,
                                key: account.password.clone().unwrap_or_default(),
                            },
                        };
                    }
                    None => problems.push(format!("{at}: no shared login named \"{name}\"")),
                }
            }

            if let Some(name) = &planned.tunnel_profile {
                match plan
                    .tunnel_profiles
                    .iter()
                    .find(|t| t.name.eq_ignore_ascii_case(name))
                {
                    Some(tunnel) => {
                        profile.transport = tunnel.chain.clone();
                    }
                    None => problems.push(format!("{at}: no tunnel profile named \"{name}\"")),
                }
            }
            profile.tunnel_profile = None;
            profile.ssh_tunnel = profile.transport.ssh.clone();

            // The proxy host dials every server itself; a `via-relay` route in
            // the file describes how a desktop app would reach it, not how the
            // host does.
            profile.route = ConnectionRoute::Direct;
            profile.provenance = ConnectionProvenance::Proxied;
            profile.id = proxy_profile_id(index, &profile.name);
            profiles.push(profile);
        }

        if !problems.is_empty() {
            return Err(CredentialImportError { problems });
        }

        let labels = |entities: &[crate::credential_import::PlannedEntity]| {
            entities
                .iter()
                .map(|e| ProxyLabel {
                    name: e.name.clone(),
                    color_hex: e.color_hex.clone(),
                    safety: e.safety.clone(),
                })
                .collect()
        };
        Ok(Self {
            profiles,
            clients: labels(&plan.clients),
            projects: labels(&plan.projects),
            environments: labels(&plan.environments),
        })
    }

    /// The profile a browser session named, if the file defines it.
    pub fn find(&self, id: ConnectionId) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// The copy that leaves the host: same ids, names, engines, servers,
    /// databases, labels, and safety flags, but no secret, tunnel, or token.
    pub fn client_listing(&self) -> Self {
        Self {
            profiles: self.profiles.iter().map(redact_for_client).collect(),
            clients: self.clients.clone(),
            projects: self.projects.clone(),
            environments: self.environments.clone(),
        }
    }
}

/// Version of the browser ↔ proxy WebSocket contract carried in
/// [`ProxyServerMessage::Hello`]. Bump when the framing or the hello shape
/// changes incompatibly.
///
/// * 1 — hello + opaque engine frames.
/// * 2 — the hello may carry a [`ProxyLicense`] and the proxy may push
///   [`ProxyServerMessage::License`] when that state changes (SQLly
///   Embedded). The framing is unchanged, so a v2 client accepts a v1 hello.
pub const PROXY_PROTOCOL_VERSION: u32 = 2;

/// The oldest hello a client still understands (see
/// [`PROXY_PROTOCOL_VERSION`]).
pub const PROXY_PROTOCOL_MIN_SUPPORTED: u32 = 1;

/// Where an embedded proxy stands with its SQLly Embedded license. Anything
/// but [`Ok`](Self::Ok) means the proxy answers every request with a
/// failure naming the problem until the license recovers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyLicenseState {
    /// Verified with sqlly.app recently; queries run.
    Ok,
    /// The license period ended less than 15 days ago: queries still run,
    /// and every client sees the renewal countdown.
    Grace,
    /// No successful check-in with sqlly.app for too long (40 minutes).
    Stale,
    /// sqlly.app refused this bridge instance: another bridge holds the
    /// license's instance slot.
    NotAdmitted,
    /// The license period is over.
    Expired,
    /// The key was regenerated or the license revoked.
    Revoked,
    /// The bridge was started for a domain the license is not bound to.
    DomainMismatch,
    /// The bridge has no license yet: it waits for its administrator to
    /// link it to a sqlly.app account.
    Unlicensed,
}

impl ProxyLicenseState {
    /// Whether the license is fully in order (no message for the user).
    #[must_use]
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Whether the proxy serves requests in this state: [`Ok`](Self::Ok)
    /// and [`Grace`](Self::Grace) do; everything else is refused.
    #[must_use]
    pub fn serves(self) -> bool {
        matches!(self, Self::Ok | Self::Grace)
    }

    /// The wire / log spelling (`snake_case`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Grace => "grace",
            Self::Stale => "stale",
            Self::NotAdmitted => "not_admitted",
            Self::Expired => "expired",
            Self::Revoked => "revoked",
            Self::DomainMismatch => "domain_mismatch",
            Self::Unlicensed => "unlicensed",
        }
    }
}

/// The license summary an embedded proxy shares with its browsers: enough
/// for the app to tell the user why work is refused, never the key or the
/// account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyLicense {
    pub state: ProxyLicenseState,
    /// The plan's display name (`SQLly Embedded 25`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// A sentence for the user when `state` is not ok.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The sentence every client sees while the bridge is not linked to a
/// license.
pub const PROXY_LICENSE_UNLICENSED_MESSAGE: &str = "SQLly Embedded is not linked to a license yet. Ask your administrator to connect it to sqlly.app.";

/// The sentence every client sees once the grace period is over.
pub const PROXY_LICENSE_EXPIRED_MESSAGE: &str =
    "The SQLly Embedded license for this site has expired. Ask your administrator to renew it.";

impl ProxyLicense {
    /// Whether the license is fully in order (no message for the user).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.state.is_ok()
    }

    /// Whether requests are served under this license (ok or in grace).
    #[must_use]
    pub fn serves(&self) -> bool {
        self.state.serves()
    }

    pub fn user_display_message(&self) -> crate::DisplayMessage {
        use crate::DisplayMessage;
        if let Some(message) = self.message.as_deref().filter(|m| !m.trim().is_empty()) {
            return DisplayMessage::raw(message);
        }
        let (key, fallback) = match self.state {
            ProxyLicenseState::Ok => (
                "services.embedded.license.active",
                "SQLly Embedded license is active.",
            ),
            ProxyLicenseState::Grace => (
                "services.embedded.license.grace",
                "The SQLly Embedded license for this site expires soon. Ask your administrator to renew it.",
            ),
            ProxyLicenseState::Expired => (
                "services.embedded.license.expired",
                PROXY_LICENSE_EXPIRED_MESSAGE,
            ),
            ProxyLicenseState::Stale => (
                "services.embedded.license.stale",
                "SQLly Embedded could not confirm its license with sqlly.app for 40 minutes; ask your administrator to check in.",
            ),
            ProxyLicenseState::NotAdmitted => (
                "services.embedded.license.not_admitted",
                "Another bridge holds this SQLly Embedded license's instance slot.",
            ),
            ProxyLicenseState::Revoked => (
                "services.embedded.license.revoked",
                "The SQLly Embedded license key is no longer valid.",
            ),
            ProxyLicenseState::DomainMismatch => (
                "services.embedded.license.domain_mismatch",
                "This bridge is configured for a domain its SQLly Embedded license is not bound to.",
            ),
            ProxyLicenseState::Unlicensed => (
                "services.embedded.license.unlicensed",
                PROXY_LICENSE_UNLICENSED_MESSAGE,
            ),
        };
        DisplayMessage::new(key, fallback)
    }

    /// English compatibility fallback; localized callers use
    /// [`Self::user_display_message`].
    #[must_use]
    pub fn user_message(&self) -> String {
        self.user_display_message().into_fallback()
    }
}

/// The JSON text messages a proxy sends over its WebSocket. Everything else
/// on the socket is a binary engine frame (`[tag][body]`, one per message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyServerMessage {
    /// First message on every accepted socket: what this proxy serves.
    Hello {
        protocol: u32,
        /// A label for the status bar / connection manager banner.
        relay: String,
        /// The secret-free listing ([`ProxyCatalog::client_listing`]).
        catalog: ProxyCatalog,
        /// The embedded license state (protocol 2, SQLly Embedded only).
        /// Absent from a plain credential-file proxy.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        license: Option<ProxyLicense>,
        /// Tabs to open (SQLly Embedded launch context): what the host app
        /// asked for when it minted the session's launch token. Sent with the
        /// first session a token admits only; absent otherwise. Clients that
        /// predate it ignore the field.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        open: Vec<LaunchOpen>,
    },
    /// Pushed by an embedded proxy whenever its license state changes
    /// (protocol 2). The socket stays open; requests are refused with a
    /// failure frame while the state is not ok and resume when it is.
    License {
        state: ProxyLicenseState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Sent before the proxy closes the socket on a failure of its own (engine
    /// spawn failure, credential file unreadable). A refused token or a full
    /// proxy closes with a WebSocket close reason instead.
    Error { message: String },
}

impl ProxyServerMessage {
    /// The [`License`](Self::License) message for a license snapshot.
    #[must_use]
    pub fn license(license: &ProxyLicense) -> Self {
        Self::License {
            state: license.state,
            plan: license.plan.clone(),
            message: license.message.clone(),
        }
    }
}

/// The most tabs one launch may open.
pub const LAUNCH_MAX_TABS: usize = 8;
/// The largest script one launch tab may carry, in bytes.
pub const LAUNCH_MAX_SCRIPT_BYTES: usize = 256 * 1024;
/// The most parameters one launch tab may carry.
pub const LAUNCH_MAX_PARAMETERS: usize = 64;
/// The longest parameter value, in bytes.
pub const LAUNCH_MAX_VALUE_BYTES: usize = 64 * 1024;
/// The longest tab title, in characters.
pub const LAUNCH_MAX_TITLE_CHARS: usize = 120;

/// The parameter types a launch may declare: the canonical spellings of
/// SQLly's `-- sqlly param:` types, so the strip in the browser accepts every
/// one of them.
pub const LAUNCH_PARAMETER_TYPES: &[&str] = &[
    "text", "int", "decimal", "bool", "date", "datetime", "time", "uuid", "json", "binary",
];

/// One query tab a SQLly Embedded launch asks the browser to open: the
/// script, the connection it runs on, its parameter values and the cursor.
/// The bridge validates and resolves it when the token is minted, so every
/// field here is already known to be good; the browser never runs it on its
/// own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOpen {
    /// The catalog id of the connection the tab is bound to (resolved from
    /// the name or id the host gave). `None` opens the tab unbound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_id: Option<ConnectionId>,
    /// The connection's display name, for messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection: Option<String>,
    /// A database to switch to instead of the connection's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// The tab title; `None` keeps SQLly's own naming.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The SQL to load, verbatim.
    #[serde(default)]
    pub script: String,
    /// Values for the script's `@name` parameters.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<LaunchParameter>,
    /// Where to put the caret in `script`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<LaunchCursor>,
}

/// A launch parameter: a name SQLly declares and the value its strip shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchParameter {
    /// With the leading `@`.
    pub name: String,
    /// One of [`LAUNCH_PARAMETER_TYPES`].
    #[serde(rename = "type")]
    pub ty: String,
    /// The value as text; `None` leaves the input empty for the user.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// A 1-based caret position in a launch script.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchCursor {
    pub line: u32,
    pub column: u32,
}

impl LaunchCursor {
    /// The byte offset of this position in `text`: the line clamped to the
    /// document, the column (counted in characters) clamped to that line.
    #[must_use]
    pub fn offset_in(self, text: &str) -> usize {
        let target = self.line.max(1) as usize - 1;
        let mut start = 0;
        for (index, line) in text.split('\n').enumerate() {
            if index == target {
                let column = self.column.max(1) as usize - 1;
                return start
                    + line
                        .char_indices()
                        .nth(column)
                        .map_or(line.len(), |(at, _)| at);
            }
            start += line.len() + 1;
        }
        text.len()
    }
}

/// The deterministic id of the `index`-th connection named `name` in a
/// credential file.
pub fn proxy_profile_id(index: usize, name: &str) -> ConnectionId {
    let seed = format!("{index}\u{0}{name}");
    ConnectionId(Uuid::new_v5(&PROXY_PROFILE_NAMESPACE, seed.as_bytes()))
}

/// A secret-free copy of `profile` for the browser: the SQL password / token
/// becomes an empty in-memory reference, and the transport chain (which
/// carries SSH keys and proxy passwords, and which the browser never dials)
/// is dropped along with any runtime tokens.
pub fn redact_for_client(profile: &ConnectionProfile) -> ConnectionProfile {
    let mut copy = profile.clone();
    if let Some(secret_ref) = copy.authentication.secret_ref_mut() {
        *secret_ref = SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        };
    }
    copy.transport = TransportChain::default();
    copy.ssh_tunnel = None;
    copy.tunnel_profile = None;
    copy.runtime_entra_access_token = None;
    copy.runtime_cloud_iam_token = None;
    copy.provenance = ConnectionProvenance::Proxied;
    copy
}

/// Documented default port per engine, mirroring the connection editor's
/// table so a credential file that omits `port` connects the same way from a
/// proxy host as it would after a desktop import.
pub fn default_port(engine: DatabaseType) -> u16 {
    match engine {
        DatabaseType::CockroachDb => 26257,
        DatabaseType::YugabyteDb => 5433,
        DatabaseType::TiDb => 4000,
        DatabaseType::ClickHouse => 8123,
        DatabaseType::Oracle => 1521,
        DatabaseType::LibSql => 8080,
        DatabaseType::Sqlite | DatabaseType::DuckDb => 0,
        other => match other.compatible_with().unwrap_or(other) {
            DatabaseType::Postgres => 5432,
            DatabaseType::MySql | DatabaseType::MariaDb => 3306,
            DatabaseType::Redis => 6379,
            _ => 1433,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::AuthenticationType;

    const FILE: &str = r##"{
      "format": "sqlly-credentials",
      "version": 1,
      "clients": [ { "name": "Acme", "color": "#1E88E5" } ],
      "environments": [ { "name": "Production", "safety": { "only_allow_select": true } } ],
      "sql_auth_accounts": [ { "name": "Reader", "username": "ro", "password": "ro-secret" } ],
      "tunnel_profiles": [
        { "name": "Bastion", "ssh": { "host": "bastion.acme.com", "username": "deploy", "auth": { "type": "password", "password": "ssh-pw" } } }
      ],
      "connections": [
        { "name": "Sales", "engine": "postgres", "server": "pg1", "database": "sales",
          "client": "Acme", "authentication": { "type": "sql-login", "account": "Reader" },
          "tunnel_profile": "Bastion", "route": { "type": "via-relay", "relay_device_id": "d", "pair_id": "p" } },
        { "name": "Ops", "engine": "sql-server", "server": "sql01",
          "authentication": { "type": "sql-login", "username": "ops", "password": "ops-secret" } },
        { "name": "Scratch", "engine": "sqlite", "server": "/tmp/scratch.db" }
      ]
    }"##;

    #[test]
    fn resolves_shared_logins_and_tunnels_within_the_file() {
        let catalog = ProxyCatalog::from_credential_text(FILE).unwrap();
        assert_eq!(catalog.profiles.len(), 3);
        let sales = &catalog.profiles[0];
        match &sales.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "ro");
                assert_eq!(secret_ref.resolve().unwrap(), "ro-secret");
            }
            other => panic!("unexpected auth {other:?}"),
        }
        assert!(sales.transport.ssh.is_some());
        assert!(sales.ssh_tunnel.is_some());
        assert!(sales.tunnel_profile.is_none());
        assert_eq!(sales.route, ConnectionRoute::Direct);
        assert_eq!(sales.provenance, ConnectionProvenance::Proxied);
        assert_eq!(sales.port, 5432);
        assert_eq!(catalog.clients[0].name, "Acme");
        assert_eq!(catalog.clients[0].color_hex.as_deref(), Some("#1E88E5"));
        assert_eq!(catalog.environments[0].safety.only_allow_select, Some(true));
    }

    #[test]
    fn ids_are_deterministic_across_parses_and_distinct_per_entry() {
        let a = ProxyCatalog::from_credential_text(FILE).unwrap();
        let b = ProxyCatalog::from_credential_text(FILE).unwrap();
        let ids_a: Vec<_> = a.profiles.iter().map(|p| p.id).collect();
        let ids_b: Vec<_> = b.profiles.iter().map(|p| p.id).collect();
        assert_eq!(ids_a, ids_b);
        assert_ne!(ids_a[0], ids_a[1]);
        assert_eq!(ids_a[1], proxy_profile_id(1, "Ops"));
        assert!(a.find(ids_a[2]).is_some());
        assert!(a.find(ConnectionId(Uuid::nil())).is_none());
    }

    #[test]
    fn client_listing_carries_no_secret() {
        let catalog = ProxyCatalog::from_credential_text(FILE).unwrap();
        let listing = catalog.client_listing();
        let json = serde_json::to_string(&listing).unwrap();
        assert!(!json.contains("ro-secret"));
        assert!(!json.contains("ops-secret"));
        assert!(!json.contains("ssh-pw"));
        assert!(!json.contains("bastion.acme.com"));
        // Identity and shape survive so the browser can show and name them.
        assert_eq!(listing.profiles[0].id, catalog.profiles[0].id);
        assert_eq!(listing.profiles[0].database.as_deref(), Some("sales"));
        assert_eq!(listing.profiles[0].client.as_deref(), Some("Acme"));
        assert!(listing.profiles[0].transport.is_empty());
        let round: ProxyCatalog = serde_json::from_str(&json).unwrap();
        assert_eq!(round, listing);
    }

    #[test]
    fn dangling_references_are_all_reported() {
        let text = FILE
            .replace("\"account\": \"Reader\"", "\"account\": \"Nobody\"")
            .replace(
                "\"tunnel_profile\": \"Bastion\"",
                "\"tunnel_profile\": \"Nowhere\"",
            );
        let err = ProxyCatalog::from_credential_text(&text).unwrap_err();
        assert_eq!(err.problems.len(), 2, "{err}");
        assert!(err.problems[0].contains("Nobody"));
        assert!(err.problems[1].contains("Nowhere"));
    }

    #[test]
    fn server_messages_are_tagged_json() {
        let hello = ProxyServerMessage::Hello {
            protocol: PROXY_PROTOCOL_VERSION,
            relay: "relay".into(),
            catalog: ProxyCatalog::default(),
            license: None,
            open: Vec::new(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(
            json.starts_with(r#"{"type":"hello","protocol":2"#),
            "{json}"
        );
        assert!(!json.contains("license"), "{json}");
        assert!(
            !json.contains("open"),
            "no launch context, no field: {json}"
        );
        let back: ProxyServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hello);

        // A protocol-1 hello (no `license`) still parses.
        let v1: ProxyServerMessage = serde_json::from_str(
            r#"{"type":"hello","protocol":1,"relay":"old","catalog":{"profiles":[]}}"#,
        )
        .unwrap();
        assert!(matches!(
            v1,
            ProxyServerMessage::Hello {
                protocol: 1,
                license: None,
                ..
            }
        ));

        let license = ProxyLicense {
            state: ProxyLicenseState::Stale,
            plan: Some("SQLly Embedded 5".into()),
            message: None,
        };
        let pushed = serde_json::to_string(&ProxyServerMessage::license(&license)).unwrap();
        assert_eq!(
            pushed,
            r#"{"type":"license","state":"stale","plan":"SQLly Embedded 5"}"#
        );
        assert!(license.user_message().contains("40 minutes"));
        assert!(!license.is_ok());
        assert!(!license.serves());
        assert_eq!(ProxyLicenseState::NotAdmitted.as_str(), "not_admitted");
        assert!(ProxyLicenseState::Grace.serves());
        assert!(!ProxyLicenseState::Grace.is_ok());
        assert_eq!(ProxyLicenseState::Grace.as_str(), "grace");
        let expired = ProxyLicense {
            state: ProxyLicenseState::Expired,
            plan: None,
            message: None,
        };
        assert_eq!(expired.user_message(), PROXY_LICENSE_EXPIRED_MESSAGE);
        let err: ProxyServerMessage =
            serde_json::from_str(r#"{"type":"error","message":"boom"}"#).unwrap();
        assert_eq!(
            err,
            ProxyServerMessage::Error {
                message: "boom".into()
            }
        );
    }

    #[test]
    fn default_ports_follow_the_editor_table() {
        assert_eq!(default_port(DatabaseType::Postgres), 5432);
        assert_eq!(default_port(DatabaseType::MySql), 3306);
        assert_eq!(default_port(DatabaseType::SqlServer), 1433);
        assert_eq!(default_port(DatabaseType::Sqlite), 0);
        assert_eq!(default_port(DatabaseType::Redis), 6379);
    }

    #[test]
    fn launch_cursor_offsets_clamp_and_count_characters() {
        let text = "SELECT 1\nFROM é t\nWHERE x";
        let at = |line, column| LaunchCursor { line, column }.offset_in(text);
        assert_eq!(at(1, 1), 0);
        assert_eq!(at(1, 8), 7);
        assert_eq!(at(1, 99), 8, "past the end of a line clamps to its end");
        assert_eq!(&text[at(2, 6)..], "é t\nWHERE x");
        assert_eq!(
            &text[at(2, 7)..],
            " t\nWHERE x",
            "columns count characters, not bytes"
        );
        assert_eq!(&text[at(3, 1)..], "WHERE x");
        assert_eq!(at(9, 1), text.len(), "past the last line clamps to the end");
        assert_eq!(at(0, 0), 0, "zero is treated as the first line and column");
    }

    #[test]
    fn hello_carries_launch_tabs_only_when_present() {
        let open = vec![LaunchOpen {
            connection_id: Some(proxy_profile_id(0, "Shop")),
            connection: Some("Shop".into()),
            database: Some("sales".into()),
            title: Some("Big orders".into()),
            script: "SELECT * FROM orders WHERE total > @min_total".into(),
            parameters: vec![LaunchParameter {
                name: "@min_total".into(),
                ty: "int".into(),
                value: Some("500".into()),
            }],
            cursor: Some(LaunchCursor {
                line: 1,
                column: 42,
            }),
        }];
        let hello = ProxyServerMessage::Hello {
            protocol: PROXY_PROTOCOL_VERSION,
            relay: "relay".into(),
            catalog: ProxyCatalog::default(),
            license: None,
            open: open.clone(),
        };
        let json = serde_json::to_string(&hello).unwrap();
        assert!(json.contains(r#""connectionId""#), "{json}");
        assert!(json.contains(r#""type":"int""#), "{json}");
        let back: ProxyServerMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, hello);
        assert!(LAUNCH_PARAMETER_TYPES.contains(&"int"));
    }
}
