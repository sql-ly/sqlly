use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlly_lang::sqlmodel::{DatabaseType, DbTypeMetadata};
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub Uuid);

impl ConnectionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthenticationType {
    SqlLogin {
        username: String,
        secret_ref: SecretRef,
    },
    Integrated,
    EntraDeviceCode,
    EntraBrowser,
    /// Amazon RDS / Aurora IAM database authentication: the password is a
    /// 15-minute SigV4 token minted from local AWS credentials on connect.
    AwsRdsIam {
        /// Database user created with `rds_iam` (PG) / AWSAuthenticationPlugin (MySQL).
        username: String,
        /// AWS region of the instance, e.g. `us-west-2`.
        region: String,
        /// Named profile from `~/.aws/config`; `None` = default credential chain.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    /// Google Cloud SQL IAM database authentication: the password is an OAuth
    /// access token (scope sqlservice.login) from Application Default Credentials.
    GcpCloudSqlIam {
        /// IAM principal as the database expects it (PG: full email minus
        /// `.gserviceaccount.com`; MySQL: local part only).
        username: String,
    },
    /// A bearer token with no user name: a Turso database token or a
    /// self-hosted `sqld` JWT (libSQL). Stored like a password — the
    /// reference points at the keyring, never at the token itself. An empty
    /// in-memory reference means "no token" (anonymous local server).
    AuthToken {
        secret_ref: SecretRef,
    },
}

impl AuthenticationType {
    pub fn is_cloud_iam(&self) -> bool {
        matches!(
            self,
            AuthenticationType::AwsRdsIam { .. } | AuthenticationType::GcpCloudSqlIam { .. }
        )
    }

    pub fn username(&self) -> Option<&str> {
        match self {
            AuthenticationType::SqlLogin { username, .. }
            | AuthenticationType::AwsRdsIam { username, .. }
            | AuthenticationType::GcpCloudSqlIam { username } => Some(username),
            AuthenticationType::Integrated
            | AuthenticationType::EntraDeviceCode
            | AuthenticationType::EntraBrowser
            | AuthenticationType::AuthToken { .. } => None,
        }
    }

    /// The keyring/in-memory reference behind a password-like secret (SQL
    /// login password, auth token), or `None` for token-cache sign-ins.
    pub fn secret_ref(&self) -> Option<&SecretRef> {
        match self {
            AuthenticationType::SqlLogin { secret_ref, .. }
            | AuthenticationType::AuthToken { secret_ref } => Some(secret_ref),
            _ => None,
        }
    }

    /// Mutable form of [`Self::secret_ref`], for hydrating a keyring
    /// reference into a runtime-only in-memory value.
    pub fn secret_ref_mut(&mut self) -> Option<&mut SecretRef> {
        match self {
            AuthenticationType::SqlLogin { secret_ref, .. }
            | AuthenticationType::AuthToken { secret_ref } => Some(secret_ref),
            _ => None,
        }
    }

    pub fn requires_tls(&self) -> bool {
        self.is_cloud_iam()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConnectionEnvironment {
    #[default]
    Local,
    Dev,
    Test,
    Staging,
    Production,
    /// No environment chosen. Used by the connection editor when the user has
    /// not selected (or has explicitly cleared) the environment chip. `Local`
    /// remains the serde `#[default]` for backward-compatible deserialization
    /// of stores authored before `Unset` existed.
    Unset,
}

impl ConnectionEnvironment {
    /// Built-in environment entity name in the connection store (case-insensitive match).
    pub fn entity_name(self) -> Option<&'static str> {
        match self {
            Self::Production => Some("Production"),
            Self::Staging => Some("Staging"),
            Self::Test => Some("Test"),
            Self::Dev => Some("Dev"),
            Self::Local | Self::Unset => None,
        }
    }

    /// Whether an environment entity named `name` is this environment's entity
    /// (case-insensitive). Dev also answers to "Development", the name older
    /// builds and some hand-written credential files used, so protections set
    /// on either spelling reach DEV connections.
    pub fn matches_entity_name(self, name: &str) -> bool {
        let name = name.trim();
        match self.entity_name() {
            Some(canonical) if canonical.eq_ignore_ascii_case(name) => true,
            _ => self == Self::Dev && name.eq_ignore_ascii_case("Development"),
        }
    }

    /// The entity for this environment among `entities` (named by `name_of`),
    /// preferring an exact match on the built-in name over an alias.
    pub fn find_entity<T>(self, entities: &[T], name_of: impl Fn(&T) -> &str) -> Option<&T> {
        let canonical = self.entity_name()?;
        entities
            .iter()
            .find(|e| name_of(e).trim().eq_ignore_ascii_case(canonical))
            .or_else(|| {
                entities
                    .iter()
                    .find(|e| self.matches_entity_name(name_of(e)))
            })
    }
}

/// Where a [`ConnectionProfile`] originated. Provenance drives UI affordances
/// (e.g. discovered profiles can be re-linked; imported ones may need secret
/// re-entry) and is never inferred from secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionProvenance {
    /// Created by hand in the connection editor.
    #[default]
    Manual,
    /// Created from an Azure SQL discovery result.
    AzureDiscovered,
    /// Imported from an exported profile bundle (secrets re-entered separately).
    Imported,
    /// Loaded from a filesystem-backed project/workspace.
    Filesystem,
    /// Carried over from an earlier on-disk schema version.
    Migrated,
    /// Handed to a browser session by a relay proxy, which holds the
    /// credentials; the profile itself is read-only and secret-free.
    Proxied,
    /// Defined by a provisioning file or `SQLLY_CONNECTION_*` environment
    /// variable (see [`crate::provisioning`]): rebuilt on every launch, never
    /// written to the saved connections file, and read-only in the app.
    Provisioned,
}

/// Binds a profile to a specific Microsoft Entra account so that the same
/// username across different tenants does not silently reuse the wrong token
/// cache. Held secret-free: only stable identifiers are stored here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntraAccountBinding {
    /// Stable account id (`home_account_id:tenant_id`), matching
    /// `sqlly_services::security::azure::EntraAccountId`.
    pub account_id: String,
    pub tenant_id: String,
    /// Optional UPN/username for display; never a secret.
    pub username: Option<String>,
}

/// Outcome of the most recent connection test, kept on the profile so the
/// connection-management screen can show health at a glance. Never carries
/// secret material — only a short, redacted error summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionTestStatus {
    #[default]
    Untested,
    Succeeded,
    Failed,
}

/// Secret-free, observed runtime status for a profile. Updated by the
/// connection service after connects/tests; serialized alongside the profile.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ConnectionStatus {
    pub last_connected_at: Option<DateTime<Utc>>,
    pub last_tested_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub last_test_status: Option<ConnectionTestStatus>,
    /// A short, human-readable, secret-free summary of the last failure.
    /// English for app-authored failures (see `last_error_message`), so
    /// logs, the CLI, and older builds can always show it.
    pub last_error_summary: Option<String>,
    /// Catalog key and arguments of the last failure when the app authored
    /// it, so a localized UI can render it in the active language. Absent
    /// for raw driver/server text and in records written by older builds;
    /// those show `last_error_summary` as is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error_message: Option<StoredMessage>,
}

/// A persisted translation-catalog message: the key plus its named
/// arguments. The UI re-renders it in the active language and falls back to
/// the English summary stored beside it when the key is unknown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredMessage {
    pub key: String,
    #[serde(default)]
    pub arguments: Vec<(String, String)>,
}

impl ConnectionStatus {
    /// Records a successful connection at `now`, clearing any prior error.
    pub fn record_connected(&mut self, now: DateTime<Utc>) {
        self.last_connected_at = Some(now);
        self.last_tested_at = Some(now);
        self.last_test_status = Some(ConnectionTestStatus::Succeeded);
        self.last_error_summary = None;
        self.last_error_message = None;
    }

    /// Records a successful test at `now`, clearing any prior error.
    pub fn record_test_success(&mut self, now: DateTime<Utc>) {
        self.last_tested_at = Some(now);
        self.last_test_status = Some(ConnectionTestStatus::Succeeded);
        self.last_error_summary = None;
        self.last_error_message = None;
    }

    /// Records a failed test/connect, storing a redacted, length-bounded summary
    /// so no secret can leak through an overly verbose driver error.
    pub fn record_failure(&mut self, now: DateTime<Utc>, summary: impl Into<String>) {
        self.last_tested_at = Some(now);
        self.last_test_status = Some(ConnectionTestStatus::Failed);
        self.last_error_summary = Some(redact_error_summary(&summary.into()));
        self.last_error_message = None;
    }

    /// [`Self::record_failure`] for a presentation message: the English
    /// fallback becomes the summary and, when the message is keyed, its key
    /// and (redacted) arguments are kept so the UI can localize it later.
    pub fn record_failure_message(&mut self, now: DateTime<Utc>, message: &crate::DisplayMessage) {
        self.record_failure(now, message.fallback());
        self.last_error_message = message.key().map(|key| StoredMessage {
            key: key.to_string(),
            arguments: message
                .arguments()
                .map(|(name, value)| (name.to_string(), redact_error_summary(value)))
                .collect(),
        });
    }
}

