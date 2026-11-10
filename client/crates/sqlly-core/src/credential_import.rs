//! The SQLLY connection file: a hand-authorable JSON file that provisions
//! connections, optionally *with* their clear-text passwords, plus the client /
//! project / environment entities they belong to (colors and base64-embedded
//! icon images included).
//!
//! It is the one format SQLLY writes for both Export Connections (every
//! secret field left out) and Export Conn w/ Creds (passwords filled in), and
//! what Import Connections reads; the app layer moves every secret it finds
//! into the OS keyring before anything is persisted. The older secret-free
//! [`crate::ProfileExportBundle`] is still read, never written. The `format`
//! value stays `"sqlly-credentials"` so files written by older versions and
//! files written now are the same format.
//!
//! Parsing is strict (`deny_unknown_fields`) so a typo in a hand-written file is
//! reported instead of silently dropping a setting, and validation collects
//! every problem in the file rather than stopping at the first. Nothing here
//! touches the keyring, the filesystem, or the connection store: the output is
//! a [`CredentialImportPlan`] whose profiles carry `InMemory` secrets for the
//! caller to secure.
//!
//! The format is documented for users on sqlly.app/docs.

use std::collections::HashSet;
use std::path::PathBuf;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};

use crate::connection::{
    AuthenticationType, ConnectionEnvironment, ConnectionProfile, ConnectionProvenance,
    ConnectionRoute, EntraAccountBinding, KubePortForward, KubeTargetKind, MaskRule,
    MaskingOverrides, ProxyHop, ProxyKind, RuleScope, SecretProvider, SecretRef, SshAuth, SshHop,
    SshTunnelConfig, SsmPortForward, TlsMode, TransportChain,
};
use crate::profile_store::ExportedSafetyFeatures;

/// Value of the required top-level `format` field.
pub const CREDENTIAL_FILE_FORMAT: &str = "sqlly-credentials";
/// Newest `version` this build understands.
pub const CREDENTIAL_FILE_VERSION: u32 = 1;
/// Largest decoded icon image accepted per entity.
pub const MAX_IMAGE_BYTES: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// File schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialFile {
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clients: Vec<CredentialEntity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<CredentialEntity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environments: Vec<CredentialEntity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sql_auth_accounts: Vec<CredentialSqlAccount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnel_profiles: Vec<CredentialTunnelProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub connections: Vec<CredentialConnection>,
}

/// A client, project, or environment label.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialEntity {
    pub name: String,
    /// `#RGB`, `#RRGGBB`, or `#RRGGBBAA` (the `#` is optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// A `data:image/...;base64,` URI or bare base64 of a PNG, JPEG, GIF,
    /// WebP, BMP, ICO, or SVG image.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Only the flags listed change; the rest keep an existing entity's
    /// value (off for a new entity).
    #[serde(default)]
    pub safety: CredentialSafety,
}

/// Per-flag safety patch for a client, project, or environment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSafety {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_outer_transaction_rollback_scope: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub only_allow_select: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub use_confirmation_dialog: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_ai_review_for_data_changes: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unconstrained_update: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unconstrained_delete: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_touch_id_to_connect: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_touch_id_for_non_select: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_production: Option<bool>,
}

impl CredentialSafety {
    /// Applies the listed flags onto `target`.
    pub fn apply_to(&self, target: &mut ExportedSafetyFeatures) {
        let set = |slot: &mut bool, value: Option<bool>| {
            if let Some(value) = value {
                *slot = value;
            }
        };
        set(
            &mut target.use_outer_transaction_rollback_scope,
            self.use_outer_transaction_rollback_scope,
        );
        set(&mut target.only_allow_select, self.only_allow_select);
        set(
            &mut target.use_confirmation_dialog,
            self.use_confirmation_dialog,
        );
        set(
            &mut target.require_ai_review_for_data_changes,
            self.require_ai_review_for_data_changes,
        );
        set(
            &mut target.allow_unconstrained_update,
            self.allow_unconstrained_update,
        );
        set(
            &mut target.allow_unconstrained_delete,
            self.allow_unconstrained_delete,
        );
        set(
            &mut target.require_touch_id_to_connect,
            self.require_touch_id_to_connect,
        );
        set(
            &mut target.require_touch_id_for_non_select,
            self.require_touch_id_for_non_select,
        );
        set(&mut target.is_production, self.is_production);
    }
}

/// A reusable SQL login shared by several connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSqlAccount {
    pub name: String,
    pub username: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

/// A named, reusable proxy / SSH / Kubernetes chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialTunnelProfile {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<CredentialProxy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<CredentialSsh>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube: Option<CredentialKube>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssm: Option<CredentialSsm>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialTransport {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<CredentialProxy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<CredentialSsh>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube: Option<CredentialKube>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssm: Option<CredentialSsm>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialProxy {
    #[serde(default)]
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSsh {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth: CredentialSshAuth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<CredentialSshHop>,
    /// Seconds between SSH keepalives (30 when absent, 0 for none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_seconds: Option<u64>,
    /// Unanswered keepalives before the tunnel is reopened (3 when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_max: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSshHop {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth: CredentialSshAuth,
}

fn default_ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CredentialSshAuth {
    #[default]
    Agent,
    KeyFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<String>,
    },
    /// Password sign-in. `password` is left out of an export without
    /// credentials; the connection then asks for it on first connect.
    Password {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialKube {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default = "default_kube_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub target_kind: KubeTargetKind,
    pub target_name: String,
    pub remote_port: u16,
}

fn default_kube_namespace() -> String {
    "default".to_string()
}

/// An AWS Session Manager port-forward (`aws ssm start-session`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialSsm {
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host: Option<String>,
}