/// Produces a short, secret-free error summary suitable for persistence and
/// display. Strips anything that looks like an inline `password=` / `pwd=`
/// token and caps the length so a chatty driver cannot smuggle a secret in.
pub fn redact_error_summary(raw: &str) -> String {
    const MAX: usize = 200;
    let mut out = String::with_capacity(raw.len().min(MAX));
    // Redact `key=value` pairs for sensitive keys. Each replacement changes
    // the string's length, so offsets are re-derived from the *current* text
    // on every pass — replacing at positions found in the original string
    // would drift after the first substitution (and could split a char
    // boundary). `to_ascii_lowercase` is byte-length-preserving, so indexes
    // found in the lowered copy are valid in the original.
    let mut redacted = raw.to_string();
    for key in ["password", "pwd", "secret", "token", "access_token"] {
        let mut search_from = 0;
        loop {
            let lower = redacted.to_ascii_lowercase();
            let Some(pos) = lower[search_from..].find(key).map(|p| p + search_from) else {
                break;
            };
            // Replace from the key through the next whitespace/semicolon.
            let end = redacted[pos..]
                .find([' ', ';', '&', '\n'])
                .map(|e| pos + e)
                .unwrap_or(redacted.len());
            let replacement = format!("{key}=<redacted>");
            let replacement_len = replacement.len();
            redacted.replace_range(pos..end, &replacement);
            search_from = pos + replacement_len;
        }
    }
    for ch in redacted.chars() {
        if out.len() >= MAX {
            out.push('…');
            break;
        }
        // Collapse newlines/control chars into spaces.
        if ch.is_control() {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out.trim().to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AzureSqlConnectionDefaults {
    pub port: u16,
    pub database: String,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
    pub timeout_seconds: u64,
    pub application_name: String,
}

impl Default for AzureSqlConnectionDefaults {
    fn default() -> Self {
        Self {
            port: 1433,
            database: "master".to_string(),
            encrypt: true,
            trust_server_certificate: false,
            timeout_seconds: 30,
            application_name: "SQLLY".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretRef {
    pub provider: SecretProvider,
    pub key: String,
}

impl SecretRef {
    /// Resolve the secret value this reference points at — the single
    /// resolution boundary shared by every driver and the remote engine
    /// serializer. `InMemory` carries the value itself (hydrated profiles);
    /// `Environment` reads the named environment variable (explicit CLI/headless
    /// opt-in); `MacOsKeychain` reads the OS keyring entry the app writes
    /// (`SQLLY.sql-auth` / `credential.{account_uuid}`). A missing secret is an
    /// authentication error naming the key or account — never a silent
    /// empty-string fallback, which would surface as a baffling server-side
    /// login failure instead of the real local problem.
    pub fn resolve(&self) -> crate::StudioResult<String> {
        crate::secret_resolver::resolve_secret(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretProvider {
    MacOsKeychain,
    Environment,
    InMemory,
}

/// How a [`ConnectionProfile`] reaches its target engine.
///
/// `Direct` (the default) opens a socket straight to the configured
/// `server:port`, exactly as SQLLY always has. `ViaRelay` routes the encrypted
/// engine session through a relay device: the profile stores only the stable
/// destination **device**/**pair** identifiers granted by the store — never a
/// relay hostname as identity, and never an API token. The relay endpoint is
/// discovered dynamically (`GET /remote-endpoint-list`); `relay_hint` may carry
/// an optional cached endpoint to try first, but it is advisory only.
///
/// Ids are `String` to match how the rest of the client represents
/// device/pair ids (`control_plane::ConnectorDevice::device_id`, the store
/// pairing `device_id`, and the relay route-pair id from
/// `DELETE /api/relay/pairs/{id}` are all opaque server-issued strings).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ConnectionRoute {
    /// Connect straight to `server:port`. Back-compatible default for every
    /// profile saved before routing metadata existed.
    #[default]
    Direct,
    /// Route the engine session through a relay device via an authorized pair.
    ViaRelay {
        /// Stable id of the destination relay-connector device (the bridge that
        /// fronts the real database), as issued by the store.
        relay_device_id: String,
        /// Id of the authorized route pair granting this app access to the
        /// relay device.
        pair_id: String,
        /// Optional cached relay endpoint to try first. Advisory only — the
        /// authoritative endpoint comes from dynamic relay discovery — so it is
        /// never treated as connection identity.
        #[serde(default)]
        relay_hint: Option<String>,
    },
}

/// One SSH endpoint the tunnel logs into. Secret-free: passwords and key
/// passphrases are `SecretRef`s into the OS keyring, never inline values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshHop {
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub username: String,
    #[serde(default)]
    pub auth: SshAuth,
}

fn default_ssh_port() -> u16 {
    22
}

/// Serde default for [`ConnectionProfile::allow_unbounded_mutations`]: the
/// unbounded-statement confirmation is opt-in, so profiles saved before the
/// field existed decode as "allowed" like every new profile.
fn default_allow_unbounded() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SshAuth {
    /// Sign with the running ssh-agent (SSH_AUTH_SOCK on unix).
    #[default]
    Agent,
    /// Private key file on disk; optional passphrase in the keyring.
    KeyFile {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        passphrase: Option<SecretRef>,
    },
    /// SSH password (keyring-backed).
    Password { secret_ref: SecretRef },
}

/// Per-connection SSH tunnel: the driver connects to a loopback listener that
/// forwards to `server:port` through `target` (optionally via `jump` first).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    pub target: SshHop,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<SshHop>,
    /// SSH-level keepalives for every session of this tunnel. Absent on
    /// profiles saved before the field existed, which get the defaults.
    #[serde(default, skip_serializing_if = "SshKeepalive::is_default")]
    pub keepalive: SshKeepalive,
}

/// Seconds between SSH keepalives when a profile sets none.
pub const DEFAULT_SSH_KEEPALIVE_SECONDS: u64 = 30;
/// Unanswered keepalives after which a tunnel's SSH session is closed (and
/// opened again on next use) when a profile sets none.
pub const DEFAULT_SSH_KEEPALIVE_MAX: u32 = 3;

/// How a tunnel keeps an idle SSH session alive: while nothing arrives from
/// the server for `interval_seconds`, the client sends a keepalive request;
/// after `max_missed` go unanswered, the session is treated as dropped.
/// Keepalives also stop NAT gateways and firewalls from expiring an idle
/// tunnel. `None` fields take the defaults; an interval of 0 turns
/// keepalives off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SshKeepalive {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_missed: Option<u32>,
}

impl SshKeepalive {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// The keepalive interval: the default when unset, `None` (off) for 0.
    pub fn interval(&self) -> Option<Duration> {
        match self.interval_seconds {
            None => Some(Duration::from_secs(DEFAULT_SSH_KEEPALIVE_SECONDS)),
            Some(0) => None,
            Some(seconds) => Some(Duration::from_secs(seconds)),
        }
    }

    /// Unanswered keepalives before the session is closed (at least 1).
    pub fn max_missed(&self) -> usize {
        self.max_missed.unwrap_or(DEFAULT_SSH_KEEPALIVE_MAX).max(1) as usize
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum KubeTargetKind {
    #[default]
    Pod,
    Service,
    Deployment,
}

impl KubeTargetKind {
    /// The `TYPE/` prefix kubectl expects: `pod`, `service`, `deployment`.
    pub fn kubectl_prefix(self) -> &'static str {
        match self {
            Self::Pod => "pod",
            Self::Service => "service",
            Self::Deployment => "deployment",
        }
    }

    pub const ALL: [KubeTargetKind; 3] = [Self::Pod, Self::Service, Self::Deployment];

    pub fn label(self) -> &'static str {
        match self {
            Self::Pod => "Pod",
            Self::Service => "Service",
            Self::Deployment => "Deployment",
        }
    }

    pub fn message(self) -> crate::DisplayMessage {
        let (key, fallback) = match self {
            Self::Pod => ("services.connection.kube_target.pod", "Pod"),
            Self::Service => ("services.connection.kube_target.service", "Service"),
            Self::Deployment => ("services.connection.kube_target.deployment", "Deployment"),
        };
        crate::DisplayMessage::new(key, fallback)
    }
}

/// A `kubectl port-forward` the drivers start before connecting. Secret-free:
/// cluster credentials stay in the user's kubeconfig. `server`/`port` on the
/// profile are ignored while this is set; the driver connects to the local
/// loopback port kubectl reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KubePortForward {
    /// Explicit kubeconfig path; `None` = kubectl's own default (`KUBECONFIG` or `~/.kube/config`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig: Option<String>,
    /// `None` = current-context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default = "default_kube_namespace")]
    pub namespace: String,
    #[serde(default)]
    pub target_kind: KubeTargetKind,
    pub target_name: String,
    /// Port inside the pod/service (e.g. 5432).
    pub remote_port: u16,
}

fn default_kube_namespace() -> String {
    "default".into()
}

fn kube_name_is_safe(value: &str) -> bool {
    !value.is_empty()
        && !value.chars().any(|c| {
            c.is_whitespace() || matches!(c, ';' | '&' | '|' | '$' | '`' | '\'' | '"' | '<' | '>')
        })
}

impl KubePortForward {
    /// Human-readable errors; empty = valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.namespace.trim().is_empty() {
            errors.push("Kubernetes namespace is required".to_string());
        } else if !kube_name_is_safe(self.namespace.trim()) {
            errors.push("Kubernetes namespace contains invalid characters".to_string());
        }
        if self.target_name.trim().is_empty() {
            errors.push("Kubernetes target name is required".to_string());
        } else if !kube_name_is_safe(self.target_name.trim()) {
            errors.push("Kubernetes target name contains invalid characters".to_string());
        }
        if self.remote_port == 0 {
            errors.push("Remote port must be 1–65535".to_string());
        }
        if let Some(ctx) = &self.context {
            if ctx.trim().is_empty() {
                errors.push("Kubernetes context cannot be empty when set".to_string());
            } else if !kube_name_is_safe(ctx.trim()) {
                errors.push("Kubernetes context contains invalid characters".to_string());
            }
        }
        if let Some(path) = &self.kubeconfig
            && path.trim().is_empty()
        {
            errors.push("Kubeconfig path cannot be empty when set".to_string());
        }
        errors
    }

    /// `kubectl` argv (without the binary): `[--kubeconfig X] [--context C] -n NS port-forward --address 127.0.0.1 KIND/NAME :PORT`.
    pub fn kubectl_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if let Some(path) = self.kubeconfig.as_ref().filter(|p| !p.trim().is_empty()) {
            args.push("--kubeconfig".to_string());
            args.push(path.trim().to_string());
        }
        if let Some(ctx) = self.context.as_ref().filter(|c| !c.trim().is_empty()) {
            args.push("--context".to_string());
            args.push(ctx.trim().to_string());
        }
        args.push("-n".to_string());
        args.push(self.namespace.trim().to_string());
        args.push("port-forward".to_string());
        args.push("--address".to_string());
        args.push("127.0.0.1".to_string());
        args.push(format!(
            "{}/{}",
            self.target_kind.kubectl_prefix(),
            self.target_name.trim()
        ));
        args.push(format!(":{}", self.remote_port));
        args
    }

    pub fn summary_message(&self) -> crate::DisplayMessage {
        let current = self
            .context
            .as_deref()
            .is_none_or(|context| context.trim().is_empty());
        let message = crate::DisplayMessage::new(
            if current {
                "services.connection.kube.summary_current"
            } else {
                "services.connection.kube.summary"
            },
            self.summary(),
        );
        let message = if current {
            message
        } else {
            message.with_arg("context", self.context.as_deref().unwrap_or_default())
        };
        message
            .with_arg("namespace", self.namespace.trim())
            .with_arg("target_kind", self.target_kind.label())
            .with_arg("target_prefix", self.target_kind.kubectl_prefix())
            .with_arg("target_name", self.target_name.trim())
            .with_arg("remote_port", self.remote_port.to_string())
    }

    /// English compatibility fallback; localized callers use [`Self::summary_message`].
    pub fn summary(&self) -> String {
        let ctx = self
            .context
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or("(current)");
        format!(
            "{}/{}/{} {}/{}:{}",
            ctx,
            self.namespace.trim(),
            self.target_kind.label(),
            self.target_kind.kubectl_prefix(),
            self.target_name.trim(),
            self.remote_port
        )
    }
}

/// An AWS Systems Manager Session Manager port-forward the drivers start
/// before connecting (`aws ssm start-session` with the
/// `AWS-StartPortForwardingSessionToRemoteHost` document). Secret-free: AWS
/// credentials stay in the user's AWS CLI configuration. The session reaches
/// `remote_host` (default: the profile's server) on the profile's port from
/// the target managed node; the driver connects to the local port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SsmPortForward {
    /// Managed node to start the session on: an EC2 instance id
    /// (`i-0123…`), a hybrid node id (`mi-…`), or an ECS task target
    /// (`ecs:cluster_task_container`).
    #[serde(default)]
    pub target: String,
    /// `None` = the AWS CLI's own default region for the profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Named profile from `~/.aws/config`; `None` = the default chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Host the node forwards to; `None` = the connection's own server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_host: Option<String>,
}

/// The Session Manager document that forwards to a host other than the node.
pub const SSM_REMOTE_HOST_DOCUMENT: &str = "AWS-StartPortForwardingSessionToRemoteHost";

fn ssm_token_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '@' | '/'))
}

fn ssm_host_is_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '[' | ']'))
}

fn non_blank(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

impl SsmPortForward {
    /// Human-readable errors; empty = valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        let target = self.target.trim();
        if target.is_empty() {
            errors.push("AWS Session Manager target is required".to_string());
        } else if !ssm_token_is_safe(target) {
            errors.push("AWS Session Manager target contains invalid characters".to_string());
        }
        if let Some(region) = non_blank(&self.region)
            && !ssm_token_is_safe(region)
        {
            errors.push("AWS region contains invalid characters".to_string());
        }
        if let Some(profile) = non_blank(&self.profile)
            && !ssm_token_is_safe(profile)
        {
            errors.push("AWS profile contains invalid characters".to_string());
        }
        if let Some(host) = non_blank(&self.remote_host)
            && !ssm_host_is_safe(host)
        {
            errors.push("AWS Session Manager remote host contains invalid characters".to_string());
        }
        errors
    }

    /// The host the session forwards to: the override when set, else `server`.
    pub fn remote_host_or<'a>(&'a self, server: &'a str) -> &'a str {
        non_blank(&self.remote_host).unwrap_or_else(|| server.trim())
    }

    /// `aws` argv (without the binary):
    /// `ssm start-session --target T --document-name AWS-StartPortForwardingSessionToRemoteHost
    /// --parameters {"host":[H],"portNumber":[P],"localPortNumber":[L]} [--region R] [--profile N]`.
    pub fn aws_args(&self, remote_host: &str, remote_port: u16, local_port: u16) -> Vec<String> {
        // JSON rather than the CLI's `host=…,portNumber=…` shorthand, so no
        // host spelling can be read as a second parameter. Built by hand so
        // the key order does not depend on serde_json's `preserve_order`.
        let host = serde_json::Value::from(remote_host.trim());
        let parameters = format!(
            r#"{{"host":[{host}],"portNumber":["{remote_port}"],"localPortNumber":["{local_port}"]}}"#
        );
        let mut args = vec![
            "ssm".to_string(),
            "start-session".to_string(),
            "--target".to_string(),
            self.target.trim().to_string(),
            "--document-name".to_string(),
            SSM_REMOTE_HOST_DOCUMENT.to_string(),
            "--parameters".to_string(),
            parameters.to_string(),
        ];
        if let Some(region) = non_blank(&self.region) {
            args.push("--region".to_string());
            args.push(region.to_string());
        }
        if let Some(profile) = non_blank(&self.profile) {
            args.push("--profile".to_string());
            args.push(profile.to_string());
        }
        args
    }

    /// English summary for logs and diagnostics.
    pub fn summary(&self) -> String {
        let mut out = format!("ssm:{}", self.target.trim());
        if let Some(region) = non_blank(&self.region) {
            out.push_str(&format!(" region {region}"));
        }
        if let Some(profile) = non_blank(&self.profile) {
            out.push_str(&format!(" profile {profile}"));
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ProxyKind {
    #[default]
    Socks5,
    HttpConnect,
}

/// A SOCKS5 or HTTP CONNECT proxy hop. Secret-free (password via keyring).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProxyHop {
    pub kind: ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretRef>,
}

/// Ordered hops between the client and the database. At most two:
/// `[proxy]`, `[ssh]`, or `[proxy, ssh]` (proxy first). `ssh` keeps its own
/// optional jump host, which does not count as a chain hop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TransportChain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ProxyHop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshTunnelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube: Option<KubePortForward>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssm: Option<SsmPortForward>,
}

impl TransportChain {
    pub fn is_empty(&self) -> bool {
        self.proxy.is_none() && self.ssh.is_none() && self.kube.is_none() && self.ssm.is_none()
    }

    pub fn hop_count(&self) -> usize {
        if self.kube.is_some() || self.ssm.is_some() {
            return 1;
        }
        usize::from(self.proxy.is_some()) + usize::from(self.ssh.is_some())
    }

    pub fn kube_only(k: KubePortForward) -> Self {
        Self {
            kube: Some(k),
            ..Self::default()
        }
    }

    pub fn ssm_only(ssm: SsmPortForward) -> Self {
        Self {
            ssm: Some(ssm),
            ..Self::default()
        }
    }

    /// Human-readable errors; empty = valid.
    pub fn validate(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.kube.is_some() && (self.proxy.is_some() || self.ssh.is_some()) {
            errors.push(
                "Kubernetes port-forward cannot be combined with SSH or proxy hops".to_string(),
            );
        }
        if self.ssm.is_some() && (self.proxy.is_some() || self.ssh.is_some() || self.kube.is_some())
        {
            errors.push(
                "AWS Session Manager cannot be combined with other tunnel or proxy hops"
                    .to_string(),
            );
        }
        if let Some(kube) = &self.kube {
            errors.extend(kube.validate());
        }
        if let Some(ssm) = &self.ssm {
            errors.extend(ssm.validate());
        }
        if let Some(proxy) = &self.proxy {
            if proxy.host.trim().is_empty() {
                errors.push("Proxy host is required".to_string());
            }
            if proxy.port == 0 {
                errors.push("Proxy port must be 1–65535".to_string());
            }
            let has_user = proxy
                .username
                .as_ref()
                .is_some_and(|u| !u.trim().is_empty());
            let has_password = proxy.password.is_some();
            if has_password && !has_user {
                errors.push("Proxy username is required when a password is set".to_string());
            }
        }
        if let Some(ssh) = &self.ssh {
            if ssh.target.host.trim().is_empty() {
                errors.push("SSH host is required".to_string());
            }
            if ssh.target.port == 0 {
                errors.push("SSH port must be 1–65535".to_string());
            }
            if ssh.target.username.trim().is_empty() {
                errors.push("SSH username is required".to_string());
            }
            if let Some(jump) = &ssh.jump {
                if jump.host.trim().is_empty() {
                    errors.push("SSH jump host is required".to_string());
                }
                if jump.port == 0 {
                    errors.push("SSH jump port must be 1–65535".to_string());
                }
                if jump.username.trim().is_empty() {
                    errors.push("SSH jump username is required".to_string());
                }
            }
        }
        if self.hop_count() > 2 {
            errors.push("Transport chain cannot exceed two hops".to_string());
        }
        errors
    }
}

pub type TunnelProfileId = uuid::Uuid;

/// A named, reusable transport chain. Referenced by connections by id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TunnelProfile {
    pub id: TunnelProfileId,
    pub name: String,
    pub chain: TransportChain,
}

/// What a masking pattern is compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum RuleScope {
    /// Matches the bare column name (`password`, `users.password` both match on `password`).
    #[default]
    ColumnName,
    /// Matches `table.column` (schema ignored); only applies when the query's source table is known.
    TableDotColumn,
}

/// One glob or regex pattern used to select sensitive columns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaskRule {
    /// Glob (`*`, `?`) or, when wrapped in slashes (`/…/`), a regex. Case-insensitive.
    pub pattern: String,
    #[serde(default)]
    pub scope: RuleScope,
}

/// Per-connection additions and exemptions (stored on the profile; secret-free).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct MaskingOverrides {
    pub include: Vec<MaskRule>,
    pub exclude: Vec<MaskRule>,
}

impl MaskingOverrides {
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }
}

/// Server-certificate verification stance for PostgreSQL/MySQL connections.
/// `Legacy` keeps the two-boolean (`encrypt` / `trust_server_certificate`)
/// behaviour for profiles saved before this field existed; every other value
/// is an explicit choice made in the editor or parsed from a URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum TlsMode {
    /// Derive the mode from the two legacy booleans (see
    /// [`ConnectionProfile::effective_tls_mode`]).
    #[default]
    Legacy,
    /// Explicit plaintext.
    Disable,
    /// Encrypted, but the server certificate is NOT verified.
    Require,
    /// Certificate chain verified against the trusted roots; hostname not
    /// checked.
    VerifyCa,
    /// Full verification: chain and hostname.
    VerifyFull,
}

impl TlsMode {
    /// The nearest legacy `(encrypt, trust_server_certificate)` pair, kept in
    /// sync on the profile so builds that predate `tls_mode` read a safe
    /// equivalent. `VerifyCa` degrades to full verification (never looser).
    pub fn legacy_flags(self) -> (bool, bool) {
        match self {
            TlsMode::Disable => (false, false),
            TlsMode::Require => (true, true),
            TlsMode::Legacy | TlsMode::VerifyCa | TlsMode::VerifyFull => (true, false),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionProfile {
    pub id: ConnectionId,
    pub name: String,
    pub server: String,
    pub port: u16,
    pub database: Option<String>,
    /// The concrete database engine this profile targets. Drives capability
    /// metadata (supported keywords/features/type storage) and pattern
    /// equivalency in generated SQL. Defaults to SQL Server for profiles
    /// saved before this field existed.
    #[serde(default)]
    pub database_type: DatabaseType,
    #[serde(default)]
    pub environment: ConnectionEnvironment,
    #[serde(default)]
    pub client: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
    pub authentication: AuthenticationType,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
    /// Server-certificate verification for PostgreSQL/MySQL. `Legacy` keeps
    /// the two-boolean behaviour for profiles saved before this field existed.
    #[serde(default)]
    pub tls_mode: TlsMode,
    /// Additional CA bundle (PEM) trusted alongside the built-in webpki roots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_file: Option<PathBuf>,
    /// Client certificate chain (PEM) presented for mutual TLS.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_cert_file: Option<PathBuf>,
    /// Unencrypted private key (PEM) for the client certificate. Paths are
    /// not secrets; an encrypted key's passphrase would be, so encrypted keys
    /// are refused at connect time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_key_file: Option<PathBuf>,
    /// Also trust the operating system's root certificates (the macOS
    /// keychain, the Windows store, the Linux CA bundle) when verifying the
    /// server, so a company CA installed on the machine verifies without a
    /// CA file. Off by default and for every profile saved before the field
    /// existed; honoured by the Redis driver.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_trust_os_roots: bool,
    /// PostgreSQL 17+ direct TLS (`sslnegotiation=direct`): open the TLS
    /// handshake straight away, with the `postgresql` ALPN, instead of
    /// sending `SSLRequest` first, so SNI-routing TLS proxies can pass the
    /// connection through. Ignored when TLS is disabled; off by default and
    /// for every profile saved before the field existed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_direct: bool,
    pub timeout_seconds: u64,
    /// Optional per-statement execution ceiling in seconds. When set, a query
    /// that runs past it is cancelled server-side and reported as a timeout.
    /// Absent on all pre-existing stored profiles, which decode as no limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_seconds: Option<u64>,
    pub application_name: String,
    #[serde(default)]
    pub auto_wrap_rollback: bool,
    #[serde(default)]
    pub block_all_but_select: bool,
    /// Show the pre-execution confirmation dialog ("WE WILL MAKE … CHANGES")
    /// for data-changing statements on this connection. Off by default.
    #[serde(default)]
    pub confirm_data_changes: bool,
    /// Skip the confirmation dialog for unbounded UPDATE/DELETE (no WHERE
    /// clause) on this connection. On by default — the unbounded-statement
    /// protections are opt-in per connection: untick this to make those
    /// statements confirm even when `confirm_data_changes` is off.
    #[serde(default = "default_allow_unbounded")]
    pub allow_unbounded_mutations: bool,
    /// Per-connection production override. `None` inherits the environment
    /// entity's `is_production`; `Some(true)` marks this one connection
    /// production regardless of environment; `Some(false)` opts this one
    /// connection out (it never loosens an entity's explicit SELECT-only or
    /// confirmation flags — see `mutation_review::SafetyFeatures`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub production_override: Option<bool>,
    /// True when `auto_wrap_rollback` was set by a safety policy default rather
    /// than an explicit user choice. Lets the UI distinguish "policy says so"
    /// from "user asked for it" and avoid clobbering user intent on re-eval.
    #[serde(default)]
    pub rollback_policy_defaulted: bool,
    /// Optional binding to a specific Entra account/tenant (secret-free).
    #[serde(default)]
    pub entra_account: Option<EntraAccountBinding>,
    /// Runtime-only Microsoft Entra access token supplied by the UI. This is
    /// intentionally skipped so saved profiles/exported stores remain secret-free.
    #[serde(skip)]
    pub runtime_entra_access_token: Option<String>,
    /// Runtime-only cloud IAM database token (AWS SigV4 RDS token or Google
    /// OAuth access token) minted at connect time. Never persisted.
    #[serde(skip)]
    pub runtime_cloud_iam_token: Option<String>,
    /// Runtime-only: run every batch under a read-only mode the database
    /// itself enforces (a read-only transaction that is always rolled back;
    /// `PRAGMA query_only` on SQLite; a rolled-back transaction plus a
    /// statement screen on SQL Server). Set by the AI/agent/MCP query paths
    /// for SQL the model composed itself, on top of — never instead of — the
    /// SELECT-only parser gate. Never persisted; user editor runs never set it.
    #[serde(skip)]
    pub enforce_read_only: bool,
    /// Where this profile came from.
    #[serde(default)]
    pub provenance: ConnectionProvenance,
    /// Observed, secret-free runtime status.
    #[serde(default)]
    pub status: ConnectionStatus,
    /// How this profile reaches its engine (direct socket vs. via a relay
    /// device). Defaults to [`ConnectionRoute::Direct`] so profiles saved
    /// before routing existed keep connecting straight to `server:port`.
    #[serde(default)]
    pub route: ConnectionRoute,
    /// Optional SSH tunnel the drivers open before connecting. `None` for every
    /// profile saved before this field existed and for file engines.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_tunnel: Option<SshTunnelConfig>,
    /// Named tunnel profile, resolved from the store at connect time. When set,
    /// takes precedence over `ssh_tunnel`/`transport`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_profile: Option<TunnelProfileId>,
    /// Inline chain for profiles that do not use a named profile. `ssh_tunnel`
    /// is migrated into `transport.ssh` on load (see `inline_transport`).
    #[serde(default, skip_serializing_if = "TransportChain::is_empty")]
    pub transport: TransportChain,
    /// User-starred: pickers list favorites first. Defaults off for profiles
    /// saved before the flag existed.
    #[serde(default)]
    pub favorite: bool,
    /// Optional SQL run once immediately after the driver opens a session.
    /// Split with the engine's dialect-aware batch rules (`GO` on SQL Server,
    /// whole script on Postgres/MySQL/SQLite). Failures fail the connection
    /// with `Startup SQL failed: …`. Absent on profiles saved before this
    /// field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_sql: Option<String>,
    /// Free-form notes the user keeps on the connection (who owns the
    /// server, what it is for, what to watch out for). Display only: never
    /// sent to the server. Absent on profiles saved before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Redis key browser: the delimiter key names are grouped by (`:` when
    /// unset). Meaningless on SQL engines; absent on profiles saved before
    /// the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_key_delimiter: Option<String>,
    /// Redis only: reach the master through Sentinel by master name instead
    /// of dialing `server:port` (see [`crate::redis_sentinel`]). Absent on
    /// profiles saved before the field existed and on direct connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_sentinel: Option<crate::redis_sentinel::RedisSentinelConfig>,
    /// Redis only: standalone/cluster mode and the cluster node map
    /// (advertised → dial addresses). `None` behaves as
    /// [`crate::redis_cluster::RedisClusterMode::Auto`] with no map; absent
    /// on profiles saved before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_cluster: Option<crate::redis_cluster::RedisClusterConfig>,
    /// Redis family only: the editor's "Auto-detect" entry. `database_type`
    /// then holds the flavor detected so far — from the host name while the
    /// profile is edited, then from `INFO server` on connect (see
    /// [`crate::redis_flavor`]). Absent on profiles saved before the field
    /// existed and on every explicitly chosen engine.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub engine_auto_detect: bool,
    /// Redis only: names for logical databases (`3 → "sessions"`), shown as
    /// `db3 — sessions` in the explorer, the key view and the key dialogs.
    /// Local to this profile; the server knows nothing of them.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub redis_db_aliases: std::collections::BTreeMap<i64, String>,
    /// Redis only: the logical databases the explorer lists, in this order
    /// (`None` lists every database the server has).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_visible_databases: Option<Vec<i64>>,
    /// Redis with Microsoft Entra sign-in only: the Redis user name to sign
    /// in as, when the cache's access policy names the identity by one.
    /// `None` signs in as the token's object id (`oid`), which is what Azure
    /// expects by default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_entra_user: Option<String>,
    /// ClickHouse only: the server's native-protocol TCP port, used by Open
    /// in Terminal (clickhouse-client speaks the native protocol, SQLLY talks
    /// HTTP on `port`). `None` falls back to the conventional mapping
    /// (8123 → 9000, 8443 → 9440). Absent on profiles saved before the field
    /// existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clickhouse_native_port: Option<u16>,
    /// Sensitive-column masking additions/exemptions for this connection. Secret-free.
    #[serde(default, skip_serializing_if = "MaskingOverrides::is_empty")]
    pub masking: MaskingOverrides,
    /// Schemas the user hid from the object explorer on this connection,
    /// keyed by database name (the same schema name can exist in two
    /// databases). Explorer-only: queries, IntelliSense and scripting still
    /// see them. Independent of the global system-schema placement. Absent
    /// on profiles saved before the field existed and whenever nothing is
    /// hidden, so such profiles serialize exactly as before.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub hidden_schemas: std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    /// The time zone result grids from this connection show dates and epoch
    /// timestamps in, replacing the app-wide Date setting. `None` uses the
    /// app setting. Per-column zones still win. Absent on profiles saved
    /// before the field existed and whenever unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_time_zone: Option<DisplayTimeZone>,
}

/// A connection's own display time zone for result grids (see
/// [`ConnectionProfile::display_time_zone`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DisplayTimeZone {
    Utc,
    Local,
    /// An IANA zone name such as `Europe/Berlin`.
    Named(String),
}

impl ConnectionProfile {
    /// The schemas hidden from the explorer in `database`, if any.
    pub fn hidden_schemas_in(&self, database: &str) -> Option<&std::collections::BTreeSet<String>> {
        self.hidden_schemas
            .get(database)
            .filter(|set| !set.is_empty())
    }

    /// Whether `schema` in `database` is hidden from the explorer.
    pub fn is_schema_hidden(&self, database: &str, schema: &str) -> bool {
        self.hidden_schemas_in(database)
            .is_some_and(|set| set.contains(schema))
    }

    /// How many schemas are hidden from the explorer in `database`.
    pub fn hidden_schema_count(&self, database: &str) -> usize {
        self.hidden_schemas_in(database).map_or(0, |set| set.len())
    }

    /// Hide `schema` in `database` from the explorer. Returns `false` when it
    /// was already hidden.
    pub fn hide_schema(&mut self, database: &str, schema: &str) -> bool {
        self.hidden_schemas
            .entry(database.to_string())
            .or_default()
            .insert(schema.to_string())
    }

    /// Bring back every schema hidden in `database`, returning how many were
    /// hidden. Leaves no empty entry behind, so a profile with nothing hidden
    /// serializes without the field.
    pub fn show_hidden_schemas(&mut self, database: &str) -> usize {
        self.hidden_schemas
            .remove(database)
            .map_or(0, |set| set.len())
    }

    /// The TLS mode drivers should act on. Profiles saved before `tls_mode`
    /// existed carry `Legacy`, which maps the two booleans exactly onto the
    /// pre-existing behaviour: not encrypting is plaintext, encrypting with
    /// `trust_server_certificate` is unverified TLS, and encrypting without
    /// it is full verification.
    pub fn effective_tls_mode(&self) -> TlsMode {
        match self.tls_mode {
            TlsMode::Legacy => {
                if !self.encrypt {
                    TlsMode::Disable
                } else if self.trust_server_certificate {
                    TlsMode::Require
                } else {
                    TlsMode::VerifyFull
                }
            }
            explicit => explicit,
        }
    }

    /// A libSQL / Turso profile signed in with a bearer token (or none: an
    /// empty in-memory reference is an anonymous local `sqld`).
    pub fn auth_token(
        name: impl Into<String>,
        server: impl Into<String>,
        port: u16,
        secret_ref: SecretRef,
    ) -> Self {
        let mut profile = Self::sql_login(
            name,
            server,
            "",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        profile.database_type = DatabaseType::LibSql;
        profile.port = port;
        profile.authentication = AuthenticationType::AuthToken { secret_ref };
        profile
    }

    pub fn sql_login(
        name: impl Into<String>,
        server: impl Into<String>,
        username: impl Into<String>,
        secret_ref: SecretRef,
    ) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            server: server.into(),
            port: 1433,
            database: None,
            database_type: DatabaseType::SqlServer,
            environment: ConnectionEnvironment::Local,
            client: None,
            project: None,
            authentication: AuthenticationType::SqlLogin {
                username: username.into(),
                secret_ref,
            },
            encrypt: true,
            trust_server_certificate: false,
            tls_mode: TlsMode::Legacy,
            tls_ca_file: None,
            tls_client_cert_file: None,
            tls_client_key_file: None,
            timeout_seconds: 30,
            statement_timeout_seconds: None,
            application_name: "SQLLY".to_string(),
            auto_wrap_rollback: false,
            block_all_but_select: false,
            confirm_data_changes: false,
            allow_unbounded_mutations: true,
            production_override: None,
            rollback_policy_defaulted: false,
            entra_account: None,
            runtime_entra_access_token: None,
            runtime_cloud_iam_token: None,
            enforce_read_only: false,
            provenance: ConnectionProvenance::Manual,
            status: ConnectionStatus::default(),
            route: ConnectionRoute::Direct,
            ssh_tunnel: None,
            tunnel_profile: None,
            transport: TransportChain::default(),
            favorite: false,
            startup_sql: None,
            notes: None,
            redis_key_delimiter: None,
            redis_sentinel: None,
            redis_cluster: None,
            engine_auto_detect: false,
            redis_db_aliases: Default::default(),
            redis_visible_databases: None,
            redis_entra_user: None,
            clickhouse_native_port: None,
            hidden_schemas: Default::default(),
            display_time_zone: None,
            masking: MaskingOverrides::default(),
            tls_trust_os_roots: false,
            tls_direct: false,
        }
    }

    /// Effective inline transport: prefers `transport`, else migrates legacy `ssh_tunnel`.
    pub fn inline_transport(&self) -> TransportChain {
        if !self.transport.is_empty() {
            return self.transport.clone();
        }
        TransportChain {
            proxy: None,
            ssh: self.ssh_tunnel.clone(),
            kube: None,
            ssm: None,
        }
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_seconds)
    }