/// One connection. Everything except `name`, `engine`, and `server` is
/// optional and falls back to the same default the connection editor uses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConnection {
    pub name: String,
    /// Engine name, e.g. `sql-server`, `postgres`, `mysql`, `sqlite`,
    /// `neon`, `clickhouse`, `redis` (case and punctuation are ignored).
    pub engine: String,
    /// Host name, IP, `host\instance`, or the file path for SQLite/DuckDB.
    pub server: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// `local`, `dev`, `test`, `staging`, `production`, or `unset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<CredentialAuth>,
    #[serde(default)]
    pub tls: CredentialTls,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub application_name: Option<String>,
    #[serde(default)]
    pub safety: CredentialConnectionSafety,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_sql: Option<String>,
    /// The user's free-form notes on the connection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_key_delimiter: Option<String>,
    /// Sentinel settings without the Sentinel password reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_sentinel: Option<crate::redis_sentinel::RedisSentinelConfig>,
    /// The password the sentinels themselves require, in clear text like
    /// `authentication.password`; the importer moves it into the keychain.
    /// Never written by an export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_sentinel_password: Option<String>,
    /// Redis Cluster mode and node map (see [`crate::redis_cluster`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_cluster: Option<crate::redis_cluster::RedisClusterConfig>,
    /// Redis with Microsoft Entra sign-in: the Redis user name to sign in
    /// as (blank or absent: the token's object id).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_entra_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clickhouse_native_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entra_account: Option<CredentialEntraAccount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub route: Option<CredentialRoute>,
    /// Name of a tunnel profile in this file or already saved in SQLLY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<CredentialTransport>,
    #[serde(default)]
    pub masking: CredentialMasking,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CredentialAuth {
    /// User name + password, or `account` naming a shared SQL login.
    SqlLogin {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// Windows / Kerberos integrated security (SQL Server).
    Integrated,
    EntraBrowser,
    EntraDeviceCode,
    AwsRdsIam {
        username: String,
        region: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    GcpCloudSqlIam {
        username: String,
    },
    /// libSQL / Turso bearer token; omit `token` for an anonymous server.
    AuthToken {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
    /// No sign-in (SQLite / DuckDB files).
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialTls {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<CredentialTlsMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypt: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_server_certificate: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cert_file: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_key_file: Option<PathBuf>,
    /// Also trust the operating system's root certificates (Redis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_os_roots: Option<bool>,
    /// PostgreSQL 17+ direct TLS negotiation (`sslnegotiation=direct`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direct: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialTlsMode {
    Disable,
    Require,
    VerifyCa,
    VerifyFull,
}

impl From<CredentialTlsMode> for TlsMode {
    fn from(mode: CredentialTlsMode) -> Self {
        match mode {
            CredentialTlsMode::Disable => TlsMode::Disable,
            CredentialTlsMode::Require => TlsMode::Require,
            CredentialTlsMode::VerifyCa => TlsMode::VerifyCa,
            CredentialTlsMode::VerifyFull => TlsMode::VerifyFull,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialConnectionSafety {
    #[serde(default)]
    pub auto_wrap_rollback: bool,
    #[serde(default)]
    pub block_all_but_select: bool,
    #[serde(default)]
    pub confirm_data_changes: bool,
    #[serde(default = "default_true")]
    pub allow_unbounded_mutations: bool,
    /// `true` / `false` overrides the environment's production flag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production_override: Option<bool>,
}

impl Default for CredentialConnectionSafety {
    fn default() -> Self {
        Self {
            auto_wrap_rollback: false,
            block_all_but_select: false,
            confirm_data_changes: false,
            allow_unbounded_mutations: true,
            production_override: None,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialEntraAccount {
    pub account_id: String,
    pub tenant_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum CredentialRoute {
    Direct,
    ViaRelay {
        relay_device_id: String,
        pair_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        relay_hint: Option<String>,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialMasking {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub include: Vec<CredentialMaskRule>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<CredentialMaskRule>,
}

/// A bare pattern string, or `{ "pattern": ..., "scope": ... }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CredentialMaskRule {
    Pattern(String),
    Scoped {
        pattern: String,
        #[serde(default)]
        scope: CredentialMaskScope,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialMaskScope {
    #[default]
    ColumnName,
    TableDotColumn,
}

// ---------------------------------------------------------------------------
// Plan (validated output)
// ---------------------------------------------------------------------------

/// A decoded, type-checked icon image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub bytes: Vec<u8>,
    /// File extension without the dot (`png`, `jpg`, `svg`, ...).
    pub extension: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedEntity {
    pub name: String,
    /// Normalized `#RRGGBB` or `#RRGGBBAA`.
    pub color_hex: Option<String>,
    pub image: Option<DecodedImage>,
    /// Flags to change; unlisted flags keep an existing entity's value.
    pub safety: CredentialSafety,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedSqlAccount {
    pub name: String,
    pub username: String,
    pub password: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedTunnelProfile {
    pub name: String,
    /// Secrets are `InMemory` clear text for the caller to move to the keyring.
    pub chain: TransportChain,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedConnection {
    /// Fully built profile. Every secret is `InMemory` (clear text, or an empty
    /// key when the file supplied none). `tunnel_profile` is unset; see
    /// [`Self::tunnel_profile`].
    pub profile: ConnectionProfile,
    /// Shared SQL login to bind (by name), from this file or already saved.
    pub sql_account: Option<String>,
    /// Tunnel profile to bind (by name), from this file or already saved.
    pub tunnel_profile: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialImportPlan {
    pub clients: Vec<PlannedEntity>,
    pub projects: Vec<PlannedEntity>,
    pub environments: Vec<PlannedEntity>,
    pub sql_auth_accounts: Vec<PlannedSqlAccount>,
    pub tunnel_profiles: Vec<PlannedTunnelProfile>,
    pub connections: Vec<PlannedConnection>,
}

/// Every problem found in a credential file, each prefixed with where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialImportError {
    pub problems: Vec<String>,
}

impl std::fmt::Display for CredentialImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.problems.as_slice() {
            [one] => write!(f, "{one}"),
            many => write!(f, "{} problems: {}", many.len(), many.join("; ")),
        }
    }
}

impl std::error::Error for CredentialImportError {}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Cheap sniff: is `text` a JSON object whose `format` is the credential
/// format? Used to route a picked file before full parsing.
pub fn is_credential_file(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("format")
                .and_then(|f| f.as_str())
                .map(|f| f == CREDENTIAL_FILE_FORMAT)
        })
        .unwrap_or(false)
}

/// Parses and validates a credential file into an import plan.
/// `default_port` supplies the engine's standard port when a connection
/// omits `port` (the app passes its connection-editor defaults).
pub fn parse_credential_file(
    text: &str,
    default_port: impl Fn(DatabaseType) -> u16,
) -> Result<CredentialImportPlan, CredentialImportError> {
    let file: CredentialFile =
        serde_json::from_str(text).map_err(|error| CredentialImportError {
            problems: vec![format!("Not a valid SQLLY credential file: {error}")],
        })?;
    plan_credential_file(&file, default_port)
}

pub fn plan_credential_file(
    file: &CredentialFile,
    default_port: impl Fn(DatabaseType) -> u16,
) -> Result<CredentialImportPlan, CredentialImportError> {
    let mut problems = Vec::new();
    if file.format != CREDENTIAL_FILE_FORMAT {
        problems.push(format!(
            "format must be \"{CREDENTIAL_FILE_FORMAT}\" (found \"{}\")",
            file.format
        ));
    }
    if file.version == 0 || file.version > CREDENTIAL_FILE_VERSION {
        problems.push(format!(
            "version {} is not supported by this version of SQLLY (expected {CREDENTIAL_FILE_VERSION})",
            file.version
        ));
    }
    if !problems.is_empty() {
        return Err(CredentialImportError { problems });
    }

    let mut plan = CredentialImportPlan {
        clients: plan_entities("clients", &file.clients, &mut problems),
        projects: plan_entities("projects", &file.projects, &mut problems),
        environments: plan_entities("environments", &file.environments, &mut problems),
        ..CredentialImportPlan::default()
    };

    let mut account_names = HashSet::new();
    for (index, account) in file.sql_auth_accounts.iter().enumerate() {
        let at = format!("sql_auth_accounts[{index}]");
        let name = account.name.trim();
        if name.is_empty() {
            problems.push(format!("{at}: name is required"));
            continue;
        }
        if !account_names.insert(name.to_lowercase()) {
            problems.push(format!("{at} \"{name}\": duplicate account name"));
        }
        if account.username.trim().is_empty() {
            problems.push(format!("{at} \"{name}\": username is required"));
        }
        plan.sql_auth_accounts.push(PlannedSqlAccount {
            name: name.to_string(),
            username: account.username.trim().to_string(),
            password: account.password.clone().filter(|p| !p.is_empty()),
        });
    }

    let mut tunnel_names = HashSet::new();
    for (index, tunnel) in file.tunnel_profiles.iter().enumerate() {
        let name = tunnel.name.trim();
        let at = format!("tunnel_profiles[{index}] \"{name}\"");
        if name.is_empty() {
            problems.push(format!("tunnel_profiles[{index}]: name is required"));
            continue;
        }
        if !tunnel_names.insert(name.to_lowercase()) {
            problems.push(format!("{at}: duplicate tunnel profile name"));
        }
        let chain = build_transport(&CredentialTransport {
            proxy: tunnel.proxy.clone(),
            ssh: tunnel.ssh.clone(),
            kube: tunnel.kube.clone(),
            ssm: tunnel.ssm.clone(),
        });
        if chain.is_empty() {
            problems.push(format!("{at}: needs a proxy, ssh, kube, or ssm hop"));
        }
        problems.extend(chain.validate().into_iter().map(|e| format!("{at}: {e}")));
        plan.tunnel_profiles.push(PlannedTunnelProfile {
            name: name.to_string(),
            chain,
        });
    }

    for (index, connection) in file.connections.iter().enumerate() {
        let label = connection.name.trim();
        let at = if label.is_empty() {
            format!("connections[{index}]")
        } else {
            format!("connections[{index}] \"{label}\"")
        };
        if let Some(planned) = plan_connection(connection, &at, &default_port, &mut problems) {
            plan.connections.push(planned);
        }
    }

    if problems.is_empty() {
        Ok(plan)
    } else {
        Err(CredentialImportError { problems })
    }
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

fn plan_entities(
    section: &str,
    entities: &[CredentialEntity],
    problems: &mut Vec<String>,
) -> Vec<PlannedEntity> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(entities.len());
    for (index, entity) in entities.iter().enumerate() {
        let name = entity.name.trim();
        if name.is_empty() {
            problems.push(format!("{section}[{index}]: name is required"));
            continue;
        }
        let at = format!("{section}[{index}] \"{name}\"");
        if !seen.insert(name.to_lowercase()) {
            problems.push(format!("{at}: duplicate name"));
        }
        let color_hex = match entity.color.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => match normalize_hex_color(raw) {
                Some(hex) => Some(hex),
                None => {
                    problems.push(format!(
                        "{at}: color \"{raw}\" must be #RGB, #RRGGBB, or #RRGGBBAA"
                    ));
                    None
                }
            },
        };
        let image = match entity.image.as_deref().map(str::trim) {
            None | Some("") => None,
            Some(raw) => match decode_image(raw) {
                Ok(image) => Some(image),
                Err(error) => {
                    problems.push(format!("{at}: image {error}"));
                    None
                }
            },
        };
        out.push(PlannedEntity {
            name: name.to_string(),
            color_hex,
            image,
            safety: entity.safety.clone(),
        });
    }
    out
}

/// Accepts `#RGB`, `#RRGGBB`, `#RRGGBBAA` (with or without `#`) and returns
/// the uppercase `#RRGGBB` / `#RRGGBBAA` form the app stores.
pub fn normalize_hex_color(raw: &str) -> Option<String> {
    let digits = raw.trim().trim_start_matches('#');
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let expanded = match digits.len() {
        3 => digits.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 | 8 => digits.to_string(),
        _ => return None,
    };
    Some(format!("#{}", expanded.to_ascii_uppercase()))
}

/// Decodes a `data:` URI or bare base64 and sniffs the image type from its
/// bytes (a declared media type is not trusted on its own).
pub fn decode_image(raw: &str) -> Result<DecodedImage, String> {
    let payload = match raw.strip_prefix("data:") {
        Some(rest) => {
            let (header, data) = rest
                .split_once(',')
                .ok_or_else(|| "data URI is missing its ',' separator".to_string())?;
            if !header.ends_with(";base64") {
                return Err("data URI must be base64 encoded (\";base64,\")".to_string());
            }
            data
        }
        None => raw,
    };
    let compact: String = payload.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(compact.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(compact.as_bytes()))
        .map_err(|error| format!("is not valid base64: {error}"))?;
    if bytes.is_empty() {
        return Err("is empty".to_string());
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "is {} KB; the limit is {} KB",
            bytes.len() / 1024,
            MAX_IMAGE_BYTES / 1024
        ));
    }
    let extension = sniff_image_extension(&bytes).ok_or_else(|| {
        "is not a recognized image (PNG, JPEG, GIF, WebP, BMP, ICO, or SVG)".to_string()
    })?;
    Ok(DecodedImage { bytes, extension })
}

fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("gif")
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("webp")
    } else if bytes.starts_with(b"BM") {
        Some("bmp")
    } else if bytes.starts_with(&[0, 0, 1, 0]) {
        Some("ico")
    } else {
        let head = String::from_utf8_lossy(&bytes[..bytes.len().min(1024)]).to_ascii_lowercase();
        head.contains("<svg").then_some("svg")
    }
}

fn normalized_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Resolves an engine name: the internal variant name (`SqlServer`), the
/// display name (`SQL Server`), or a common alias (`mssql`, `postgresql`),
/// ignoring case, spaces, dashes, and underscores.
pub fn parse_engine(raw: &str) -> Option<DatabaseType> {
    let key = normalized_key(raw);
    let alias = match key.as_str() {
        "mssql" | "tsql" => Some(DatabaseType::SqlServer),
        "azuresql" | "azure" => Some(DatabaseType::AzureSqlDatabase),
        "pg" | "postgresql" | "psql" => Some(DatabaseType::Postgres),
        "maria" => Some(DatabaseType::MariaDb),
        "sqlite3" => Some(DatabaseType::Sqlite),
        "turso" => Some(DatabaseType::LibSql),
        "duck" => Some(DatabaseType::DuckDb),
        "cockroach" => Some(DatabaseType::CockroachDb),
        _ => None,
    };
    alias.or_else(|| {
        DatabaseType::ALL.iter().copied().find(|engine| {
            let variant = serde_json::to_value(engine)
                .ok()
                .and_then(|v| v.as_str().map(normalized_key))
                .unwrap_or_default();
            variant == key || normalized_key(engine.display_name()) == key
        })
    })
}

fn parse_environment(raw: &str) -> Option<ConnectionEnvironment> {
    match normalized_key(raw).as_str() {
        "local" => Some(ConnectionEnvironment::Local),
        "dev" | "development" => Some(ConnectionEnvironment::Dev),
        "test" | "testing" | "qa" => Some(ConnectionEnvironment::Test),
        "staging" | "stage" => Some(ConnectionEnvironment::Staging),
        "prod" | "production" => Some(ConnectionEnvironment::Production),
        "unset" | "none" | "" => Some(ConnectionEnvironment::Unset),
        _ => None,
    }
}

fn is_file_engine(engine: DatabaseType) -> bool {
    matches!(engine, DatabaseType::Sqlite | DatabaseType::DuckDb)
}

fn in_memory(secret: &str) -> SecretRef {
    SecretRef {
        provider: SecretProvider::InMemory,
        key: secret.to_string(),
    }
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn build_ssh_auth(auth: &CredentialSshAuth) -> SshAuth {
    match auth {
        CredentialSshAuth::Agent => SshAuth::Agent,
        CredentialSshAuth::KeyFile { path, passphrase } => SshAuth::KeyFile {
            path: path.trim().to_string(),
            passphrase: passphrase
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(in_memory),
        },
        // No password in the file stays an empty in-memory secret, which the
        // app asks for on first connect.
        CredentialSshAuth::Password { password } => SshAuth::Password {
            secret_ref: in_memory(password.as_deref().unwrap_or("")),
        },
    }
}

fn build_transport(transport: &CredentialTransport) -> TransportChain {
    TransportChain {
        proxy: transport.proxy.as_ref().map(|proxy| ProxyHop {
            kind: proxy.kind,
            host: proxy.host.trim().to_string(),
            port: proxy.port,
            username: non_empty(&proxy.username),
            password: proxy
                .password
                .as_deref()
                .filter(|p| !p.is_empty())
                .map(in_memory),
        }),
        ssh: transport.ssh.as_ref().map(|ssh| SshTunnelConfig {
            target: SshHop {
                host: ssh.host.trim().to_string(),
                port: ssh.port,
                username: ssh.username.trim().to_string(),
                auth: build_ssh_auth(&ssh.auth),
            },
            jump: ssh.jump.as_ref().map(|jump| SshHop {
                host: jump.host.trim().to_string(),
                port: jump.port,
                username: jump.username.trim().to_string(),
                auth: build_ssh_auth(&jump.auth),
            }),
            keepalive: crate::connection::SshKeepalive {
                interval_seconds: ssh.keepalive_seconds,
                max_missed: ssh.keepalive_max,
            },
        }),
        kube: transport.kube.as_ref().map(|kube| KubePortForward {
            kubeconfig: non_empty(&kube.kubeconfig),
            context: non_empty(&kube.context),
            namespace: kube.namespace.trim().to_string(),
            target_kind: kube.target_kind,
            target_name: kube.target_name.trim().to_string(),
            remote_port: kube.remote_port,
        }),
        ssm: transport.ssm.as_ref().map(|ssm| SsmPortForward {
            target: ssm.target.trim().to_string(),
            region: non_empty(&ssm.region),
            profile: non_empty(&ssm.profile),
            remote_host: non_empty(&ssm.remote_host),
        }),
    }
}

fn build_mask_rules(
    rules: &[CredentialMaskRule],
    at: &str,
    problems: &mut Vec<String>,
) -> Vec<MaskRule> {
    rules
        .iter()
        .filter_map(|rule| {
            let (pattern, scope) = match rule {
                CredentialMaskRule::Pattern(pattern) => (pattern, CredentialMaskScope::ColumnName),
                CredentialMaskRule::Scoped { pattern, scope } => (pattern, *scope),
            };
            if pattern.trim().is_empty() {
                problems.push(format!("{at}: masking pattern cannot be empty"));
                return None;
            }
            Some(MaskRule {
                pattern: pattern.trim().to_string(),
                scope: match scope {
                    CredentialMaskScope::ColumnName => RuleScope::ColumnName,
                    CredentialMaskScope::TableDotColumn => RuleScope::TableDotColumn,
                },
            })
        })
        .collect()
}

/// Which sign-in methods an engine accepts; mirrors the connection editor.
fn authentication_supported(engine: DatabaseType, auth: &AuthenticationType) -> bool {
    let token_engine = engine == DatabaseType::LibSql;
    match auth {
        AuthenticationType::SqlLogin { .. } => !is_file_engine(engine) && !token_engine,
        AuthenticationType::AuthToken { .. } => token_engine,
        AuthenticationType::Integrated => engine == DatabaseType::SqlServer,
        // Redis: Azure Cache for Redis / Azure Managed Redis.
        AuthenticationType::EntraBrowser | AuthenticationType::EntraDeviceCode => matches!(
            engine,
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase | DatabaseType::Redis
        ),
        // AWS IAM also covers ElastiCache / MemoryDB (Redis); Google IAM is
        // Cloud SQL only.
        AuthenticationType::AwsRdsIam { .. } => matches!(
            engine.connection_kind(),
            ConnectionKind::PostgresWire | ConnectionKind::MySqlWire | ConnectionKind::Resp
        ),
        AuthenticationType::GcpCloudSqlIam { .. } => matches!(
            engine.connection_kind(),
            ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
        ),
    }
}

fn plan_connection(
    connection: &CredentialConnection,
    at: &str,
    default_port: &impl Fn(DatabaseType) -> u16,
    problems: &mut Vec<String>,
) -> Option<PlannedConnection> {
    let before = problems.len();
    let name = connection.name.trim();
    if name.is_empty() {
        problems.push(format!("{at}: name is required"));
    }
    let Some(engine) = parse_engine(&connection.engine) else {
        problems.push(format!("{at}: unknown engine \"{}\"", connection.engine));
        return None;
    };
    let file_engine = is_file_engine(engine);
    let server = connection.server.trim();
    if server.is_empty() {
        problems.push(format!(
            "{at}: server is required{}",
            if file_engine {
                " (the database file path)"
            } else {
                ""
            }
        ));
    }
    let port = if file_engine {
        0
    } else {
        match connection.port {
            Some(0) => {
                problems.push(format!("{at}: port must be 1–65535"));
                0
            }
            Some(port) => port,
            None => default_port(engine),
        }
    };
    let environment = match connection.environment.as_deref() {
        None => ConnectionEnvironment::Unset,
        Some(raw) => parse_environment(raw).unwrap_or_else(|| {
            problems.push(format!(
                "{at}: environment \"{raw}\" must be local, dev, test, staging, production, or unset"
            ));
            ConnectionEnvironment::Unset
        }),
    };

    let mut profile = ConnectionProfile::sql_login(name, server, "", in_memory(""));
    profile.database_type = engine;
    profile.port = port;
    profile.database = non_empty(&connection.database);
    profile.environment = environment;
    profile.client = non_empty(&connection.client);
    profile.project = non_empty(&connection.project);
    profile.provenance = ConnectionProvenance::Imported;

    // ---- authentication ----
    let mut sql_account = None;
    let auth = match &connection.authentication {
        None | Some(CredentialAuth::None) => {
            if !file_engine {
                problems.push(format!(
                    "{at}: authentication is required for {}",
                    engine.display_name()
                ));
            }
            None
        }
        Some(CredentialAuth::SqlLogin {
            username,
            password,
            account,
        }) => {
            let account = non_empty(account);
            let username = non_empty(username);
            if account.is_some() && (username.is_some() || password.is_some()) {
                problems.push(format!(
                    "{at}: sql-login takes either account or username/password, not both"
                ));
            }
            // Redis and Valkey sign in as the `default` user when none is
            // named (`redis://:password@host`).
            if account.is_none()
                && username.is_none()
                && engine.connection_kind() != ConnectionKind::Resp
            {
                problems.push(format!("{at}: sql-login needs a username (or an account)"));
            }
            sql_account = account;
            Some(AuthenticationType::SqlLogin {
                username: username.unwrap_or_default(),
                secret_ref: in_memory(password.as_deref().unwrap_or("")),
            })
        }
        Some(CredentialAuth::Integrated) => Some(AuthenticationType::Integrated),
        Some(CredentialAuth::EntraBrowser) => Some(AuthenticationType::EntraBrowser),
        Some(CredentialAuth::EntraDeviceCode) => Some(AuthenticationType::EntraDeviceCode),
        Some(CredentialAuth::AwsRdsIam {
            username,
            region,
            profile: aws_profile,
        }) => {
            if username.trim().is_empty() || region.trim().is_empty() {
                problems.push(format!("{at}: aws-rds-iam needs username and region"));
            }
            Some(AuthenticationType::AwsRdsIam {
                username: username.trim().to_string(),
                region: region.trim().to_string(),
                profile: non_empty(aws_profile),
            })
        }
        Some(CredentialAuth::GcpCloudSqlIam { username }) => {
            if username.trim().is_empty() {
                problems.push(format!("{at}: gcp-cloud-sql-iam needs a username"));
            }
            Some(AuthenticationType::GcpCloudSqlIam {
                username: username.trim().to_string(),
            })
        }
        Some(CredentialAuth::AuthToken { token }) => Some(AuthenticationType::AuthToken {
            secret_ref: in_memory(token.as_deref().map(str::trim).unwrap_or("")),
        }),
    };
    if let Some(auth) = auth {
        if !authentication_supported(engine, &auth) {
            problems.push(format!(
                "{at}: this sign-in method is not supported for {}",
                engine.display_name()
            ));
        }
        if matches!(
            auth,
            AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. }
        ) {
            profile.encrypt = true;
        }
        profile.authentication = auth;
    }

    // ---- TLS ----
    let tls = &connection.tls;
    if let Some(mode) = tls.mode {
        let mode = TlsMode::from(mode);
        let (encrypt, trust) = mode.legacy_flags();
        if tls.encrypt.is_some_and(|e| e != encrypt)
            || tls.trust_server_certificate.is_some_and(|t| t != trust)
        {
            problems.push(format!(
                "{at}: tls.mode conflicts with tls.encrypt / tls.trust_server_certificate; set one or the other"
            ));
        }
        profile.tls_mode = mode;
        profile.encrypt = encrypt;
        profile.trust_server_certificate = trust;
    } else {
        if let Some(encrypt) = tls.encrypt {
            profile.encrypt = encrypt;
        }
        if let Some(trust) = tls.trust_server_certificate {
            profile.trust_server_certificate = trust;
        }
    }
    profile.tls_ca_file = tls.ca_file.clone().filter(|p| !p.as_os_str().is_empty());
    profile.tls_trust_os_roots = tls.trust_os_roots.unwrap_or(false);
    profile.tls_direct = tls.direct.unwrap_or(false);
    profile.tls_client_cert_file = tls
        .client_cert_file
        .clone()
        .filter(|p| !p.as_os_str().is_empty());
    profile.tls_client_key_file = tls
        .client_key_file
        .clone()
        .filter(|p| !p.as_os_str().is_empty());
    if profile.tls_client_cert_file.is_some() != profile.tls_client_key_file.is_some() {
        problems.push(format!(
            "{at}: tls.client_cert_file and tls.client_key_file must both be set"
        ));
    }
    if engine.requires_tls() && !profile.encrypt {
        problems.push(format!(
            "{at}: {} requires an encrypted connection",
            engine.display_name()
        ));
    }

    // ---- timeouts and misc ----
    match connection.timeout_seconds {
        Some(0) => problems.push(format!("{at}: timeout_seconds must be at least 1")),
        Some(seconds) => profile.timeout_seconds = seconds,
        None => {}
    }
    profile.statement_timeout_seconds = connection.statement_timeout_seconds.filter(|s| *s > 0);
    if let Some(app) = non_empty(&connection.application_name) {
        profile.application_name = app;
    }
    let safety = &connection.safety;
    profile.auto_wrap_rollback = safety.auto_wrap_rollback;
    profile.block_all_but_select = safety.block_all_but_select;
    profile.confirm_data_changes = safety.confirm_data_changes;
    profile.allow_unbounded_mutations = safety.allow_unbounded_mutations;
    profile.production_override = safety.production_override;
    profile.favorite = connection.favorite;
    profile.startup_sql = connection
        .startup_sql
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    profile.notes = connection
        .notes
        .as_deref()
        .filter(|notes| !notes.trim().is_empty())
        .map(str::to_string);
    profile.redis_sentinel = connection
        .redis_sentinel
        .as_ref()
        .map(crate::redis_sentinel::RedisSentinelConfig::without_secret);
    if let (Some(sentinel), Some(password)) = (
        profile.redis_sentinel.as_mut(),
        connection
            .redis_sentinel_password
            .as_deref()
            .filter(|p| !p.is_empty()),
    ) {
        sentinel.sentinel_password_ref = Some(SecretRef {
            provider: SecretProvider::InMemory,
            key: password.to_string(),
        });
    }
    profile.redis_cluster = connection
        .redis_cluster
        .clone()
        .filter(|cluster| !cluster.is_default());
    profile.redis_entra_user = connection
        .redis_entra_user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string);
    profile.redis_key_delimiter = connection
        .redis_key_delimiter
        .clone()
        .filter(|d| !d.is_empty());
    match connection.clickhouse_native_port {
        Some(_) if engine != DatabaseType::ClickHouse => problems.push(format!(
            "{at}: clickhouse_native_port only applies to ClickHouse"
        )),
        Some(0) => problems.push(format!("{at}: clickhouse_native_port must be 1–65535")),
        other => profile.clickhouse_native_port = other,
    }
    profile.entra_account = connection
        .entra_account
        .as_ref()
        .map(|binding| EntraAccountBinding {
            account_id: binding.account_id.trim().to_string(),
            tenant_id: binding.tenant_id.trim().to_string(),
            username: non_empty(&binding.username),
        });
    profile.route = match &connection.route {
        None | Some(CredentialRoute::Direct) => ConnectionRoute::Direct,
        Some(CredentialRoute::ViaRelay {
            relay_device_id,
            pair_id,
            relay_hint,
        }) => {
            if relay_device_id.trim().is_empty() || pair_id.trim().is_empty() {
                problems.push(format!(
                    "{at}: via-relay route needs relay_device_id and pair_id"
                ));
            }
            ConnectionRoute::ViaRelay {
                relay_device_id: relay_device_id.trim().to_string(),
                pair_id: pair_id.trim().to_string(),
                relay_hint: non_empty(relay_hint),
            }
        }
    };

    // ---- transport ----
    let tunnel_profile = non_empty(&connection.tunnel_profile);
    if let Some(transport) = &connection.transport {
        if tunnel_profile.is_some() {
            problems.push(format!(
                "{at}: use either tunnel_profile or transport, not both"
            ));
        }
        let chain = build_transport(transport);
        problems.extend(chain.validate().into_iter().map(|e| format!("{at}: {e}")));
        profile.ssh_tunnel = chain.ssh.clone();
        profile.transport = chain;
    }

    // ---- masking ----
    profile.masking = MaskingOverrides {
        include: build_mask_rules(&connection.masking.include, at, problems),
        exclude: build_mask_rules(&connection.masking.exclude, at, problems),
    };

    (problems.len() == before).then_some(PlannedConnection {
        profile,
        sql_account,
        tunnel_profile,
    })
}

// ---------------------------------------------------------------------------
// Export (profile → file)
// ---------------------------------------------------------------------------

/// The engine name written on export: the display name lowercased with
/// punctuation runs turned into dashes (`SQL Server` → `sql-server`,
/// `PostgreSQL` → `postgresql`), which [`parse_engine`] reads back.
pub fn engine_export_name(engine: DatabaseType) -> String {
    let mut out = String::new();
    for c in engine.display_name().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

fn environment_export_name(environment: ConnectionEnvironment) -> &'static str {
    match environment {
        ConnectionEnvironment::Local => "local",
        ConnectionEnvironment::Dev => "dev",
        ConnectionEnvironment::Test => "test",
        ConnectionEnvironment::Staging => "staging",
        ConnectionEnvironment::Production => "production",
        ConnectionEnvironment::Unset => "unset",
    }
}

/// Encodes icon bytes as a `data:` URI, or `None` when they are not a
/// recognized image type.
pub fn encode_image_data_uri(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_IMAGE_BYTES {
        return None;
    }
    let mime = match sniff_image_extension(bytes)? {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "image/svg+xml",
    };
    Some(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// Full safety flags as a patch that sets every flag.
pub fn safety_export(features: &ExportedSafetyFeatures) -> CredentialSafety {
    CredentialSafety {
        use_outer_transaction_rollback_scope: Some(features.use_outer_transaction_rollback_scope),
        only_allow_select: Some(features.only_allow_select),
        use_confirmation_dialog: Some(features.use_confirmation_dialog),
        require_ai_review_for_data_changes: Some(features.require_ai_review_for_data_changes),
        allow_unconstrained_update: Some(features.allow_unconstrained_update),
        allow_unconstrained_delete: Some(features.allow_unconstrained_delete),
        require_touch_id_to_connect: Some(features.require_touch_id_to_connect),
        require_touch_id_for_non_select: Some(features.require_touch_id_for_non_select),
        is_production: Some(features.is_production),
    }
}

/// Resolves a stored secret reference to its clear text, or `None` when it
/// has no value (the export then omits that password).
pub type SecretLookup<'a> = dyn FnMut(&SecretRef) -> Option<String> + 'a;

fn export_secret(secret_ref: &SecretRef, lookup: &mut SecretLookup<'_>) -> Option<String> {
    lookup(secret_ref).filter(|s| !s.is_empty())
}

fn export_ssh_auth(auth: &SshAuth, lookup: &mut SecretLookup<'_>) -> CredentialSshAuth {
    match auth {
        SshAuth::Agent => CredentialSshAuth::Agent,
        SshAuth::KeyFile { path, passphrase } => CredentialSshAuth::KeyFile {
            path: path.clone(),
            passphrase: passphrase.as_ref().and_then(|r| export_secret(r, lookup)),
        },
        // The sign-in method survives an export without the password.
        SshAuth::Password { secret_ref } => CredentialSshAuth::Password {
            password: export_secret(secret_ref, lookup),
        },
    }
}

/// Maps a transport chain to its file shape, resolving secrets.
pub fn export_transport(
    chain: &TransportChain,
    lookup: &mut SecretLookup<'_>,
) -> CredentialTransport {
    CredentialTransport {
        proxy: chain.proxy.as_ref().map(|proxy| CredentialProxy {
            kind: proxy.kind,
            host: proxy.host.clone(),
            port: proxy.port,
            username: proxy.username.clone(),
            password: proxy
                .password
                .as_ref()
                .and_then(|r| export_secret(r, lookup)),
        }),
        ssh: chain.ssh.as_ref().map(|ssh| CredentialSsh {
            host: ssh.target.host.clone(),
            port: ssh.target.port,
            username: ssh.target.username.clone(),
            auth: export_ssh_auth(&ssh.target.auth, lookup),
            jump: ssh.jump.as_ref().map(|jump| CredentialSshHop {
                host: jump.host.clone(),
                port: jump.port,
                username: jump.username.clone(),
                auth: export_ssh_auth(&jump.auth, lookup),
            }),
            keepalive_seconds: ssh.keepalive.interval_seconds,
            keepalive_max: ssh.keepalive.max_missed,
        }),
        kube: chain.kube.as_ref().map(|kube| CredentialKube {
            kubeconfig: kube.kubeconfig.clone(),
            context: kube.context.clone(),
            namespace: kube.namespace.clone(),
            target_kind: kube.target_kind,
            target_name: kube.target_name.clone(),
            remote_port: kube.remote_port,
        }),
        ssm: chain.ssm.as_ref().map(|ssm| CredentialSsm {
            target: ssm.target.clone(),
            region: ssm.region.clone(),
            profile: ssm.profile.clone(),
            remote_host: ssm.remote_host.clone(),
        }),
    }
}

fn export_mask_rules(rules: &[MaskRule]) -> Vec<CredentialMaskRule> {
    rules
        .iter()
        .map(|rule| match rule.scope {
            RuleScope::ColumnName => CredentialMaskRule::Pattern(rule.pattern.clone()),
            RuleScope::TableDotColumn => CredentialMaskRule::Scoped {
                pattern: rule.pattern.clone(),
                scope: CredentialMaskScope::TableDotColumn,
            },
        })
        .collect()
}

/// Maps a saved profile to a credential-file connection. `sql_account`
/// names a shared login to reference instead of an inline password;
/// `tunnel_profile` names the tunnel profile the connection uses.
pub fn export_connection(
    profile: &ConnectionProfile,
    sql_account: Option<&str>,
    tunnel_profile: Option<&str>,
    lookup: &mut SecretLookup<'_>,
) -> CredentialConnection {
    let file_engine = is_file_engine(profile.database_type);
    let authentication = match &profile.authentication {
        AuthenticationType::SqlLogin { .. } if file_engine => CredentialAuth::None,
        AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } => match sql_account {
            Some(account) => CredentialAuth::SqlLogin {
                username: None,
                password: None,
                account: Some(account.to_string()),
            },
            None => CredentialAuth::SqlLogin {
                username: Some(username.clone()),
                password: export_secret(secret_ref, lookup),
                account: None,
            },
        },
        AuthenticationType::Integrated => CredentialAuth::Integrated,
        AuthenticationType::EntraBrowser => CredentialAuth::EntraBrowser,
        AuthenticationType::EntraDeviceCode => CredentialAuth::EntraDeviceCode,
        AuthenticationType::AwsRdsIam {
            username,
            region,
            profile,
        } => CredentialAuth::AwsRdsIam {
            username: username.clone(),
            region: region.clone(),
            profile: profile.clone(),
        },
        AuthenticationType::GcpCloudSqlIam { username } => CredentialAuth::GcpCloudSqlIam {
            username: username.clone(),
        },
        AuthenticationType::AuthToken { secret_ref } => CredentialAuth::AuthToken {
            token: export_secret(secret_ref, lookup),
        },
    };
    let tls = if profile.tls_mode == TlsMode::Legacy {
        CredentialTls {
            mode: None,
            encrypt: Some(profile.encrypt),
            trust_server_certificate: Some(profile.trust_server_certificate),
            ..CredentialTls::default()
        }
    } else {
        CredentialTls {
            mode: Some(match profile.tls_mode {
                TlsMode::Disable => CredentialTlsMode::Disable,
                TlsMode::Require => CredentialTlsMode::Require,
                TlsMode::VerifyCa => CredentialTlsMode::VerifyCa,
                _ => CredentialTlsMode::VerifyFull,
            }),
            ..CredentialTls::default()
        }
    };
    let tls = CredentialTls {
        ca_file: profile.tls_ca_file.clone(),
        client_cert_file: profile.tls_client_cert_file.clone(),
        client_key_file: profile.tls_client_key_file.clone(),
        trust_os_roots: profile.tls_trust_os_roots.then_some(true),
        direct: profile.tls_direct.then_some(true),
        ..tls
    };
    let transport = if tunnel_profile.is_some() {
        None
    } else {
        let chain = profile.inline_transport();
        (!chain.is_empty()).then(|| export_transport(&chain, lookup))
    };
    CredentialConnection {
        name: profile.name.clone(),
        engine: engine_export_name(profile.database_type),
        server: profile.server.clone(),
        port: (!file_engine).then_some(profile.port),
        database: profile.database.clone(),
        environment: Some(environment_export_name(profile.environment).to_string()),
        client: profile.client.clone(),
        project: profile.project.clone(),
        authentication: Some(authentication),
        tls,
        timeout_seconds: Some(profile.timeout_seconds),
        statement_timeout_seconds: profile.statement_timeout_seconds,
        application_name: Some(profile.application_name.clone()),
        safety: CredentialConnectionSafety {
            auto_wrap_rollback: profile.auto_wrap_rollback,
            block_all_but_select: profile.block_all_but_select,
            confirm_data_changes: profile.confirm_data_changes,
            allow_unbounded_mutations: profile.allow_unbounded_mutations,
            production_override: profile.production_override,
        },
        favorite: profile.favorite,
        startup_sql: profile.startup_sql.clone(),
        notes: profile.notes.clone(),
        redis_key_delimiter: profile.redis_key_delimiter.clone(),
        redis_sentinel: profile
            .redis_sentinel
            .as_ref()
            .map(crate::redis_sentinel::RedisSentinelConfig::without_secret),
        redis_sentinel_password: None,
        redis_cluster: profile.redis_cluster.clone(),
        redis_entra_user: profile.redis_entra_user.clone(),
        clickhouse_native_port: profile.clickhouse_native_port,
        entra_account: profile
            .entra_account
            .as_ref()
            .map(|binding| CredentialEntraAccount {
                account_id: binding.account_id.clone(),
                tenant_id: binding.tenant_id.clone(),
                username: binding.username.clone(),
            }),
        route: match &profile.route {
            ConnectionRoute::Direct => None,
            ConnectionRoute::ViaRelay {
                relay_device_id,
                pair_id,
                relay_hint,
            } => Some(CredentialRoute::ViaRelay {
                relay_device_id: relay_device_id.clone(),
                pair_id: pair_id.clone(),
                relay_hint: relay_hint.clone(),
            }),
        },
        tunnel_profile: tunnel_profile.map(str::to_string),
        transport,
        masking: CredentialMasking {
            include: export_mask_rules(&profile.masking.include),
            exclude: export_mask_rules(&profile.masking.exclude),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1×1 transparent PNG.
    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn port(engine: DatabaseType) -> u16 {
        match engine {
            DatabaseType::Postgres => 5432,
            _ => 1433,
        }
    }

    fn parse(json: &str) -> Result<CredentialImportPlan, CredentialImportError> {
        parse_credential_file(json, port)
    }

    fn full_file() -> String {
        format!(
            r##"{{
  "format": "sqlly-credentials",
  "version": 1,
  "clients": [{{ "name": "Acme", "color": "#1e88e5", "image": "data:image/png;base64,{PNG_B64}", "safety": {{ "only_allow_select": true }} }}],
  "projects": [{{ "name": "Billing", "color": "F80" }}],
  "environments": [{{ "name": "Production", "color": "#C62828", "image": "{PNG_B64}" }}],
  "sql_auth_accounts": [{{ "name": "Shared DBA", "username": "dba", "password": "dba-pass" }}],
  "tunnel_profiles": [{{ "name": "Corp bastion", "ssh": {{ "host": "bastion.corp", "username": "jump", "auth": {{ "type": "password", "password": "ssh-pass" }} }} }}],
  "connections": [
    {{
      "name": "Billing prod",
      "engine": "postgresql",
      "server": "db.example.com",
      "database": "billing",
      "environment": "Production",
      "client": "Acme",
      "project": "Billing",
      "authentication": {{ "type": "sql-login", "username": "app", "password": "p@ss word" }},
      "tls": {{ "mode": "verify-full", "ca_file": "/etc/ssl/ca.pem", "trust_os_roots": true, "direct": true }},
      "timeout_seconds": 45,
      "statement_timeout_seconds": 300,
      "application_name": "Billing",
      "safety": {{ "confirm_data_changes": true, "allow_unbounded_mutations": false, "production_override": true }},
      "favorite": true,
      "startup_sql": "SET search_path TO billing;",
      "notes": "Owned by the billing team.\nRead replica only after 18:00.",
      "transport": {{
        "proxy": {{ "kind": "http-connect", "host": "proxy.corp", "port": 8080, "username": "pu", "password": "proxy-pass" }},
        "ssh": {{ "host": "bastion", "port": 2222, "username": "u", "auth": {{ "type": "key-file", "path": "~/.ssh/id_ed25519", "passphrase": "kp" }},
                 "jump": {{ "host": "edge", "username": "e" }}, "keepalive_seconds": 15 }}
      }},
      "masking": {{ "include": ["*ssn*", {{ "pattern": "users.email", "scope": "table-dot-column" }}] }}
    }},
    {{ "name": "Shared", "engine": "SQL Server", "server": "sql01", "authentication": {{ "type": "sql-login", "account": "Shared DBA" }}, "tunnel_profile": "Corp bastion" }},
    {{ "name": "Local file", "engine": "sqlite", "server": "/tmp/app.db" }},
    {{ "name": "Turso", "engine": "libsql", "server": "db.turso.io", "port": 443, "authentication": {{ "type": "auth-token", "token": "jwt" }} }}
  ]
}}"##
        )
    }

    #[test]
    fn full_file_plans_every_setting() {
        let plan = parse(&full_file()).expect("valid file");
        assert_eq!(plan.clients[0].color_hex.as_deref(), Some("#1E88E5"));
        assert_eq!(plan.clients[0].image.as_ref().unwrap().extension, "png");
        assert_eq!(plan.clients[0].safety.only_allow_select, Some(true));
        assert_eq!(plan.clients[0].safety.is_production, None);
        assert_eq!(plan.projects[0].color_hex.as_deref(), Some("#FF8800"));
        assert_eq!(
            plan.environments[0].image.as_ref().unwrap().extension,
            "png"
        );
        assert_eq!(
            plan.sql_auth_accounts[0].password.as_deref(),
            Some("dba-pass")
        );
        assert!(matches!(
            plan.tunnel_profiles[0].chain.ssh.as_ref().unwrap().target.auth,
            SshAuth::Password { ref secret_ref } if secret_ref.key == "ssh-pass"
        ));

        let billing = &plan.connections[0].profile;
        assert_eq!(billing.database_type, DatabaseType::Postgres);
        assert_eq!(billing.port, 5432, "omitted port uses the engine default");
        assert_eq!(billing.environment, ConnectionEnvironment::Production);
        assert_eq!(billing.provenance, ConnectionProvenance::Imported);
        let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &billing.authentication
        else {
            panic!("expected sql login");
        };
        assert_eq!(username, "app");
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert_eq!(secret_ref.key, "p@ss word", "passwords are never trimmed");
        assert_eq!(billing.tls_mode, TlsMode::VerifyFull);
        assert!(billing.encrypt && !billing.trust_server_certificate);
        assert_eq!(billing.timeout_seconds, 45);
        assert_eq!(billing.statement_timeout_seconds, Some(300));
        assert_eq!(billing.application_name, "Billing");
        assert!(billing.confirm_data_changes && !billing.allow_unbounded_mutations);
        assert_eq!(billing.production_override, Some(true));
        assert!(billing.favorite);
        assert_eq!(
            billing.notes.as_deref(),
            Some("Owned by the billing team.\nRead replica only after 18:00.")
        );
        let proxy = billing.transport.proxy.as_ref().unwrap();
        assert_eq!(proxy.kind, ProxyKind::HttpConnect);
        assert_eq!(proxy.password.as_ref().unwrap().key, "proxy-pass");
        let ssh = billing.transport.ssh.as_ref().unwrap();
        assert_eq!(ssh.target.port, 2222);
        assert_eq!(ssh.jump.as_ref().unwrap().port, 22);
        assert_eq!(ssh.keepalive.interval_seconds, Some(15));
        assert_eq!(ssh.keepalive.max_missed, None);
        assert!(billing.tls_trust_os_roots);
        assert!(billing.tls_direct);
        assert_eq!(billing.ssh_tunnel.as_ref(), Some(ssh));
        assert_eq!(billing.masking.include.len(), 2);
        assert_eq!(billing.masking.include[1].scope, RuleScope::TableDotColumn);

        let shared = &plan.connections[1];
        assert_eq!(shared.profile.database_type, DatabaseType::SqlServer);
        assert_eq!(shared.sql_account.as_deref(), Some("Shared DBA"));
        assert_eq!(shared.tunnel_profile.as_deref(), Some("Corp bastion"));

        let file = &plan.connections[2].profile;
        assert_eq!(file.database_type, DatabaseType::Sqlite);
        assert_eq!(file.port, 0);

        let turso = &plan.connections[3].profile;
        assert!(matches!(
            &turso.authentication,
            AuthenticationType::AuthToken { secret_ref } if secret_ref.key == "jwt"
        ));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let json = r##"{
  "format": "sqlly-credentials", "version": 1,
  "clients": [{ "name": "A", "color": "blue", "image": "bm90IGFuIGltYWdl" }],
  "connections": [
    { "name": "x", "engine": "nosuchdb", "server": "h" },
    { "name": "y", "engine": "postgres", "server": "", "authentication": { "type": "integrated" } },
    { "name": "z", "engine": "neon", "server": "h", "tls": { "encrypt": false }, "authentication": { "type": "sql-login", "username": "u" } }
  ]
}"##;
        let error = parse(json).unwrap_err();
        let all = error.problems.join("\n");
        assert!(all.contains("color \"blue\""), "{all}");
        assert!(all.contains("not a recognized image"), "{all}");
        assert!(all.contains("unknown engine \"nosuchdb\""), "{all}");
        assert!(all.contains("\"y\": server is required"), "{all}");
        assert!(
            all.contains("\"y\": this sign-in method is not supported"),
            "{all}"
        );
        assert!(all.contains("requires an encrypted connection"), "{all}");
        assert_eq!(error.problems.len(), 6, "{all}");
    }

    #[test]
    fn typos_are_rejected_instead_of_ignored() {
        let json = r#"{ "format": "sqlly-credentials", "version": 1,
            "connections": [{ "name": "x", "engine": "postgres", "server": "h", "pasword": "oops" }] }"#;
        let error = parse(json).unwrap_err();
        assert!(error.problems[0].contains("pasword"), "{error}");
    }

    #[test]
    fn wrong_format_or_newer_version_is_refused() {
        let error = parse(r#"{ "format": "other", "version": 1 }"#).unwrap_err();
        assert!(error.problems[0].contains("format must be"));
        let error = parse(r#"{ "format": "sqlly-credentials", "version": 9 }"#).unwrap_err();
        assert!(error.problems[0].contains("version 9"));
    }

    #[test]
    fn sniff_only_matches_credential_files() {
        assert!(is_credential_file(&full_file()));
        assert!(!is_credential_file(r#"{"version":1,"profiles":[]}"#));
        assert!(!is_credential_file("not json"));
    }

    #[test]
    fn engine_names_are_forgiving() {
        assert_eq!(parse_engine("sql-server"), Some(DatabaseType::SqlServer));
        assert_eq!(parse_engine("SqlServer"), Some(DatabaseType::SqlServer));
        assert_eq!(parse_engine("mssql"), Some(DatabaseType::SqlServer));
        assert_eq!(parse_engine("PostgreSQL"), Some(DatabaseType::Postgres));
        assert_eq!(parse_engine("click_house"), Some(DatabaseType::ClickHouse));
        assert_eq!(parse_engine("duckdb"), Some(DatabaseType::DuckDb));
        assert_eq!(parse_engine("nope"), None);
    }

    #[test]
    fn colors_normalize_and_images_sniff() {
        assert_eq!(normalize_hex_color("abc").as_deref(), Some("#AABBCC"));
        assert_eq!(
            normalize_hex_color("#11223344").as_deref(),
            Some("#11223344")
        );
        assert_eq!(normalize_hex_color("#12345"), None);
        assert_eq!(normalize_hex_color("#gggggg"), None);

        let svg = base64::engine::general_purpose::STANDARD
            .encode(br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"/>"#);
        assert_eq!(decode_image(&svg).unwrap().extension, "svg");
        let uri = format!("data:image/svg+xml;base64,{svg}");
        assert_eq!(decode_image(&uri).unwrap().extension, "svg");
        assert!(decode_image("data:image/png,rawtext").is_err());
        let huge =
            base64::engine::general_purpose::STANDARD.encode(vec![0x89; MAX_IMAGE_BYTES + 1]);
        assert!(decode_image(&huge).unwrap_err().contains("limit"));
    }

    #[test]
    fn sql_login_account_and_password_are_exclusive() {
        let json = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "x", "engine": "postgres", "server": "h",
              "authentication": { "type": "sql-login", "account": "A", "password": "p" } }] }"#;
        let error = parse(json).unwrap_err();
        assert!(
            error.problems[0].contains("either account or username"),
            "{error}"
        );
    }

    #[test]
    fn inline_ssm_transport_imports_and_exports() {
        let json = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "x", "engine": "postgres", "server": "db.internal",
              "authentication": { "type": "sql-login", "username": "u" },
              "transport": { "ssm": { "target": " i-0abc ", "region": "us-east-2", "profile": "" } } }] }"#;
        let plan = parse(json).expect("valid");
        let chain = &plan.connections[0].profile.transport;
        let ssm = chain.ssm.as_ref().expect("ssm hop");
        assert_eq!(ssm.target, "i-0abc");
        assert_eq!(ssm.region.as_deref(), Some("us-east-2"));
        assert_eq!(ssm.profile, None);
        let mut lookup = |_: &SecretRef| None;
        let exported = export_transport(chain, &mut lookup);
        assert_eq!(exported.ssm.expect("exported").target, "i-0abc");

        let bad = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "x", "engine": "postgres", "server": "h",
              "authentication": { "type": "sql-login", "username": "u" },
              "transport": { "ssm": { "target": "" } } }] }"#;
        assert!(
            parse(bad)
                .unwrap_err()
                .problems
                .iter()
                .any(|p| p.contains("Session Manager target is required"))
        );
    }

    #[test]
    fn tunnel_profile_and_inline_transport_are_exclusive() {
        let json = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "x", "engine": "postgres", "server": "h",
              "authentication": { "type": "sql-login", "username": "u" },
              "tunnel_profile": "T",
              "transport": { "ssh": { "host": "b", "username": "u" } } }] }"#;
        let error = parse(json).unwrap_err();
        assert!(error.problems[0].contains("not both"), "{error}");
    }

    #[test]
    fn every_engine_name_round_trips() {
        for engine in DatabaseType::ALL {
            let name = engine_export_name(engine);
            assert_eq!(parse_engine(&name), Some(engine), "{name}");
        }
        assert_eq!(
            engine_export_name(DatabaseType::SqlServer),
            "microsoft-sql-server"
        );
        assert_eq!(engine_export_name(DatabaseType::DuckDb), "duckdb");
        assert_eq!(engine_export_name(DatabaseType::Postgres), "postgresql");
    }

    /// Export → JSON → import reproduces the profile (secrets resolved from
    /// the in-memory values the plan carries).
    #[test]
    fn exported_connections_import_back_unchanged() {
        let plan = parse(&full_file()).unwrap();
        let mut lookup = |r: &SecretRef| Some(r.key.clone());
        let file = CredentialFile {
            format: CREDENTIAL_FILE_FORMAT.to_string(),
            version: CREDENTIAL_FILE_VERSION,
            clients: Vec::new(),
            projects: Vec::new(),
            environments: Vec::new(),
            sql_auth_accounts: Vec::new(),
            tunnel_profiles: Vec::new(),
            connections: plan
                .connections
                .iter()
                .filter(|c| c.sql_account.is_none() && c.tunnel_profile.is_none())
                .map(|c| export_connection(&c.profile, None, None, &mut lookup))
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file).unwrap();
        assert!(is_credential_file(&json));
        let again = parse(&json).unwrap_or_else(|e| panic!("{e}\n{json}"));
        let originals: Vec<_> = plan
            .connections
            .iter()
            .filter(|c| c.sql_account.is_none() && c.tunnel_profile.is_none())
            .collect();
        assert_eq!(again.connections.len(), originals.len());
        for (a, b) in originals.iter().zip(&again.connections) {
            let mut expected = a.profile.clone();
            let mut actual = b.profile.clone();
            expected.id = actual.id;
            // Unset environment exports as "unset"; everything else is exact.
            actual.status = expected.status.clone();
            assert_eq!(actual, expected, "{json}");
        }
    }

    /// An export without credentials keeps every sign-in method (an SSH
    /// password hop stays a password hop) and parses back, with the missing
    /// secrets left empty for the app to ask for.
    #[test]
    fn exports_without_secrets_keep_the_sign_in_methods() {
        let plan = parse(&full_file()).unwrap();
        let mut none = |_: &SecretRef| None;
        let tunnel = &plan.tunnel_profiles[0];
        let transport = export_transport(&tunnel.chain, &mut none);
        let ssh = transport.ssh.as_ref().unwrap();
        assert!(
            matches!(ssh.auth, CredentialSshAuth::Password { password: None }),
            "{:?}",
            ssh.auth
        );
        let file = CredentialFile {
            format: CREDENTIAL_FILE_FORMAT.to_string(),
            version: CREDENTIAL_FILE_VERSION,
            clients: Vec::new(),
            projects: Vec::new(),
            environments: Vec::new(),
            sql_auth_accounts: Vec::new(),
            tunnel_profiles: vec![CredentialTunnelProfile {
                name: tunnel.name.clone(),
                proxy: transport.proxy,
                ssh: transport.ssh,
                kube: transport.kube,
                ssm: transport.ssm,
            }],
            connections: plan
                .connections
                .iter()
                .filter(|c| c.sql_account.is_none() && c.tunnel_profile.is_none())
                .map(|c| export_connection(&c.profile, None, None, &mut none))
                .collect(),
        };
        let json = serde_json::to_string_pretty(&file).unwrap();
        for secret in ["ssh-pass", "p@ss word", "proxy-pass", "dba-pass"] {
            assert!(!json.contains(secret), "{secret} leaked:\n{json}");
        }
        let again = parse(&json).unwrap_or_else(|e| panic!("{e}\n{json}"));
        assert!(matches!(
            again.tunnel_profiles[0].chain.ssh.as_ref().unwrap().target.auth,
            SshAuth::Password { ref secret_ref }
                if secret_ref.provider == SecretProvider::InMemory && secret_ref.key.is_empty()
        ));
    }

    #[test]
    fn image_data_uri_round_trips() {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(PNG_B64)
            .unwrap();
        let uri = encode_image_data_uri(&bytes).unwrap();
        assert!(uri.starts_with("data:image/png;base64,"));
        assert_eq!(decode_image(&uri).unwrap().bytes, bytes);
        assert!(encode_image_data_uri(b"plain text").is_none());
    }
}