    /// The per-statement execution ceiling, when one is configured. Zero is
    /// treated as unset so a cleared numeric field never means "instantly
    /// time out".
    pub fn statement_timeout(&self) -> Option<Duration> {
        self.statement_timeout_seconds
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
    }

    /// The full capability profile of the engine this connection targets —
    /// supported operations/keywords/features, identifier quoting, row-limit
    /// pattern, and the logical→native type map. Reported as part of the data
    /// model so downstream layers (and eventually the UI) can gate and adapt
    /// behavior per engine.
    pub fn db_metadata(&self) -> DbTypeMetadata {
        self.database_type.metadata()
    }

    pub fn azure_sql_entra_browser(
        name: impl Into<String>,
        server: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self::azure_sql(name, server, database, AuthenticationType::EntraBrowser)
    }

    pub fn azure_sql_entra_device_code(
        name: impl Into<String>,
        server: impl Into<String>,
        database: impl Into<String>,
    ) -> Self {
        Self::azure_sql(name, server, database, AuthenticationType::EntraDeviceCode)
    }

    fn azure_sql(
        name: impl Into<String>,
        server: impl Into<String>,
        database: impl Into<String>,
        authentication: AuthenticationType,
    ) -> Self {
        let defaults = AzureSqlConnectionDefaults::default();
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            server: server.into(),
            port: defaults.port,
            database: Some(database.into()),
            database_type: DatabaseType::AzureSqlDatabase,
            environment: ConnectionEnvironment::Production,
            client: None,
            project: None,
            authentication,
            encrypt: defaults.encrypt,
            trust_server_certificate: defaults.trust_server_certificate,
            tls_mode: TlsMode::Legacy,
            tls_ca_file: None,
            tls_client_cert_file: None,
            tls_client_key_file: None,
            timeout_seconds: defaults.timeout_seconds,
            statement_timeout_seconds: None,
            application_name: defaults.application_name,
            auto_wrap_rollback: false,
            block_all_but_select: true,
            confirm_data_changes: false,
            allow_unbounded_mutations: true,
            production_override: None,
            rollback_policy_defaulted: false,
            entra_account: None,
            runtime_entra_access_token: None,
            runtime_cloud_iam_token: None,
            enforce_read_only: false,
            provenance: ConnectionProvenance::Manual,
            status: ConnectionStatus::default(),
            route: ConnectionRoute::Direct,
            ssh_tunnel: None,
            tunnel_profile: None,
            transport: TransportChain::default(),
            favorite: false,
            startup_sql: None,
            notes: None,
            redis_key_delimiter: None,
            redis_sentinel: None,
            redis_cluster: None,
            engine_auto_detect: false,
            redis_db_aliases: Default::default(),
            redis_visible_databases: None,
            redis_entra_user: None,
            clickhouse_native_port: None,
            hidden_schemas: Default::default(),
            display_time_zone: None,
            masking: MaskingOverrides::default(),
            tls_trust_os_roots: false,
            tls_direct: false,
        }
    }

    pub fn aws_rds_iam(
        name: impl Into<String>,
        server: impl Into<String>,
        port: u16,
        database_type: DatabaseType,
        username: impl Into<String>,
        region: impl Into<String>,
    ) -> Self {
        Self::cloud_iam_profile(
            name,
            server,
            port,
            database_type,
            AuthenticationType::AwsRdsIam {
                username: username.into(),
                region: region.into(),
                profile: None,
            },
        )
    }

    pub fn gcp_cloud_sql_iam(
        name: impl Into<String>,
        server: impl Into<String>,
        port: u16,
        database_type: DatabaseType,
        username: impl Into<String>,
    ) -> Self {
        Self::cloud_iam_profile(
            name,
            server,
            port,
            database_type,
            AuthenticationType::GcpCloudSqlIam {
                username: username.into(),
            },
        )
    }

    fn cloud_iam_profile(
        name: impl Into<String>,
        server: impl Into<String>,
        port: u16,
        database_type: DatabaseType,
        authentication: AuthenticationType,
    ) -> Self {
        Self {
            id: ConnectionId::new(),
            name: name.into(),
            server: server.into(),
            port,
            database: None,
            database_type,
            environment: ConnectionEnvironment::Local,
            client: None,
            project: None,
            authentication,
            encrypt: true,
            trust_server_certificate: false,
            tls_mode: TlsMode::Legacy,
            tls_ca_file: None,
            tls_client_cert_file: None,
            tls_client_key_file: None,
            timeout_seconds: 30,
            statement_timeout_seconds: None,
            application_name: "SQLLY".to_string(),
            auto_wrap_rollback: false,
            block_all_but_select: false,
            confirm_data_changes: false,
            allow_unbounded_mutations: true,
            production_override: None,
            rollback_policy_defaulted: false,
            entra_account: None,
            runtime_entra_access_token: None,
            runtime_cloud_iam_token: None,
            enforce_read_only: false,
            provenance: ConnectionProvenance::Manual,
            status: ConnectionStatus::default(),
            route: ConnectionRoute::Direct,
            ssh_tunnel: None,
            tunnel_profile: None,
            transport: TransportChain::default(),
            favorite: false,
            startup_sql: None,
            notes: None,
            redis_key_delimiter: None,
            redis_sentinel: None,
            redis_cluster: None,
            engine_auto_detect: false,
            redis_db_aliases: Default::default(),
            redis_visible_databases: None,
            redis_entra_user: None,
            clickhouse_native_port: None,
            hidden_schemas: Default::default(),
            display_time_zone: None,
            masking: MaskingOverrides::default(),
            tls_trust_os_roots: false,
            tls_direct: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DatabaseSummary {
    pub name: String,
    pub state: String,
    pub compatibility_level: Option<u16>,
}

/// A secret-free, validated summary of how a [`ConnectionProfile`] will be
/// used. The connection editor (and the CLI `connection preview` command)
/// renders this so users can confirm endpoint, auth mode, and TLS posture and
/// see actionable warnings before any secret is read or a socket is opened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionPreview {
    pub endpoint: String,
    pub database: Option<String>,
    pub authentication: String,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
    pub environment: ConnectionEnvironment,
    pub client: Option<String>,
    pub project: Option<String>,
    pub auto_wrap_rollback: bool,
    pub block_all_but_select: bool,
    pub confirm_data_changes: bool,
    pub allow_unbounded_mutations: bool,
    pub rollback_policy_defaulted: bool,
    pub provenance: ConnectionProvenance,
    pub entra_account: Option<EntraAccountBinding>,
    pub status: ConnectionStatus,
    pub is_azure: bool,
    /// Non-fatal advisories (e.g. insecure TLS posture for Azure).
    pub warnings: Vec<String>,
}

impl ConnectionProfile {
    /// Stable fingerprint of the connection's catalog scope (where + as-whom).
    /// This is the canonical implementation of the format the schema catalog
    /// has always used to name its per-connection SQLite files and fingerprint
    /// its records — the engine and the services layer both delegate here.
    ///
    /// The format is FROZEN: catalog files and persisted AI data on disk are
    /// keyed by it, so any change silently orphans existing catalogs. It
    /// deliberately excludes WHICH Entra account is bound (two accounts on the
    /// same server/database share a catalog) — see [`pool_identity`] for the
    /// account-aware variant. Also deliberately excludes the password (a key
    /// collision on everything else but the password is not a realistic
    /// concern and the password is sensitive).
    pub fn catalog_identity(&self) -> String {
        let auth = match &self.authentication {
            AuthenticationType::SqlLogin { username, .. } => format!("sql:{username}"),
            AuthenticationType::EntraDeviceCode => "entra-device".to_string(),
            AuthenticationType::EntraBrowser => "entra-browser".to_string(),
            AuthenticationType::Integrated => "integrated".to_string(),
            AuthenticationType::AwsRdsIam { username, .. } => format!("aws-rds-iam:{username}"),
            AuthenticationType::GcpCloudSqlIam { username } => {
                format!("gcp-cloud-sql-iam:{username}")
            }
            AuthenticationType::AuthToken { .. } => "auth-token".to_string(),
        };
        format!(
            "{}|{}|{}|{}|{}|{}",
            self.server.trim(),
            self.port,
            self.database.as_deref().unwrap_or(""),
            auth,
            self.encrypt,
            self.trust_server_certificate,
        )
    }

    /// Identity used to key warm-connection pools. Same as
    /// [`catalog_identity`] for non-Entra auth; for Entra auth it additionally
    /// binds the specific account (`account_id|tenant_id`) so two Entra users
    /// on the same server/database never share a warm client (session state —
    /// temp tables, `SESSION_CONTEXT`, impersonation — must not leak across
    /// accounts). The access token itself is never part of the key: tokens
    /// rotate hourly while a warm connection stays valid, so keying on the
    /// token would defeat pooling.
    ///
    /// An Entra profile with no account binding falls back to the profile's
    /// unique id, so two unbound profiles can never collide either.
    pub fn pool_identity(&self) -> String {
        let base = self.catalog_identity();
        match &self.authentication {
            AuthenticationType::EntraBrowser | AuthenticationType::EntraDeviceCode => {
                let account = match &self.entra_account {
                    Some(binding) => format!("{}|{}", binding.account_id, binding.tenant_id),
                    None => format!("unbound-profile:{}", self.id.0),
                };
                format!("{base}|{account}")
            }
            _ => base,
        }
    }

    /// Whether the target host looks like an Azure SQL endpoint.
    pub fn is_azure_endpoint(&self) -> bool {
        let host = self.server.to_ascii_lowercase();
        host.ends_with(".database.windows.net")
            || host.ends_with(".database.azure.com")
            || host.contains(".database.")
    }

    /// Produces a secret-free [`ConnectionPreview`], collecting validation
    /// warnings. Never reads or echoes any secret material.
    pub fn preview(&self) -> ConnectionPreview {
        let is_azure = self.is_azure_endpoint();
        let authentication = match &self.authentication {
            AuthenticationType::SqlLogin { username, .. } => {
                format!("SQL login ({username})")
            }
            AuthenticationType::Integrated => "Integrated".to_string(),
            AuthenticationType::EntraDeviceCode => "Microsoft Entra (device code)".to_string(),
            AuthenticationType::EntraBrowser => "Microsoft Entra (browser)".to_string(),
            AuthenticationType::AwsRdsIam {
                username, region, ..
            } => {
                format!("AWS IAM ({username}, {region})")
            }
            AuthenticationType::GcpCloudSqlIam { username } => {
                format!("Google IAM ({username})")
            }
            AuthenticationType::AuthToken { secret_ref } => {
                if secret_ref.provider == SecretProvider::InMemory && secret_ref.key.is_empty() {
                    "No auth token (anonymous)".to_string()
                } else {
                    "Auth token".to_string()
                }
            }
        };

        let mut warnings = Vec::new();
        if is_azure && !self.encrypt {
            warnings.push("Azure SQL requires encryption; enable Encrypt.".to_string());
        }
        if self.encrypt && self.trust_server_certificate {
            warnings.push(
                "Trusting the server certificate disables TLS validation; avoid in production."
                    .to_string(),
            );
        }
        if is_azure && self.database.is_none() {
            warnings.push(
                "Azure SQL Database connections should specify a database (no USE support)."
                    .to_string(),
            );
        }
        if matches!(self.authentication, AuthenticationType::Integrated) && is_azure {
            warnings.push(
                "Integrated auth is not available for Azure SQL; use Microsoft Entra.".to_string(),
            );
        }
        if self.authentication.requires_tls() && !self.encrypt {
            warnings.push(
                "Cloud IAM authentication requires encryption; TLS is forced on connect."
                    .to_string(),
            );
        }
        // TLS certificate validation messaging surfaced to the UI.
        if self.encrypt && !self.trust_server_certificate {
            // Reassure: validation is on. (Informational, not a warning gate.)
        } else if !self.encrypt && !is_azure {
            warnings.push(
                "Encryption is disabled; traffic and credentials are sent in clear text."
                    .to_string(),
            );
        }
        if self.environment == ConnectionEnvironment::Production && self.trust_server_certificate {
            warnings.push(
                "Production profile trusts the server certificate; TLS identity is not verified."
                    .to_string(),
            );
        }

        ConnectionPreview {
            endpoint: format!("tcp:{},{}", self.server, self.port),
            database: self.database.clone(),
            authentication,
            encrypt: self.encrypt,
            trust_server_certificate: self.trust_server_certificate,
            environment: self.environment,
            client: self.client.clone(),
            project: self.project.clone(),
            auto_wrap_rollback: self.auto_wrap_rollback,
            block_all_but_select: self.block_all_but_select,
            confirm_data_changes: self.confirm_data_changes,
            allow_unbounded_mutations: self.allow_unbounded_mutations,
            rollback_policy_defaulted: self.rollback_policy_defaulted,
            provenance: self.provenance,
            entra_account: self.entra_account.clone(),
            status: self.status.clone(),
            is_azure,
            warnings,
        }
    }
}

/// One keyring entry the duplicate needs copied: the original profile's
/// reference and the fresh reference the copy was minted with. Only
/// [`SecretProvider::MacOsKeychain`] references are listed — `InMemory`
/// carries its value inline and `Environment` names a variable that is not
/// per-profile, so neither needs (or can have) a per-profile copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretCopy {
    pub from: SecretRef,
    pub to: SecretRef,
}

/// Result of [`ConnectionProfile::duplicate`]: the independent copy plus the
/// secret entries the caller must copy in the secret store before the copy
/// can authenticate on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicatedProfile {
    pub profile: ConnectionProfile,
    pub secrets: Vec<SecretCopy>,
}

const COPY_SUFFIX: &str = " (copy";

/// The name a duplicate of `original` gets: `"<name> (copy)"`, or — when the
/// name already carries that suffix — the next number: `" (copy)"` becomes
/// `" (copy 2)"`, `" (copy 2)"` becomes `" (copy 3)"`.
pub fn copy_name(original: &str) -> String {
    let original = original.trim_end();
    match split_copy_suffix(original) {
        Some((base, n)) => format!("{base}{COPY_SUFFIX} {})", n + 1),
        None => format!("{original}{COPY_SUFFIX})"),
    }
}

/// [`copy_name`], bumped until it collides with none of `existing` (compared
/// case-insensitively after trimming), so two duplicates of the same profile
/// never share a name.
pub fn unique_copy_name<I, S>(original: &str, existing: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let taken: Vec<String> = existing
        .into_iter()
        .map(|s| s.as_ref().trim().to_lowercase())
        .collect();
    let mut candidate = copy_name(original);
    while taken.contains(&candidate.trim().to_lowercase()) {
        candidate = copy_name(&candidate);
    }
    candidate
}

/// Splits `"<base> (copy)"` into `(base, 1)` and `"<base> (copy N)"` into
/// `(base, N)`; `None` when the name does not end in a copy suffix.
fn split_copy_suffix(name: &str) -> Option<(&str, u32)> {
    let inner = name.strip_suffix(')')?;
    let start = inner.rfind(COPY_SUFFIX)?;
    let (base, tail) = inner.split_at(start);
    let tail = &tail[COPY_SUFFIX.len()..];
    if tail.is_empty() {
        return Some((base, 1));
    }
    let number = tail.strip_prefix(' ')?;
    if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    number.parse::<u32>().ok().map(|n| (base, n))
}

/// Re-mint a keyring reference for a copy, recording the pair; every other
/// provider is carried over unchanged.
fn duplicate_secret_ref(secret_ref: &SecretRef, secrets: &mut Vec<SecretCopy>) -> SecretRef {
    match secret_ref.provider {
        SecretProvider::MacOsKeychain => {
            let fresh = SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: Uuid::new_v4().to_string(),
            };
            secrets.push(SecretCopy {
                from: secret_ref.clone(),
                to: fresh.clone(),
            });
            fresh
        }
        SecretProvider::Environment | SecretProvider::InMemory => secret_ref.clone(),
    }
}

fn duplicate_ssh_auth(auth: &SshAuth, secrets: &mut Vec<SecretCopy>) -> SshAuth {
    match auth {
        SshAuth::Agent => SshAuth::Agent,
        SshAuth::KeyFile { path, passphrase } => SshAuth::KeyFile {
            path: path.clone(),
            passphrase: passphrase
                .as_ref()
                .map(|p| duplicate_secret_ref(p, secrets)),
        },
        SshAuth::Password { secret_ref } => SshAuth::Password {
            secret_ref: duplicate_secret_ref(secret_ref, secrets),
        },
    }
}

fn duplicate_ssh_tunnel(
    tunnel: &SshTunnelConfig,
    secrets: &mut Vec<SecretCopy>,
) -> SshTunnelConfig {
    let hop = |hop: &SshHop, secrets: &mut Vec<SecretCopy>| SshHop {
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
        auth: duplicate_ssh_auth(&hop.auth, secrets),
    };
    SshTunnelConfig {
        target: hop(&tunnel.target, secrets),
        jump: tunnel.jump.as_ref().map(|j| hop(j, secrets)),
        keepalive: tunnel.keepalive,
    }
}

fn duplicate_transport(chain: &TransportChain, secrets: &mut Vec<SecretCopy>) -> TransportChain {
    TransportChain {
        proxy: chain.proxy.as_ref().map(|proxy| ProxyHop {
            kind: proxy.kind,
            host: proxy.host.clone(),
            port: proxy.port,
            username: proxy.username.clone(),
            password: proxy
                .password
                .as_ref()
                .map(|p| duplicate_secret_ref(p, secrets)),
        }),
        ssh: chain
            .ssh
            .as_ref()
            .map(|tunnel| duplicate_ssh_tunnel(tunnel, secrets)),
        kube: chain.kube.clone(),
        ssm: chain.ssm.clone(),
    }
}

impl ConnectionProfile {
    /// An independent copy of this profile named [`copy_name`] of the
    /// original. See [`Self::duplicate_named`] for what is and is not copied.
    pub fn duplicate(&self) -> DuplicatedProfile {
        self.duplicate_named(copy_name(&self.name))
    }

    /// An independent copy of this profile under `name`.
    ///
    /// Every setting is carried over — server, port, database, engine,
    /// authentication mode and username, TLS, timeouts, safety flags,
    /// environment/client/project, routing, SSH/proxy/Kubernetes hops, named
    /// tunnel, favorite, startup SQL and masking — with these exceptions:
    ///
    /// - `id` is freshly minted, so the copy is a separate saved connection.
    /// - Every keyring-backed secret reference (SQL password, SSH password or
    ///   key passphrase, proxy password, on the inline chain and the legacy
    ///   `ssh_tunnel` alike) is re-minted, and each original→copy pair is
    ///   returned in [`DuplicatedProfile::secrets`] so the caller can copy
    ///   the secret value itself; the two profiles never share an entry, so
    ///   deleting or re-keying one cannot break the other.
    /// - `status` starts untested: observed health belongs to the original.
    /// - `provenance` is `Manual`: the copy was made by hand, whatever the
    ///   original was discovered or imported from.
    /// - Runtime-only tokens (`runtime_entra_access_token`,
    ///   `runtime_cloud_iam_token`) are dropped; they are session state.
    pub fn duplicate_named(&self, name: impl Into<String>) -> DuplicatedProfile {
        let mut secrets = Vec::new();
        let authentication = match &self.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => AuthenticationType::SqlLogin {
                username: username.clone(),
                secret_ref: duplicate_secret_ref(secret_ref, &mut secrets),
            },
            AuthenticationType::AuthToken { secret_ref } => AuthenticationType::AuthToken {
                secret_ref: duplicate_secret_ref(secret_ref, &mut secrets),
            },
            other => other.clone(),
        };
        // Legacy `ssh_tunnel` mirrors `transport.ssh` on every profile the
        // editor has saved; when both are present and equal they are one
        // tunnel and must resolve to the *same* fresh references, or the
        // copy would carry two keys for one secret.
        let transport = duplicate_transport(&self.transport, &mut secrets);
        let ssh_tunnel = match &self.ssh_tunnel {
            None => None,
            Some(tunnel) if self.transport.ssh.as_ref() == Some(tunnel) => transport.ssh.clone(),
            Some(tunnel) => Some(duplicate_ssh_tunnel(tunnel, &mut secrets)),
        };
        let redis_sentinel = self.redis_sentinel.as_ref().map(|sentinel| {
            let mut copy = sentinel.clone();
            copy.sentinel_password_ref = sentinel
                .sentinel_password_ref
                .as_ref()
                .map(|r| duplicate_secret_ref(r, &mut secrets));
            copy
        });
        let profile = ConnectionProfile {
            redis_sentinel,
            id: ConnectionId::new(),
            name: name.into(),
            authentication,
            transport,
            ssh_tunnel,
            status: ConnectionStatus::default(),
            provenance: ConnectionProvenance::Manual,
            runtime_entra_access_token: None,
            runtime_cloud_iam_token: None,
            ..self.clone()
        };
        DuplicatedProfile { profile, secrets }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_matches_its_built_in_entity_and_the_legacy_spelling() {
        let dev = ConnectionEnvironment::Dev;
        assert_eq!(dev.entity_name(), Some("Dev"));
        for name in ["Dev", "dev", " DEV ", "Development", "development"] {
            assert!(dev.matches_entity_name(name), "{name}");
        }
        assert!(!dev.matches_entity_name("Devops"));
        assert!(!ConnectionEnvironment::Production.matches_entity_name("Development"));
        assert!(ConnectionEnvironment::Production.matches_entity_name("production"));
        assert!(!ConnectionEnvironment::Local.matches_entity_name("Local"));
    }

    #[test]
    fn find_entity_prefers_the_built_in_name_over_the_alias() {
        let entities = ["Development", "Test", "Dev"];
        let found = ConnectionEnvironment::Dev.find_entity(&entities, |n| n);
        assert_eq!(found, Some(&"Dev"));
        let legacy_only = ["Development", "Production"];
        assert_eq!(
            ConnectionEnvironment::Dev.find_entity(&legacy_only, |n| n),
            Some(&"Development")
        );
        assert_eq!(
            ConnectionEnvironment::Unset.find_entity(&legacy_only, |n| n),
            None
        );
    }

    /// A profile with every optional field populated and every keyring slot
    /// filled, so the duplicate test notices a field the copy forgot. When a
    /// field is added to `ConnectionProfile`, set it here (struct literal, no
    /// `..`) and the equality assertion below covers it automatically.
    fn fully_populated_profile() -> ConnectionProfile {
        let keychain = |key: &str| SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: key.to_string(),
        };
        let tunnel = SshTunnelConfig {
            target: SshHop {
                host: "bastion.example.com".into(),
                port: 2222,
                username: "ops".into(),
                auth: SshAuth::KeyFile {
                    path: "/home/ops/.ssh/id_ed25519".into(),
                    passphrase: Some(keychain("11111111-1111-1111-1111-111111111111")),
                },
            },
            jump: Some(SshHop {
                host: "jump.example.com".into(),
                port: 22,
                username: "jumper".into(),
                auth: SshAuth::Password {
                    secret_ref: keychain("22222222-2222-2222-2222-222222222222"),
                },
            }),
            keepalive: Default::default(),
        };
        ConnectionProfile {
            id: ConnectionId::new(),
            name: "Warehouse".into(),
            server: "db.example.com".into(),
            port: 5432,
            database: Some("analytics".into()),
            database_type: DatabaseType::Postgres,
            environment: ConnectionEnvironment::Staging,
            client: Some("Acme".into()),
            project: Some("Reporting".into()),
            authentication: AuthenticationType::SqlLogin {
                username: "reporter".into(),
                secret_ref: keychain("00000000-0000-0000-0000-000000000000"),
            },
            encrypt: true,
            trust_server_certificate: false,
            tls_mode: TlsMode::VerifyCa,
            tls_ca_file: Some(PathBuf::from("/etc/ssl/ca.pem")),
            tls_client_cert_file: Some(PathBuf::from("/etc/ssl/client.pem")),
            tls_client_key_file: Some(PathBuf::from("/etc/ssl/client.key")),
            timeout_seconds: 45,
            statement_timeout_seconds: Some(120),
            application_name: "SQLLY-reports".into(),
            auto_wrap_rollback: true,
            block_all_but_select: true,
            confirm_data_changes: true,
            allow_unbounded_mutations: false,
            enforce_read_only: true,
            production_override: Some(true),
            rollback_policy_defaulted: true,
            entra_account: Some(EntraAccountBinding {
                account_id: "home:tenant".into(),
                tenant_id: "tenant".into(),
                username: Some("who@example.com".into()),
            }),
            runtime_entra_access_token: Some("runtime-token".into()),
            runtime_cloud_iam_token: Some("iam-token".into()),
            provenance: ConnectionProvenance::Imported,
            status: ConnectionStatus {
                last_connected_at: Some(Utc::now()),
                last_tested_at: Some(Utc::now()),
                last_test_status: Some(ConnectionTestStatus::Failed),
                last_error_summary: Some("login failed".into()),
                last_error_message: None,
            },
            route: ConnectionRoute::ViaRelay {
                relay_device_id: "device-1".into(),
                pair_id: "pair-1".into(),
                relay_hint: Some("relay.example.com:443".into()),
            },
            ssh_tunnel: Some(tunnel.clone()),
            tunnel_profile: Some(Uuid::new_v4()),
            transport: TransportChain {
                proxy: Some(ProxyHop {
                    kind: ProxyKind::HttpConnect,
                    host: "proxy.example.com".into(),
                    port: 3128,
                    username: Some("proxyuser".into()),
                    password: Some(keychain("33333333-3333-3333-3333-333333333333")),
                }),
                ssh: Some(tunnel),
                kube: Some(KubePortForward {
                    kubeconfig: Some("/home/ops/.kube/config".into()),
                    context: Some("prod".into()),
                    namespace: "data".into(),
                    target_kind: KubeTargetKind::Service,
                    target_name: "postgres".into(),
                    remote_port: 5432,
                }),
                ssm: Some(SsmPortForward {
                    target: "i-0123456789abcdef0".into(),
                    region: Some("us-east-1".into()),
                    profile: Some("ops".into()),
                    remote_host: None,
                }),
            },
            favorite: true,
            startup_sql: Some("SET search_path TO reporting".into()),
            notes: Some("Reporting replica; ask #data before heavy queries.".into()),
            redis_key_delimiter: Some(":".into()),
            redis_sentinel: None,
            redis_cluster: None,
            engine_auto_detect: false,
            redis_db_aliases: Default::default(),
            redis_visible_databases: None,
            redis_entra_user: None,
            clickhouse_native_port: None,
            hidden_schemas: Default::default(),
            display_time_zone: None,
            masking: MaskingOverrides {
                include: vec![MaskRule {
                    pattern: "ssn".into(),
                    scope: RuleScope::ColumnName,
                }],
                exclude: vec![MaskRule {
                    pattern: "users.email".into(),
                    scope: RuleScope::TableDotColumn,
                }],
            },
            tls_trust_os_roots: false,
            tls_direct: false,
        }
    }

    /// Every keyring reference reachable from a profile, in a stable order.
    fn keychain_keys(profile: &ConnectionProfile) -> Vec<String> {
        let mut keys = Vec::new();
        let mut push = |r: &SecretRef| {
            if r.provider == SecretProvider::MacOsKeychain {
                keys.push(r.key.clone());
            }
        };
        if let Some(secret_ref) = profile.authentication.secret_ref() {
            push(secret_ref);
        }
        let mut push_auth = |auth: &SshAuth| match auth {
            SshAuth::Agent => {}
            SshAuth::KeyFile { passphrase, .. } => {
                if let Some(p) = passphrase {
                    push(p);
                }
            }
            SshAuth::Password { secret_ref } => push(secret_ref),
        };
        for tunnel in [profile.ssh_tunnel.as_ref(), profile.transport.ssh.as_ref()]
            .into_iter()
            .flatten()
        {
            push_auth(&tunnel.target.auth);
            if let Some(jump) = &tunnel.jump {
                push_auth(&jump.auth);
            }
        }
        if let Some(p) = profile
            .transport
            .proxy
            .as_ref()
            .and_then(|p| p.password.as_ref())
        {
            push(p);
        }
        keys
    }

    #[test]
    fn copy_name_appends_and_increments_the_copy_suffix() {
        assert_eq!(copy_name("Prod"), "Prod (copy)");
        assert_eq!(copy_name("Prod (copy)"), "Prod (copy 2)");
        assert_eq!(copy_name("Prod (copy 2)"), "Prod (copy 3)");
        assert_eq!(copy_name("Prod (copy 9)"), "Prod (copy 10)");
        assert_eq!(copy_name("Prod (copy) "), "Prod (copy 2)");
        // Not a copy suffix: a parenthetical that merely contains the word.
        assert_eq!(copy_name("Prod (copy of x)"), "Prod (copy of x) (copy)");
        assert_eq!(copy_name("Prod (copy2)"), "Prod (copy2) (copy)");
        assert_eq!(copy_name(""), " (copy)");
    }

    #[test]
    fn unique_copy_name_skips_names_already_taken() {
        let taken = ["Prod", "Prod (copy)", "prod (COPY 2)"];
        assert_eq!(unique_copy_name("Prod", taken), "Prod (copy 3)");
        assert_eq!(unique_copy_name("Prod", ["Other"]), "Prod (copy)");
        assert_eq!(
            unique_copy_name("Prod (copy)", ["Prod (copy)"]),
            "Prod (copy 2)"
        );
    }

    #[test]
    fn duplicate_copies_every_setting_except_identity_secrets_and_runtime_state() {
        let original = fully_populated_profile();
        let DuplicatedProfile { profile: copy, .. } = original.duplicate();

        assert_ne!(
            copy.id, original.id,
            "the copy must be its own saved connection"
        );
        assert_eq!(copy.name, "Warehouse (copy)");
        assert_eq!(copy.status, ConnectionStatus::default());
        assert_eq!(copy.provenance, ConnectionProvenance::Manual);
        assert_eq!(copy.runtime_entra_access_token, None);
        assert_eq!(copy.runtime_cloud_iam_token, None);

        // Normalise the deliberate differences away; everything else must be
        // identical, which is what catches a future field the copy forgot.
        let mut expected = original.clone();
        expected.id = copy.id;
        expected.name = copy.name.clone();
        expected.status = ConnectionStatus::default();
        expected.provenance = ConnectionProvenance::Manual;
        expected.runtime_entra_access_token = None;
        expected.runtime_cloud_iam_token = None;
        expected.authentication = copy.authentication.clone();
        expected.ssh_tunnel = copy.ssh_tunnel.clone();
        expected.transport = copy.transport.clone();
        assert_eq!(copy, expected);

        // The secret-bearing structures differ only in their keyring keys.
        let AuthenticationType::SqlLogin { username, .. } = &copy.authentication else {
            panic!("auth mode must be preserved");
        };
        assert_eq!(username, "reporter");
        let (Some(orig_tunnel), Some(copy_tunnel)) = (&original.ssh_tunnel, &copy.ssh_tunnel)
        else {
            panic!("tunnel must be copied");
        };
        assert_eq!(copy_tunnel.target.host, orig_tunnel.target.host);
        assert_eq!(copy_tunnel.target.port, orig_tunnel.target.port);
        assert_eq!(copy_tunnel.target.username, orig_tunnel.target.username);
        let SshAuth::KeyFile { path, passphrase } = &copy_tunnel.target.auth else {
            panic!("ssh auth mode must be preserved");
        };
        assert_eq!(path, "/home/ops/.ssh/id_ed25519");
        assert!(passphrase.is_some());
        assert_eq!(copy.transport.kube, original.transport.kube);
        assert_eq!(copy.transport.ssm, original.transport.ssm);
        let (Some(orig_proxy), Some(copy_proxy)) =
            (&original.transport.proxy, &copy.transport.proxy)
        else {
            panic!("proxy must be copied");
        };
        assert_eq!(copy_proxy.host, orig_proxy.host);
        assert_eq!(copy_proxy.username, orig_proxy.username);
        assert!(copy_proxy.password.is_some());
    }

    #[test]
    fn duplicate_never_shares_a_keyring_entry_with_the_original() {
        let original = fully_populated_profile();
        let DuplicatedProfile {
            profile: copy,
            secrets,
        } = original.duplicate();

        let original_keys = keychain_keys(&original);
        let copy_keys = keychain_keys(&copy);
        assert_eq!(copy_keys.len(), original_keys.len(), "every slot re-minted");
        for key in &copy_keys {
            assert!(
                !original_keys.contains(key),
                "copy still references the original's keyring entry {key}"
            );
            assert!(Uuid::parse_str(key).is_ok(), "fresh keys are uuids: {key}");
        }
        for (a, b) in original_keys.iter().zip(&copy_keys) {
            let pair = secrets
                .iter()
                .find(|s| s.from.key == *a)
                .unwrap_or_else(|| panic!("no copy recorded for {a}"));
            assert_eq!(pair.to.key, *b);
            assert_eq!(pair.to.provider, SecretProvider::MacOsKeychain);
        }
        // The legacy mirror and the inline chain describe one tunnel, so they
        // resolve to the same fresh references: 4 distinct secrets, not 6.
        assert_eq!(copy.ssh_tunnel, copy.transport.ssh);
        assert_eq!(secrets.len(), 4, "sql + passphrase + jump password + proxy");
    }

    #[test]
    fn duplicate_re_mints_the_redis_sentinel_password() {
        let mut original = fully_populated_profile();
        let sentinel_key = Uuid::new_v4().to_string();
        original.redis_sentinel = Some(crate::redis_sentinel::RedisSentinelConfig {
            master_name: "mymaster".into(),
            sentinels: vec!["10.0.0.1:26379".into()],
            sentinel_password_ref: Some(SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: sentinel_key.clone(),
            }),
            prefer_replica_reads: true,
        });
        let DuplicatedProfile {
            profile: copy,
            secrets,
        } = original.duplicate();
        let copied = copy.redis_sentinel.expect("sentinel settings copied");
        assert_eq!(copied.master_name, "mymaster");
        assert!(copied.prefer_replica_reads);
        let fresh = copied.sentinel_password_ref.expect("password ref kept");
        assert_ne!(fresh.key, sentinel_key);
        assert!(
            secrets
                .iter()
                .any(|s| s.from.key == sentinel_key && s.to.key == fresh.key)
        );
        assert_eq!(secrets.len(), 5);
    }

    #[test]
    fn duplicate_carries_in_memory_and_environment_secrets_verbatim() {
        let mut original = fully_populated_profile();
        original.authentication = AuthenticationType::SqlLogin {
            username: "sa".into(),
            secret_ref: SecretRef {
                provider: SecretProvider::Environment,
                key: "PGPASSWORD".into(),
            },
        };
        original.ssh_tunnel = None;
        original.transport = TransportChain::default();
        let DuplicatedProfile {
            profile: copy,
            secrets,
        } = original.duplicate();
        assert_eq!(copy.authentication, original.authentication);
        assert!(secrets.is_empty());

        original.authentication = AuthenticationType::SqlLogin {
            username: "sa".into(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "hunter2".into(),
            },
        };
        let DuplicatedProfile {
            profile: copy,
            secrets,
        } = original.duplicate();
        assert_eq!(copy.authentication, original.authentication);
        assert!(secrets.is_empty());
    }

    #[test]
    fn duplicate_of_non_password_auth_keeps_the_auth_mode_and_binding() {
        let mut original = fully_populated_profile();
        original.authentication = AuthenticationType::EntraBrowser;
        original.ssh_tunnel = None;
        original.transport = TransportChain::default();
        let copy = original.duplicate().profile;
        assert_eq!(copy.authentication, AuthenticationType::EntraBrowser);
        assert_eq!(copy.entra_account, original.entra_account);

        original.authentication = AuthenticationType::AwsRdsIam {
            username: "iam".into(),
            region: "us-west-2".into(),
            profile: Some("work".into()),
        };
        let copy = original.duplicate().profile;
        assert_eq!(copy.authentication, original.authentication);
    }

    #[test]
    fn duplicate_named_uses_the_given_name_and_two_duplicates_differ() {
        let original = fully_populated_profile();
        let a = original.duplicate_named("Warehouse (copy 7)").profile;
        let b = original.duplicate().profile;
        assert_eq!(a.name, "Warehouse (copy 7)");
        assert_ne!(a.id, b.id);
        assert!(
            keychain_keys(&a)
                .iter()
                .all(|k| !keychain_keys(&b).contains(k))
        );
        assert_eq!(copy_name(&a.name), "Warehouse (copy 8)");
    }

    #[test]
    fn azure_sql_profiles_use_secure_defaults() {
        let profile = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        assert_eq!(profile.port, 1433);
        assert_eq!(profile.database.as_deref(), Some("sales"));
        assert_eq!(profile.authentication, AuthenticationType::EntraBrowser);
        assert_eq!(profile.environment, ConnectionEnvironment::Production);
        assert!(profile.block_all_but_select);
        assert!(!profile.auto_wrap_rollback);
        assert!(profile.encrypt);
        assert!(!profile.trust_server_certificate);
    }

    #[test]
    fn azure_entra_profile_previews_cleanly_without_secrets() {
        let profile = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        let preview = profile.preview();
        assert!(preview.is_azure);
        assert_eq!(preview.endpoint, "tcp:prod.database.windows.net,1433");
        assert_eq!(preview.authentication, "Microsoft Entra (browser)");
        assert_eq!(preview.environment, ConnectionEnvironment::Production);
        assert!(preview.block_all_but_select);
        assert!(preview.encrypt);
        assert!(preview.warnings.is_empty(), "{:?}", preview.warnings);
    }

    #[test]
    fn azure_profile_warns_on_insecure_and_missing_database() {
        let mut profile = ConnectionProfile::azure_sql_entra_device_code(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        profile.encrypt = false;
        profile.database = None;
        let preview = profile.preview();
        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("requires encryption"))
        );
        assert!(
            preview
                .warnings
                .iter()
                .any(|w| w.contains("specify a database"))
        );
    }

    #[test]
    fn sql_login_preview_redacts_secret_and_detects_non_azure() {
        let profile = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "hunter2".to_string(),
            },
        );
        let preview = profile.preview();
        assert!(!preview.is_azure);
        assert_eq!(preview.authentication, "SQL login (sa)");
        assert_eq!(profile.environment, ConnectionEnvironment::Local);
        assert!(!profile.auto_wrap_rollback);
        assert!(!profile.block_all_but_select);
        // The secret value must never appear anywhere in the preview.
        let json = serde_json::to_string(&preview).unwrap();
        assert!(!json.contains("hunter2"));
    }

    #[test]
    fn connection_environment_serializes_as_plan_values() {
        let json = serde_json::to_string(&ConnectionEnvironment::Production).unwrap();
        assert_eq!(json, "\"PRODUCTION\"");

        let environment: ConnectionEnvironment = serde_json::from_str("\"STAGING\"").unwrap();
        assert_eq!(environment, ConnectionEnvironment::Staging);
    }

    #[test]
    fn legacy_connection_profile_json_gets_safe_defaults() {
        let json = r#"{
            "id":"00000000-0000-0000-0000-000000000000",
            "name":"local",
            "server":"localhost",
            "port":1433,
            "database":null,
            "authentication":{"SqlLogin":{"username":"sa","secret_ref":{"provider":"InMemory","key":"secret"}}},
            "encrypt":true,
            "trust_server_certificate":false,
            "timeout_seconds":30,
            "application_name":"SQLLY"
        }"#;

        let profile: ConnectionProfile = serde_json::from_str(json).unwrap();
        assert_eq!(profile.environment, ConnectionEnvironment::Local);
        assert_eq!(profile.client, None);
        assert_eq!(profile.project, None);
        assert!(!profile.auto_wrap_rollback);
        assert!(!profile.block_all_but_select);
        // New fields default safely on legacy JSON.
        assert!(!profile.rollback_policy_defaulted);
        assert_eq!(profile.entra_account, None);
        assert_eq!(profile.provenance, ConnectionProvenance::Manual);
        assert_eq!(profile.status, ConnectionStatus::default());
        // A profile saved before routing existed must default to Direct.
        assert_eq!(profile.route, ConnectionRoute::Direct);
        assert_eq!(profile.startup_sql, None);
        assert_eq!(profile.notes, None);
    }

    #[test]
    fn connection_notes_round_trip_and_stay_out_of_json_when_empty() {
        let mut profile = ConnectionProfile::sql_login(
            "Billing",
            "db.example.com",
            "app",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("\"notes\""), "{json}");

        profile.notes = Some("Owned by the billing team.\nMaintenance: Sundays 02:00 UTC".into());
        let json = serde_json::to_string(&profile).unwrap();
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.notes.as_deref(),
            Some("Owned by the billing team.\nMaintenance: Sundays 02:00 UTC")
        );
    }

    #[test]
    fn connection_route_defaults_to_direct() {
        assert_eq!(ConnectionRoute::default(), ConnectionRoute::Direct);
        // Constructors produce Direct profiles.
        let sql = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        assert_eq!(sql.route, ConnectionRoute::Direct);
        let azure = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        assert_eq!(azure.route, ConnectionRoute::Direct);
    }

    #[test]
    fn via_relay_profile_round_trips_through_json() {
        let mut profile = ConnectionProfile::sql_login(
            "remote",
            "10.0.0.5",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        profile.route = ConnectionRoute::ViaRelay {
            relay_device_id: "dev_relay_42".to_string(),
            pair_id: "pair_abc".to_string(),
            relay_hint: Some("relay-eu.sqlly.app:443".to_string()),
        };

        let json = serde_json::to_string(&profile).unwrap();
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.route, profile.route);
        match restored.route {
            ConnectionRoute::ViaRelay {
                relay_device_id,
                pair_id,
                relay_hint,
            } => {
                assert_eq!(relay_device_id, "dev_relay_42");
                assert_eq!(pair_id, "pair_abc");
                assert_eq!(relay_hint.as_deref(), Some("relay-eu.sqlly.app:443"));
            }
            other => panic!("expected ViaRelay, got {other:?}"),
        }
    }

    /// The ClickHouse native port is optional in both directions: profiles
    /// saved before it existed decode as `None`, and `None` is not written,
    /// so older builds reading a newer store see nothing new.
    #[test]
    fn clickhouse_native_port_is_backward_compatible() {
        let mut profile = ConnectionProfile::sql_login(
            "ch",
            "ch.internal",
            "default",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        profile.database_type = DatabaseType::ClickHouse;
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("clickhouse_native_port"), "{json}");
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.clickhouse_native_port, None);

        profile.clickhouse_native_port = Some(19000);
        let json = serde_json::to_string(&profile).unwrap();
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.clickhouse_native_port, Some(19000));
    }

    #[test]
    fn via_relay_hint_is_optional_and_defaults_when_absent() {
        // A ViaRelay route persisted without a relay_hint must deserialize with
        // relay_hint == None (the hint is advisory, not identity).
        let json = r#"{"ViaRelay":{"relay_device_id":"dev_1","pair_id":"pair_1"}}"#;
        let route: ConnectionRoute = serde_json::from_str(json).unwrap();
        assert_eq!(
            route,
            ConnectionRoute::ViaRelay {
                relay_device_id: "dev_1".to_string(),
                pair_id: "pair_1".to_string(),
                relay_hint: None,
            }
        );
    }

    #[test]
    fn provenance_serializes_kebab_case() {
        assert_eq!(
            serde_json::to_string(&ConnectionProvenance::AzureDiscovered).unwrap(),
            "\"azure-discovered\""
        );
        let p: ConnectionProvenance = serde_json::from_str("\"filesystem\"").unwrap();
        assert_eq!(p, ConnectionProvenance::Filesystem);
    }

    #[test]
    fn entra_binding_and_provenance_survive_round_trip_in_preview() {
        let mut profile = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        profile.provenance = ConnectionProvenance::AzureDiscovered;
        profile.entra_account = Some(EntraAccountBinding {
            account_id: "home-a:tenant-a".to_string(),
            tenant_id: "tenant-a".to_string(),
            username: Some("dba@contoso.com".to_string()),
        });
        let preview = profile.preview();
        assert_eq!(preview.provenance, ConnectionProvenance::AzureDiscovered);
        assert_eq!(
            preview.entra_account.as_ref().unwrap().tenant_id,
            "tenant-a"
        );
    }

    #[test]
    fn catalog_identity_matches_legacy_engine_format() {
        // The format keys on-disk catalog files — it must stay byte-stable.
        let profile = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            " prod.database.windows.net ",
            "sales",
        );
        assert_eq!(
            profile.catalog_identity(),
            "prod.database.windows.net|1433|sales|entra-browser|true|false"
        );
        let sql = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        assert_eq!(sql.catalog_identity(), "localhost|1433||sql:sa|true|false");
    }

    #[test]
    fn pool_identity_separates_entra_accounts_on_same_server() {
        let mut a = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        let mut b = a.clone();
        a.entra_account = Some(EntraAccountBinding {
            account_id: "home-a:tenant-a".to_string(),
            tenant_id: "tenant-a".to_string(),
            username: Some("alice@contoso.com".to_string()),
        });
        b.entra_account = Some(EntraAccountBinding {
            account_id: "home-b:tenant-b".to_string(),
            tenant_id: "tenant-b".to_string(),
            username: Some("bob@fabrikam.com".to_string()),
        });
        // Same catalog scope, different pool scopes.
        assert_eq!(a.catalog_identity(), b.catalog_identity());
        assert_ne!(a.pool_identity(), b.pool_identity());
        assert!(a.pool_identity().contains("home-a:tenant-a|tenant-a"));
        assert!(b.pool_identity().contains("home-b:tenant-b|tenant-b"));
    }

    #[test]
    fn pool_identity_ignores_token_and_is_stable_across_rotation() {
        let mut profile = ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        );
        profile.entra_account = Some(EntraAccountBinding {
            account_id: "home-a:tenant-a".to_string(),
            tenant_id: "tenant-a".to_string(),
            username: None,
        });
        let before = profile.pool_identity();
        profile.runtime_entra_access_token = Some("token-one".to_string());
        let mid = profile.pool_identity();
        profile.runtime_entra_access_token = Some("token-two".to_string());
        assert_eq!(before, mid);
        assert_eq!(mid, profile.pool_identity());
        assert!(!before.contains("token-one"));
    }

    #[test]
    fn pool_identity_unbound_entra_profiles_fall_back_to_profile_id() {
        // Two unbound Entra profiles with identical server/db must not share
        // a warm client — they may represent different signed-in users.
        let a =
            ConnectionProfile::azure_sql_entra_browser("one", "prod.database.windows.net", "sales");
        let b =
            ConnectionProfile::azure_sql_entra_browser("two", "prod.database.windows.net", "sales");
        assert_eq!(a.catalog_identity(), b.catalog_identity());
        assert_ne!(a.pool_identity(), b.pool_identity());
        // The same profile's identity is stable across calls.
        assert_eq!(a.pool_identity(), a.pool_identity());
    }

    #[test]
    fn pool_identity_matches_catalog_identity_for_sql_auth() {
        let sql = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        assert_eq!(sql.pool_identity(), sql.catalog_identity());
    }

    #[test]
    fn connection_status_records_success_and_redacted_failure() {
        let now = Utc::now();
        let mut status = ConnectionStatus::default();
        assert_eq!(status.last_test_status, None);

        status.record_connected(now);
        assert_eq!(
            status.last_test_status,
            Some(ConnectionTestStatus::Succeeded)
        );
        assert_eq!(status.last_connected_at, Some(now));
        assert_eq!(status.last_error_summary, None);

        status.record_failure(now, "Login failed for user; password=hunter2 in connstr");
        assert_eq!(status.last_test_status, Some(ConnectionTestStatus::Failed));
        let summary = status.last_error_summary.clone().unwrap();
        assert!(
            !summary.contains("hunter2"),
            "summary leaked secret: {summary}"
        );
        assert!(summary.contains("password=<redacted>"));
    }

    #[test]
    fn keyed_failures_keep_their_key_and_redacted_arguments() {
        let now = Utc::now();
        let mut status = ConnectionStatus::default();
        let message = crate::DisplayMessage::new(
            "services.errors.connection",
            "Connection error: refused password=hunter2",
        )
        .with_arg("detail", "refused password=hunter2");
        status.record_failure_message(now, &message);
        assert_eq!(status.last_test_status, Some(ConnectionTestStatus::Failed));
        let summary = status.last_error_summary.clone().unwrap();
        assert!(summary.starts_with("Connection error: refused"));
        assert!(!summary.contains("hunter2"));
        let stored = status.last_error_message.clone().unwrap();
        assert_eq!(stored.key, "services.errors.connection");
        assert_eq!(stored.arguments.len(), 1);
        assert_eq!(stored.arguments[0].0, "detail");
        assert!(!stored.arguments[0].1.contains("hunter2"));

        // A raw (driver) failure afterwards drops the stale key.
        status.record_failure(now, "server closed the connection");
        assert_eq!(status.last_error_message, None);
        status.record_failure_message(now, &crate::DisplayMessage::raw("plain"));
        assert_eq!(status.last_error_message, None);
        status.record_test_success(now);
        assert_eq!(status.last_error_summary, None);
    }

    #[test]
    fn status_records_without_a_stored_message_still_deserialize() {
        let old = r#"{"last_connected_at":null,"last_tested_at":null,"last_test_status":"failed","last_error_summary":"Login failed"}"#;
        let status: ConnectionStatus = serde_json::from_str(old).unwrap();
        assert_eq!(status.last_error_summary.as_deref(), Some("Login failed"));
        assert_eq!(status.last_error_message, None);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("last_error_message"), "{json}");
    }

    #[test]
    fn redact_error_summary_strips_secrets_and_caps_length() {
        let redacted = redact_error_summary("token=abc123def; rest of message");
        assert!(!redacted.contains("abc123def"));
        assert!(redacted.contains("token=<redacted>"));

        let long = "x".repeat(500);
        let capped = redact_error_summary(&long);
        assert!(capped.chars().count() <= 201);
    }

    #[test]
    fn redact_error_summary_handles_multiple_keys_and_repeats() {
        // The first replacement shortens the string; later keys must be
        // located in the *updated* text (stale offsets used to walk off the
        // end of the shrunken string and panic or leave secrets behind).
        let raw = format!(
            "login failed: password={}; then token=tok-secret-1 and pwd=hunter2",
            "a".repeat(60)
        );
        let redacted = redact_error_summary(&raw);
        assert!(!redacted.contains("aaaa"), "{redacted}");
        assert!(!redacted.contains("tok-secret-1"), "{redacted}");
        assert!(!redacted.contains("hunter2"), "{redacted}");
        assert!(redacted.contains("password=<redacted>"), "{redacted}");
        assert!(redacted.contains("token=<redacted>"), "{redacted}");
        assert!(redacted.contains("pwd=<redacted>"), "{redacted}");

        // The same key twice: both occurrences are redacted.
        let twice = redact_error_summary("password=one password=two");
        assert!(!twice.contains("one") && !twice.contains("two"), "{twice}");
    }

    #[test]
    fn tls_disabled_non_azure_warns_clear_text() {
        let mut profile = ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        profile.encrypt = false;
        let preview = profile.preview();
        assert!(preview.warnings.iter().any(|w| w.contains("clear text")));
    }

    #[test]
    fn secret_never_leaks_across_preview_status_and_display_surfaces() {
        // A profile carrying a real-looking secret in its SecretRef key.
        let secret = "P@ssw0rd-do-not-leak";
        let mut profile = ConnectionProfile::sql_login(
            "prod-db",
            "db.contoso.com",
            "appuser",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: secret.to_string(),
            },
        );
        // Record a failure whose raw text embeds the secret as a connstr token.
        profile.status.record_failure(
            Utc::now(),
            format!("connection failed: pwd={secret}; server=db.contoso.com"),
        );

        // 1) Preview (the secret-free projection rendered in UI/CLI) must not
        //    contain the secret anywhere in its serialized form.
        let preview = profile.preview();
        let preview_json = serde_json::to_string(&preview).unwrap();
        assert!(
            !preview_json.contains(secret),
            "secret leaked into preview: {preview_json}"
        );

        // 2) The auth display string must not contain the secret.
        assert!(!preview.authentication.contains(secret));

        // 3) The persisted status error summary must be redacted.
        let summary = profile.status.last_error_summary.clone().unwrap();
        assert!(
            !summary.contains(secret),
            "secret leaked into status: {summary}"
        );
        assert!(summary.contains("pwd=<redacted>"));
    }

    #[test]
    fn legacy_profile_json_without_ssh_tunnel_deserializes_to_none() {
        let json = r#"{
            "id": "00000000-0000-0000-0000-000000000001",
            "name": "legacy",
            "server": "localhost",
            "port": 1433,
            "database": null,
            "authentication": {
                "SqlLogin": {
                    "username": "sa",
                    "secret_ref": { "provider": "InMemory", "key": "x" }
                }
            },
            "encrypt": true,
            "trust_server_certificate": false,
            "timeout_seconds": 30,
            "application_name": "SQLLY"
        }"#;
        let profile: ConnectionProfile = serde_json::from_str(json).unwrap();
        assert!(profile.ssh_tunnel.is_none());
    }

    #[test]
    fn profile_without_masking_field_decodes_default() {
        let profile = ConnectionProfile::sql_login(
            "n",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        let json = serde_json::to_string(&profile).unwrap();
        assert!(
            !json.contains("masking"),
            "empty overrides must omit the field: {json}"
        );
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert!(back.masking.is_empty());
    }

    fn hidden_schema_test_profile() -> ConnectionProfile {
        ConnectionProfile::sql_login(
            "n",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        )
    }

    #[test]
    fn profile_without_hidden_schemas_serializes_as_before() {
        let profile = hidden_schema_test_profile();
        let json = serde_json::to_string(&profile).unwrap();
        assert!(
            !json.contains("hidden_schemas"),
            "a profile with nothing hidden must omit the field: {json}"
        );
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert!(back.hidden_schemas.is_empty());
    }

    #[test]
    fn legacy_profile_json_without_hidden_schemas_decodes_empty() {
        let mut value = serde_json::to_value(hidden_schema_test_profile()).unwrap();
        value.as_object_mut().unwrap().remove("hidden_schemas");
        let back: ConnectionProfile = serde_json::from_value(value).unwrap();
        assert!(back.hidden_schemas.is_empty());
        assert!(!back.is_schema_hidden("db", "dbo"));
    }

    #[test]
    fn display_time_zone_is_omitted_when_unset_and_round_trips() {
        let mut profile = hidden_schema_test_profile();
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("display_time_zone"), "{json}");
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.display_time_zone, None);
        for zone in [
            DisplayTimeZone::Utc,
            DisplayTimeZone::Local,
            DisplayTimeZone::Named("Europe/Berlin".into()),
        ] {
            profile.display_time_zone = Some(zone.clone());
            let json = serde_json::to_string(&profile).unwrap();
            let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
            assert_eq!(back.display_time_zone, Some(zone));
        }
    }

    #[test]
    fn hidden_schemas_round_trip_per_database() {
        let mut profile = hidden_schema_test_profile();
        assert!(profile.hide_schema("Sales", "staging"));
        assert!(profile.hide_schema("Sales", "audit"));
        assert!(!profile.hide_schema("Sales", "audit"), "already hidden");
        assert!(profile.hide_schema("Hr", "staging"));
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("hidden_schemas"), "{json}");
        let back: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back, profile);
        assert!(back.is_schema_hidden("Sales", "staging"));
        assert!(back.is_schema_hidden("Hr", "staging"));
        // Per database: the same schema name elsewhere stays visible.
        assert!(!back.is_schema_hidden("Ops", "staging"));
        assert_eq!(back.hidden_schema_count("Sales"), 2);
        assert_eq!(back.hidden_schema_count("Ops"), 0);
    }

    #[test]
    fn show_hidden_schemas_clears_only_that_database() {
        let mut profile = hidden_schema_test_profile();
        profile.hide_schema("Sales", "staging");
        profile.hide_schema("Sales", "audit");
        profile.hide_schema("Hr", "staging");
        assert_eq!(profile.show_hidden_schemas("Sales"), 2);
        assert_eq!(profile.hidden_schema_count("Sales"), 0);
        assert!(profile.is_schema_hidden("Hr", "staging"));
        assert_eq!(profile.show_hidden_schemas("Sales"), 0);
        assert_eq!(profile.show_hidden_schemas("Hr"), 1);
        // Nothing left hidden: the profile serializes exactly as before.
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("hidden_schemas"), "{json}");
    }

    #[test]
    fn ssh_keyfile_auth_round_trips() {
        let tunnel = SshTunnelConfig {
            target: SshHop {
                host: "bastion.example.com".to_string(),
                port: 22,
                username: "deploy".to_string(),
                auth: SshAuth::KeyFile {
                    path: "/home/me/.ssh/id_ed25519".to_string(),
                    passphrase: None,
                },
            },
            jump: None,
            keepalive: Default::default(),
        };
        let json = serde_json::to_string(&tunnel).unwrap();
        let restored: SshTunnelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(tunnel, restored);
    }

    #[test]
    fn ssh_hop_default_port_is_22() {
        let json = r#"{"host":"h","username":"u"}"#;
        let hop: SshHop = serde_json::from_str(json).unwrap();
        assert_eq!(hop.port, 22);
    }

    #[test]
    fn ssh_password_serialization_has_no_inline_secret() {
        let tunnel = SshTunnelConfig {
            target: SshHop {
                host: "bastion".to_string(),
                port: 22,
                username: "u".to_string(),
                auth: SshAuth::Password {
                    secret_ref: SecretRef {
                        provider: SecretProvider::MacOsKeychain,
                        key: "11111111-1111-1111-1111-111111111111".to_string(),
                    },
                },
            },
            jump: None,
            keepalive: Default::default(),
        };
        let json = serde_json::to_string(&tunnel).unwrap();
        assert!(!json.contains("password"));
        assert!(json.contains("11111111-1111-1111-1111-111111111111"));
    }

    #[test]
    fn transport_chain_round_trips_json() {
        let chain = TransportChain {
            proxy: Some(ProxyHop {
                kind: ProxyKind::Socks5,
                host: "proxy.corp".to_string(),
                port: 1080,
                username: Some("proxyuser".to_string()),
                password: Some(SecretRef {
                    provider: SecretProvider::MacOsKeychain,
                    key: "proxy-secret".to_string(),
                }),
            }),
            ssh: Some(SshTunnelConfig {
                target: SshHop {
                    host: "bastion".to_string(),
                    port: 22,
                    username: "jump".to_string(),
                    auth: SshAuth::Agent,
                },
                jump: None,
                keepalive: Default::default(),
            }),
            kube: None,
            ssm: None,
        };
        let json = serde_json::to_string(&chain).unwrap();
        let restored: TransportChain = serde_json::from_str(&json).unwrap();
        assert_eq!(chain, restored);
    }

    #[test]
    fn transport_chain_validate_rejects_empty_host_and_zero_port() {
        let chain = TransportChain {
            proxy: Some(ProxyHop {
                kind: ProxyKind::HttpConnect,
                host: "".to_string(),
                port: 0,
                username: None,
                password: None,
            }),
            ssh: None,
            kube: None,
            ssm: None,
        };
        let errors = chain.validate();
        assert!(errors.iter().any(|e| e.contains("host")));
        assert!(errors.iter().any(|e| e.contains("port")));
    }

    fn sample_ssm() -> SsmPortForward {
        SsmPortForward {
            target: "i-0123456789abcdef0".to_string(),
            region: Some("us-east-1".to_string()),
            profile: Some("prod".to_string()),
            remote_host: None,
        }
    }

    #[test]
    fn ssm_aws_args_shape() {
        assert_eq!(
            sample_ssm().aws_args("mydb.abc123.us-east-1.rds.amazonaws.com", 5432, 49152),
            vec![
                "ssm",
                "start-session",
                "--target",
                "i-0123456789abcdef0",
                "--document-name",
                "AWS-StartPortForwardingSessionToRemoteHost",
                "--parameters",
                r#"{"host":["mydb.abc123.us-east-1.rds.amazonaws.com"],"portNumber":["5432"],"localPortNumber":["49152"]}"#,
                "--region",
                "us-east-1",
                "--profile",
                "prod",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn ssm_aws_args_omit_blank_region_and_profile() {
        let ssm = SsmPortForward {
            target: " i-abc ".to_string(),
            region: Some("  ".to_string()),
            profile: None,
            remote_host: None,
        };
        let args = ssm.aws_args("db.internal", 3306, 50000);
        assert!(!args.iter().any(|a| a == "--region" || a == "--profile"));
        assert_eq!(args[3], "i-abc");
        assert_eq!(args.len(), 8);
    }

    #[test]
    fn ssm_remote_host_defaults_to_the_server() {
        let mut ssm = sample_ssm();
        assert_eq!(ssm.remote_host_or(" db.example.com "), "db.example.com");
        ssm.remote_host = Some("10.0.4.12".to_string());
        assert_eq!(ssm.remote_host_or("db.example.com"), "10.0.4.12");
        ssm.remote_host = Some(" ".to_string());
        assert_eq!(ssm.remote_host_or("db.example.com"), "db.example.com");
    }

    #[test]
    fn ssm_validate_rejects_missing_target_and_shell_characters() {
        let empty = SsmPortForward::default();
        assert!(
            empty
                .validate()
                .iter()
                .any(|e| e.contains("target is required"))
        );
        let bad = SsmPortForward {
            target: "i-1;rm -rf".to_string(),
            region: Some("us east".to_string()),
            profile: Some("a\"b".to_string()),
            remote_host: Some("db,portNumber=22".to_string()),
        };
        let errors = bad.validate();
        assert_eq!(errors.len(), 4, "{errors:?}");
        assert!(sample_ssm().validate().is_empty());
        let ecs = SsmPortForward {
            target: "ecs:cluster_task123_runtime456".to_string(),
            ..SsmPortForward::default()
        };
        assert!(ecs.validate().is_empty());
    }

    #[test]
    fn ssm_round_trips_and_old_profiles_without_it_still_load() {
        let chain = TransportChain::ssm_only(sample_ssm());
        let json = serde_json::to_string(&chain).unwrap();
        let restored: TransportChain = serde_json::from_str(&json).unwrap();
        assert_eq!(chain, restored);
        assert_eq!(chain.hop_count(), 1);
        assert!(!chain.is_empty());

        let old: TransportChain = serde_json::from_str("{}").unwrap();
        assert!(old.ssm.is_none());
        assert!(!serde_json::to_string(&old).unwrap().contains("ssm"));

        let minimal: SsmPortForward = serde_json::from_str(r#"{"target":"i-1"}"#).unwrap();
        assert_eq!(minimal.region, None);
        assert_eq!(minimal.remote_host, None);
    }

    #[test]
    fn ssm_cannot_be_combined_with_other_hops() {
        let mut chain = TransportChain::ssm_only(sample_ssm());
        chain.proxy = Some(ProxyHop {
            kind: ProxyKind::Socks5,
            host: "proxy".to_string(),
            port: 1080,
            username: None,
            password: None,
        });
        assert!(
            chain
                .validate()
                .iter()
                .any(|e| e.contains("AWS Session Manager cannot be combined"))
        );
    }

    #[test]
    fn kube_port_forward_round_trips_json() {
        let kube = KubePortForward {
            kubeconfig: Some("/home/me/.kube/config".to_string()),
            context: Some("dev-cluster".to_string()),
            namespace: "data".to_string(),
            target_kind: KubeTargetKind::Service,
            target_name: "postgres".to_string(),
            remote_port: 5432,
        };
        let json = serde_json::to_string(&kube).unwrap();
        let restored: KubePortForward = serde_json::from_str(&json).unwrap();
        assert_eq!(kube, restored);
    }

    #[test]
    fn kube_port_forward_validate_rejects_empty_target_and_zero_port() {
        let kube = KubePortForward {
            kubeconfig: None,
            context: None,
            namespace: "default".to_string(),
            target_kind: KubeTargetKind::Pod,
            target_name: "".to_string(),
            remote_port: 0,
        };
        let errors = kube.validate();
        assert!(errors.iter().any(|e| e.contains("target")));
        assert!(errors.iter().any(|e| e.contains("port")));
    }

    #[test]
    fn kube_kubectl_args_shape() {
        let kube = KubePortForward {
            kubeconfig: Some("/cfg".to_string()),
            context: Some("ctx".to_string()),
            namespace: "ns".to_string(),
            target_kind: KubeTargetKind::Service,
            target_name: "pg".to_string(),
            remote_port: 5432,
        };
        assert_eq!(
            kube.kubectl_args(),
            vec![
                "--kubeconfig",
                "/cfg",
                "--context",
                "ctx",
                "-n",
                "ns",
                "port-forward",
                "--address",
                "127.0.0.1",
                "service/pg",
                ":5432"
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn kube_kubectl_args_omits_absent_kubeconfig_and_context() {
        let kube = KubePortForward {
            kubeconfig: None,
            context: None,
            namespace: "default".to_string(),
            target_kind: KubeTargetKind::Pod,
            target_name: "db-0".to_string(),
            remote_port: 3306,
        };
        let args = kube.kubectl_args();
        assert!(!args.iter().any(|a| a == "--kubeconfig"));
        assert!(!args.iter().any(|a| a == "--context"));
        assert_eq!(args.last().map(String::as_str), Some(":3306"));
    }

    #[test]
    fn transport_chain_rejects_kube_with_ssh_or_proxy() {
        let chain = TransportChain {
            proxy: Some(ProxyHop {
                kind: ProxyKind::Socks5,
                host: "proxy".to_string(),
                port: 1080,
                username: None,
                password: None,
            }),
            ssh: None,
            kube: Some(KubePortForward {
                kubeconfig: None,
                context: None,
                namespace: "default".to_string(),
                target_kind: KubeTargetKind::Pod,
                target_name: "db".to_string(),
                remote_port: 5432,
            }),
            ssm: None,
        };
        let errors = chain.validate();
        assert!(
            errors
                .iter()
                .any(|e| e.contains("Kubernetes port-forward cannot be combined"))
        );
    }

    #[test]
    fn inline_transport_migrates_legacy_ssh_tunnel() {
        let tunnel = SshTunnelConfig {
            target: SshHop {
                host: "bastion".to_string(),
                port: 22,
                username: "u".to_string(),
                auth: SshAuth::Agent,
            },
            jump: None,
            keepalive: Default::default(),
        };
        let mut profile = ConnectionProfile::sql_login(
            "t",
            "db",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "x".to_string(),
            },
        );
        profile.ssh_tunnel = Some(tunnel.clone());
        let chain = profile.inline_transport();
        assert_eq!(chain.ssh, Some(tunnel));
        assert!(chain.proxy.is_none());
    }

    #[test]
    fn aws_rds_iam_round_trips_json_without_runtime_token() {
        let mut profile = ConnectionProfile::aws_rds_iam(
            "rds",
            "mydb.abc123.us-west-2.rds.amazonaws.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        profile.runtime_cloud_iam_token = Some("t".to_string());
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("\"t\""));
        assert!(!json.contains("runtime_cloud_iam_token"));
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.runtime_cloud_iam_token, None);
        match restored.authentication {
            AuthenticationType::AwsRdsIam {
                username,
                region,
                profile,
            } => {
                assert_eq!(username, "jane");
                assert_eq!(region, "us-west-2");
                assert!(profile.is_none());
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn gcp_cloud_sql_iam_round_trips() {
        let mut profile = ConnectionProfile::gcp_cloud_sql_iam(
            "cloudsql",
            "10.0.0.1",
            5432,
            DatabaseType::Postgres,
            "alice@example.com",
        );
        profile.runtime_cloud_iam_token = Some("tok".to_string());
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("tok"));
        let restored: ConnectionProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.runtime_cloud_iam_token, None);
        match restored.authentication {
            AuthenticationType::GcpCloudSqlIam { username } => {
                assert_eq!(username, "alice@example.com");
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn legacy_sql_login_json_still_deserialises() {
        let json = r#"{
            "id": "11111111-1111-1111-1111-111111111111",
            "name": "local",
            "server": "localhost",
            "port": 1433,
            "database": null,
            "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
            "encrypt": true,
            "trust_server_certificate": false,
            "timeout_seconds": 30,
            "application_name": "SQLLY",
            "auto_wrap_rollback": false,
            "block_all_but_select": false,
            "confirm_data_changes": false,
            "allow_unbounded_mutations": false
        }"#;
        let profile: ConnectionProfile = serde_json::from_str(json).unwrap();
        match profile.authentication {
            AuthenticationType::SqlLogin { username, .. } => assert_eq!(username, "sa"),
            other => panic!("unexpected auth: {other:?}"),
        }
        assert_eq!(profile.runtime_cloud_iam_token, None);
    }

    #[test]
    fn tunnel_profile_round_trips_json() {
        let profile = TunnelProfile {
            id: uuid::Uuid::new_v4(),
            name: "corp-bastion".to_string(),
            chain: TransportChain {
                proxy: None,
                ssh: Some(SshTunnelConfig {
                    target: SshHop {
                        host: "bastion".to_string(),
                        port: 22,
                        username: "deploy".to_string(),
                        auth: SshAuth::Agent,
                    },
                    jump: None,
                    keepalive: Default::default(),
                }),
                kube: None,
                ssm: None,
            },
        };
        let json = serde_json::to_string(&profile).unwrap();
        let restored: TunnelProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(profile, restored);
    }

    #[test]
    fn legacy_mode_maps_booleans() {
        let mut profile = ConnectionProfile::sql_login(
            "tls",
            "db.example.com",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        assert_eq!(profile.tls_mode, TlsMode::Legacy);

        profile.encrypt = false;
        profile.trust_server_certificate = false;
        assert_eq!(profile.effective_tls_mode(), TlsMode::Disable);
        profile.encrypt = true;
        profile.trust_server_certificate = true;
        assert_eq!(profile.effective_tls_mode(), TlsMode::Require);
        profile.trust_server_certificate = false;
        assert_eq!(profile.effective_tls_mode(), TlsMode::VerifyFull);

        // An explicit mode wins over the booleans.
        profile.tls_mode = TlsMode::VerifyCa;
        assert_eq!(profile.effective_tls_mode(), TlsMode::VerifyCa);

        // A profile serialized before the field existed decodes as Legacy.
        let mut value = serde_json::to_value(&profile).unwrap();
        value.as_object_mut().unwrap().remove("tls_mode");
        let restored: ConnectionProfile = serde_json::from_value(value).unwrap();
        assert_eq!(restored.tls_mode, TlsMode::Legacy);
        assert_eq!(restored.effective_tls_mode(), TlsMode::VerifyFull);
    }
}
