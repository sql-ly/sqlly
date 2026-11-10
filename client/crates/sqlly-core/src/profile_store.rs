//! Persistent connection-profile store with a recent-connections list and
//! secret-free import/export.
//!
//! This is intentionally separate from [`crate::registered_servers`]: the
//! Registered Servers tree is about *organizing* servers into named groups (a
//! presentation concern), whereas the [`ProfileStore`] is the flat,
//! authoritative set of connection profiles the app knows about, plus
//! most-recently-used ordering for the "Recent" UI. The macOS/CLI layer persists
//! this store as JSON; the core owns the model and invariants.
//!
//! Secrets are never embedded in a profile (only a [`crate::SecretRef`] is), so
//! the store itself holds no secret material. Export additionally drops the
//! secret *reference* so a shared bundle cannot even hint at where a secret
//! lives; import marks profiles as [`crate::ConnectionProvenance::Imported`] and
//! requires the operator to re-attach a secret reference before connecting.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::DatabaseType;
use crate::connection::{
    AuthenticationType, ConnectionId, ConnectionProfile, ConnectionProvenance, ConnectionRoute,
    EntraAccountBinding, MaskingOverrides, SecretProvider, SecretRef,
};

/// The maximum number of entries retained in the recent-connections list.
pub const DEFAULT_RECENT_LIMIT: usize = 20;

/// A persistent set of connection profiles plus most-recently-used ordering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileStore {
    profiles: Vec<ConnectionProfile>,
    /// Connection ids in most-recent-first order. May reference profiles that
    /// have since been removed; such stale ids are pruned lazily on read.
    recent: Vec<Uuid>,
    recent_limit: usize,
}

impl Default for ProfileStore {
    fn default() -> Self {
        Self {
            profiles: Vec::new(),
            recent: Vec::new(),
            recent_limit: DEFAULT_RECENT_LIMIT,
        }
    }
}

impl ProfileStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_recent_limit(recent_limit: usize) -> Self {
        Self {
            recent_limit: recent_limit.max(1),
            ..Self::default()
        }
    }

    /// Inserts a new profile or replaces an existing one with the same id.
    /// Returns the profile's [`ConnectionId`].
    pub fn upsert(&mut self, profile: ConnectionProfile) -> ConnectionId {
        let id = profile.id;
        if let Some(slot) = self.profiles.iter_mut().find(|p| p.id == id) {
            *slot = profile;
        } else {
            self.profiles.push(profile);
        }
        id
    }

    /// Removes a profile and drops it from the recent list. Returns it if found.
    pub fn remove(&mut self, id: ConnectionId) -> Option<ConnectionProfile> {
        self.recent.retain(|r| *r != id.0);
        if let Some(pos) = self.profiles.iter().position(|p| p.id == id) {
            Some(self.profiles.remove(pos))
        } else {
            None
        }
    }

    pub fn get(&self, id: ConnectionId) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    pub fn profiles(&self) -> &[ConnectionProfile] {
        &self.profiles
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    /// Records a successful use of `id`, moving it to the front of the recent
    /// list (deduplicated) and trimming to the configured limit. No-op for ids
    /// not present in the store.
    pub fn mark_used(&mut self, id: ConnectionId) {
        if !self.profiles.iter().any(|p| p.id == id) {
            return;
        }
        self.recent.retain(|r| *r != id.0);
        self.recent.insert(0, id.0);
        self.recent.truncate(self.recent_limit);
    }

    /// The recent profiles, most-recent-first, skipping any stale ids whose
    /// profile has been removed.
    pub fn recent(&self) -> Vec<&ConnectionProfile> {
        self.recent
            .iter()
            .filter_map(|rid| self.profiles.iter().find(|p| p.id.0 == *rid))
            .collect()
    }

    /// Produces a secret-free export bundle suitable for sharing or backup. The
    /// secret *reference* is dropped entirely; the recipient re-attaches one.
    pub fn export(&self) -> ProfileExportBundle {
        ProfileExportBundle {
            version: PROFILE_EXPORT_VERSION,
            profiles: self.profiles.iter().map(export_profile).collect(),
            clients: Vec::new(),
            projects: Vec::new(),
            environments: Vec::new(),
            sql_auth_accounts: Vec::new(),
        }
    }

    /// Imports profiles from a bundle. Each imported profile receives a fresh id
    /// (to avoid clobbering existing ones), is marked
    /// [`ConnectionProvenance::Imported`], and gets a placeholder secret
    /// reference the operator must replace. Returns the new ids.
    pub fn import(&mut self, bundle: &ProfileExportBundle) -> Vec<ConnectionId> {
        let mut ids = Vec::with_capacity(bundle.profiles.len());
        for exported in &bundle.profiles {
            let profile = exported.into_profile();
            ids.push(self.upsert(profile));
        }
        ids
    }
}

/// Current on-disk/export schema version for [`ProfileExportBundle`].
pub const PROFILE_EXPORT_VERSION: u32 = 1;

/// Secret-free safety toggles for a client, project, or environment entity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExportedSafetyFeatures {
    #[serde(default)]
    pub use_outer_transaction_rollback_scope: bool,
    #[serde(default)]
    pub only_allow_select: bool,
    #[serde(default)]
    pub use_confirmation_dialog: bool,
    #[serde(default)]
    pub require_ai_review_for_data_changes: bool,
    #[serde(default)]
    pub allow_unconstrained_update: bool,
    #[serde(default)]
    pub allow_unconstrained_delete: bool,
    #[serde(default)]
    pub require_touch_id_to_connect: bool,
    #[serde(default)]
    pub require_touch_id_for_non_select: bool,
    #[serde(default)]
    pub is_production: bool,
}

/// A secret-free client entity for export/import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedClientEntity {
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: ExportedSafetyFeatures,
}

/// A secret-free project entity for export/import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedProjectEntity {
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: ExportedSafetyFeatures,
}

/// A secret-free environment entity for export/import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedEnvironmentEntity {
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: ExportedSafetyFeatures,
}

/// Non-secret metadata for a reusable SQL authentication account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSqlAuthAccount {
    pub name: String,
    pub username: String,
}

/// A secret-free, portable bundle of connection profiles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileExportBundle {
    pub version: u32,
    pub profiles: Vec<ExportedProfile>,
    #[serde(default)]
    pub clients: Vec<ExportedClientEntity>,
    #[serde(default)]
    pub projects: Vec<ExportedProjectEntity>,
    #[serde(default)]
    pub environments: Vec<ExportedEnvironmentEntity>,
    #[serde(default)]
    pub sql_auth_accounts: Vec<ExportedSqlAuthAccount>,
}

/// A single profile as it appears in an export bundle: every field *except*
/// anything secret or secret-referencing. Authentication is reduced to its
/// non-secret shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedProfile {
    pub name: String,
    pub server: String,
    pub port: u16,
    pub database: Option<String>,
    pub environment: crate::connection::ConnectionEnvironment,
    pub client: Option<String>,
    pub project: Option<String>,
    /// The auth *kind* only; for SQL logins the username is retained but the
    /// secret reference is intentionally omitted.
    pub authentication: ExportedAuthentication,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
    pub timeout_seconds: u64,
    pub application_name: String,
    pub auto_wrap_rollback: bool,
    pub block_all_but_select: bool,
    /// Added after the first export format shipped; defaults off when reading
    /// older bundles.
    #[serde(default)]
    pub confirm_data_changes: bool,
    /// Added after the first export format shipped. Defaults on when reading
    /// older bundles: the unbounded-statement confirmation is opt-in, so a
    /// bundle that never carried the setting imports without the protection,
    /// like every new profile.
    #[serde(default = "default_allow_unbounded_export")]
    pub allow_unbounded_mutations: bool,
    #[serde(default)]
    pub database_type: DatabaseType,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement_timeout_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_sql: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_key_delimiter: Option<String>,
    /// Sentinel settings without the Sentinel password reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_sentinel: Option<crate::redis_sentinel::RedisSentinelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_cluster: Option<crate::redis_cluster::RedisClusterConfig>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub engine_auto_detect: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_entra_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clickhouse_native_port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entra_account: Option<EntraAccountBinding>,
    #[serde(default)]
    pub route: ConnectionRoute,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_tunnel: Option<ExportedSshTunnel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_profile: Option<uuid::Uuid>,
    #[serde(default, skip_serializing_if = "ExportedTransportChain::is_empty")]
    pub transport: ExportedTransportChain,
    #[serde(default)]
    pub favorite: bool,
    /// TLS verification mode (PostgreSQL/MySQL); `Legacy` for bundles written
    /// before the field existed.
    #[serde(default)]
    pub tls_mode: crate::connection::TlsMode,
    /// Certificate *paths* are secret-free and carried in the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_cert_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_key_file: Option<std::path::PathBuf>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_trust_os_roots: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tls_direct: bool,
}

/// Secret-free proxy hop for export bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedProxyHop {
    pub kind: crate::connection::ProxyKind,
    pub host: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ExportedTransportChain {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<ExportedProxyHop>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<ExportedSshTunnel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kube: Option<crate::connection::KubePortForward>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssm: Option<crate::connection::SsmPortForward>,
}

impl ExportedTransportChain {
    fn is_empty(&self) -> bool {
        self.proxy.is_none() && self.ssh.is_none() && self.kube.is_none() && self.ssm.is_none()
    }
}

/// Secret-free SSH hop for export bundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSshHop {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: ExportedSshAuth,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportedSshAuth {
    Agent,
    KeyFile { path: String },
    Password,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExportedSshTunnel {
    pub target: ExportedSshHop,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<ExportedSshHop>,
    #[serde(
        default,
        skip_serializing_if = "crate::connection::SshKeepalive::is_default"
    )]
    pub keepalive: crate::connection::SshKeepalive,
}

/// Serde default for [`ExportedProfile::allow_unbounded_mutations`]; see the
/// field's doc.
fn default_allow_unbounded_export() -> bool {
    true
}

fn export_ssh_auth(auth: &crate::connection::SshAuth) -> ExportedSshAuth {
    use crate::connection::SshAuth;
    match auth {
        SshAuth::Agent => ExportedSshAuth::Agent,
        SshAuth::KeyFile { path, .. } => ExportedSshAuth::KeyFile { path: path.clone() },
        SshAuth::Password { .. } => ExportedSshAuth::Password,
    }
}

fn export_ssh_hop(hop: &crate::connection::SshHop) -> ExportedSshHop {
    ExportedSshHop {
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
        auth: export_ssh_auth(&hop.auth),
    }
}

fn export_proxy_hop(hop: &crate::connection::ProxyHop) -> ExportedProxyHop {
    ExportedProxyHop {
        kind: hop.kind,
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
    }
}

fn export_transport_chain(chain: &crate::connection::TransportChain) -> ExportedTransportChain {
    ExportedTransportChain {
        proxy: chain.proxy.as_ref().map(export_proxy_hop),
        ssh: chain.ssh.as_ref().map(|t| ExportedSshTunnel {
            target: export_ssh_hop(&t.target),
            jump: t.jump.as_ref().map(export_ssh_hop),
            keepalive: t.keepalive,
        }),
        kube: chain.kube.clone(),
        ssm: chain.ssm.clone(),
    }
}

fn import_proxy_hop(hop: &ExportedProxyHop) -> crate::connection::ProxyHop {
    crate::connection::ProxyHop {
        kind: hop.kind,
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
        password: hop.username.as_ref().map(|_| SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        }),
    }
}

fn import_transport_chain(chain: &ExportedTransportChain) -> crate::connection::TransportChain {
    crate::connection::TransportChain {
        proxy: chain.proxy.as_ref().map(import_proxy_hop),
        ssh: chain
            .ssh
            .as_ref()
            .map(|t| crate::connection::SshTunnelConfig {
                target: import_ssh_hop(&t.target),
                jump: t.jump.as_ref().map(import_ssh_hop),
                keepalive: t.keepalive,
            }),
        kube: chain.kube.clone(),
        ssm: chain.ssm.clone(),
    }
}

fn import_ssh_auth(auth: &ExportedSshAuth) -> crate::connection::SshAuth {
    use crate::connection::{SecretProvider, SecretRef, SshAuth};
    match auth {
        ExportedSshAuth::Agent => SshAuth::Agent,
        ExportedSshAuth::KeyFile { path } => SshAuth::KeyFile {
            path: path.clone(),
            passphrase: None,
        },
        ExportedSshAuth::Password => SshAuth::Password {
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        },
    }
}

fn import_ssh_hop(hop: &ExportedSshHop) -> crate::connection::SshHop {
    crate::connection::SshHop {
        host: hop.host.clone(),
        port: hop.port,
        username: hop.username.clone(),
        auth: import_ssh_auth(&hop.auth),
    }
}

/// Secret-free representation of [`AuthenticationType`] for export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportedAuthentication {
    /// Username retained; no secret reference.
    SqlLogin {
        username: String,
    },
    Integrated,
    EntraDeviceCode,
    EntraBrowser,
    AwsRdsIam {
        username: String,
        region: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
    GcpCloudSqlIam {
        username: String,
    },
    /// libSQL / Turso bearer token; the token itself never leaves the keyring.
    AuthToken,
}

/// Maps a live profile to its secret-free export shape.
pub fn export_profile(profile: &ConnectionProfile) -> ExportedProfile {
    ExportedProfile {
        name: profile.name.clone(),
        server: profile.server.clone(),
        port: profile.port,
        database: profile.database.clone(),
        environment: profile.environment,
        client: profile.client.clone(),
        project: profile.project.clone(),
        authentication: match &profile.authentication {
            AuthenticationType::SqlLogin { username, .. } => ExportedAuthentication::SqlLogin {
                username: username.clone(),
            },
            AuthenticationType::Integrated => ExportedAuthentication::Integrated,
            AuthenticationType::EntraDeviceCode => ExportedAuthentication::EntraDeviceCode,
            AuthenticationType::EntraBrowser => ExportedAuthentication::EntraBrowser,
            AuthenticationType::AwsRdsIam {
                username,
                region,
                profile,
            } => ExportedAuthentication::AwsRdsIam {
                username: username.clone(),
                region: region.clone(),
                profile: profile.clone(),
            },
            AuthenticationType::GcpCloudSqlIam { username } => {
                ExportedAuthentication::GcpCloudSqlIam {
                    username: username.clone(),
                }
            }
            AuthenticationType::AuthToken { .. } => ExportedAuthentication::AuthToken,
        },
        encrypt: profile.encrypt,
        trust_server_certificate: profile.trust_server_certificate,
        timeout_seconds: profile.timeout_seconds,
        application_name: profile.application_name.clone(),
        auto_wrap_rollback: profile.auto_wrap_rollback,
        block_all_but_select: profile.block_all_but_select,
        confirm_data_changes: profile.confirm_data_changes,
        allow_unbounded_mutations: profile.allow_unbounded_mutations,
        database_type: profile.database_type,
        statement_timeout_seconds: profile.statement_timeout_seconds,
        startup_sql: profile.startup_sql.clone(),
        notes: profile.notes.clone(),
        redis_key_delimiter: profile.redis_key_delimiter.clone(),
        redis_sentinel: profile
            .redis_sentinel
            .as_ref()
            .map(crate::redis_sentinel::RedisSentinelConfig::without_secret),
        redis_cluster: profile.redis_cluster.clone(),
        engine_auto_detect: profile.engine_auto_detect,
        redis_entra_user: profile.redis_entra_user.clone(),
        clickhouse_native_port: profile.clickhouse_native_port,
        entra_account: profile.entra_account.clone(),
        route: profile.route.clone(),
        ssh_tunnel: profile.ssh_tunnel.as_ref().map(|t| ExportedSshTunnel {
            target: export_ssh_hop(&t.target),
            jump: t.jump.as_ref().map(export_ssh_hop),
            keepalive: t.keepalive,
        }),
        tunnel_profile: profile.tunnel_profile,
        transport: export_transport_chain(&profile.transport),
        favorite: profile.favorite,
        tls_mode: profile.tls_mode,
        tls_ca_file: profile.tls_ca_file.clone(),
        tls_client_cert_file: profile.tls_client_cert_file.clone(),
        tls_client_key_file: profile.tls_client_key_file.clone(),
        tls_trust_os_roots: profile.tls_trust_os_roots,
        tls_direct: profile.tls_direct,
    }
}

impl ExportedProfile {
    /// Reconstructs a [`ConnectionProfile`] from an exported, secret-free entry.
    /// SQL-login profiles get a placeholder [`SecretRef`] the operator must
    /// replace before connecting; the profile is marked imported.
    pub fn into_profile(&self) -> ConnectionProfile {
        let authentication = match &self.authentication {
            ExportedAuthentication::SqlLogin { username } => AuthenticationType::SqlLogin {
                username: username.clone(),
                // Placeholder: forces secret re-entry. Provider=InMemory + empty
                // key is never a real secret.
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: String::new(),
                },
            },
            ExportedAuthentication::Integrated => AuthenticationType::Integrated,
            ExportedAuthentication::EntraDeviceCode => AuthenticationType::EntraDeviceCode,
            ExportedAuthentication::EntraBrowser => AuthenticationType::EntraBrowser,
            ExportedAuthentication::AwsRdsIam {
                username,
                region,
                profile,
            } => AuthenticationType::AwsRdsIam {
                username: username.clone(),
                region: region.clone(),
                profile: profile.clone(),
            },
            ExportedAuthentication::GcpCloudSqlIam { username } => {
                AuthenticationType::GcpCloudSqlIam {
                    username: username.clone(),
                }
            }
            // Placeholder like SqlLogin: the operator re-enters the token.
            ExportedAuthentication::AuthToken => AuthenticationType::AuthToken {
                secret_ref: SecretRef {
                    provider: SecretProvider::InMemory,
                    key: String::new(),
                },
            },
        };
        ConnectionProfile {
            id: ConnectionId::new(),
            name: self.name.clone(),
            server: self.server.clone(),
            port: self.port,
            database: self.database.clone(),
            database_type: self.database_type,
            environment: self.environment,
            client: self.client.clone(),
            project: self.project.clone(),
            authentication,
            encrypt: self.encrypt,
            trust_server_certificate: self.trust_server_certificate,
            timeout_seconds: self.timeout_seconds,
            statement_timeout_seconds: self.statement_timeout_seconds,
            application_name: self.application_name.clone(),
            auto_wrap_rollback: self.auto_wrap_rollback,
            block_all_but_select: self.block_all_but_select,
            confirm_data_changes: self.confirm_data_changes,
            allow_unbounded_mutations: self.allow_unbounded_mutations,
            production_override: None,
            rollback_policy_defaulted: false,
            entra_account: self.entra_account.clone(),
            runtime_entra_access_token: None,
            runtime_cloud_iam_token: None,
            enforce_read_only: false,
            provenance: ConnectionProvenance::Imported,
            status: crate::connection::ConnectionStatus::default(),
            route: self.route.clone(),
            ssh_tunnel: self
                .ssh_tunnel
                .as_ref()
                .map(|t| crate::connection::SshTunnelConfig {
                    target: import_ssh_hop(&t.target),
                    jump: t.jump.as_ref().map(import_ssh_hop),
                    keepalive: t.keepalive,
                }),
            tunnel_profile: self.tunnel_profile,
            transport: import_transport_chain(&self.transport),
            favorite: self.favorite,
            startup_sql: self.startup_sql.clone(),
            notes: self.notes.clone(),
            redis_key_delimiter: self.redis_key_delimiter.clone(),
            redis_sentinel: self
                .redis_sentinel
                .as_ref()
                .map(crate::redis_sentinel::RedisSentinelConfig::without_secret),
            redis_cluster: self.redis_cluster.clone(),
            engine_auto_detect: self.engine_auto_detect,
            redis_db_aliases: Default::default(),
            redis_visible_databases: None,
            redis_entra_user: self.redis_entra_user.clone(),
            clickhouse_native_port: self.clickhouse_native_port,
            hidden_schemas: Default::default(),
            display_time_zone: None,
            masking: MaskingOverrides::default(),
            tls_mode: self.tls_mode,
            tls_ca_file: self.tls_ca_file.clone(),
            tls_client_cert_file: self.tls_client_cert_file.clone(),
            tls_client_key_file: self.tls_client_key_file.clone(),
            tls_trust_os_roots: self.tls_trust_os_roots,
            tls_direct: self.tls_direct,
        }
    }

    /// Whether this imported profile still needs a secret attached before it can
    /// connect (true for SQL logins fresh out of import).
    pub fn needs_secret(&self) -> bool {
        let ssh_needs = self
            .ssh_tunnel
            .as_ref()
            .is_some_and(|t| matches!(t.target.auth, ExportedSshAuth::Password));
        let transport_ssh_needs = self
            .transport
            .ssh
            .as_ref()
            .is_some_and(|t| matches!(t.target.auth, ExportedSshAuth::Password));
        let proxy_needs = self
            .transport
            .proxy
            .as_ref()
            .is_some_and(|p| p.username.is_some());
        matches!(self.authentication, ExportedAuthentication::SqlLogin { .. })
            || ssh_needs
            || transport_ssh_needs
            || proxy_needs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DatabaseType;
    use crate::connection::{ConnectionEnvironment, SecretProvider, SecretRef};

    fn sql_profile(name: &str, server: &str) -> ConnectionProfile {
        ConnectionProfile::sql_login(
            name,
            server,
            "sa",
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: "super-secret-key".to_string(),
            },
        )
    }

    #[test]
    fn upsert_get_remove_round_trip() {
        let mut store = ProfileStore::new();
        let p = sql_profile("a", "host-a");
        let id = store.upsert(p.clone());
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(id).unwrap().name, "a");

        // Upsert with same id replaces, not duplicates.
        let mut updated = store.get(id).unwrap().clone();
        updated.name = "a2".to_string();
        store.upsert(updated);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get(id).unwrap().name, "a2");

        let removed = store.remove(id).unwrap();
        assert_eq!(removed.name, "a2");
        assert!(store.is_empty());
    }

    #[test]
    fn recent_is_mru_deduped_and_capped() {
        let mut store = ProfileStore::with_recent_limit(2);
        let a = store.upsert(sql_profile("a", "h-a"));
        let b = store.upsert(sql_profile("b", "h-b"));
        let c = store.upsert(sql_profile("c", "h-c"));

        store.mark_used(a);
        store.mark_used(b);
        store.mark_used(a); // a moves back to front, deduped
        store.mark_used(c); // pushes b out (limit 2)

        let recent: Vec<&str> = store.recent().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(recent, vec!["c", "a"]);
    }

    #[test]
    fn recent_skips_removed_profiles() {
        let mut store = ProfileStore::new();
        let a = store.upsert(sql_profile("a", "h-a"));
        let b = store.upsert(sql_profile("b", "h-b"));
        store.mark_used(a);
        store.mark_used(b);
        store.remove(a);
        let recent: Vec<&str> = store.recent().iter().map(|p| p.name.as_str()).collect();
        assert_eq!(recent, vec!["b"]);
    }

    #[test]
    fn mark_used_ignores_unknown_id() {
        let mut store = ProfileStore::new();
        store.mark_used(ConnectionId::new());
        assert!(store.recent().is_empty());
    }

    #[test]
    fn export_contains_no_secret_reference() {
        let mut store = ProfileStore::new();
        store.upsert(sql_profile("a", "h-a"));
        let bundle = store.export();
        let json = serde_json::to_string(&bundle).unwrap();
        // Neither the secret key nor the provider enum may appear.
        assert!(
            !json.contains("super-secret-key"),
            "leaked secret key: {json}"
        );
        assert!(
            !json.contains("MacOsKeychain"),
            "leaked secret provider: {json}"
        );
        assert!(!json.contains("secret_ref"), "leaked secret ref: {json}");
        // Username (non-secret) is retained.
        assert!(json.contains("\"sa\""));
    }

    #[test]
    fn import_marks_provenance_and_requires_secret_reentry() {
        let mut source = ProfileStore::new();
        source.upsert(sql_profile("a", "h-a"));
        let bundle = source.export();
        assert!(bundle.profiles[0].needs_secret());

        let mut target = ProfileStore::new();
        let ids = target.import(&bundle);
        assert_eq!(ids.len(), 1);
        let imported = target.get(ids[0]).unwrap();
        assert_eq!(imported.provenance, ConnectionProvenance::Imported);
        match &imported.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "sa");
                // Placeholder secret ref: empty key forces re-entry.
                assert!(secret_ref.key.is_empty());
            }
            other => panic!("unexpected auth: {other:?}"),
        }
    }

    #[test]
    fn import_assigns_fresh_ids_to_avoid_clobbering() {
        let mut source = ProfileStore::new();
        let original_id = source.upsert(sql_profile("a", "h-a"));
        let bundle = source.export();

        let mut target = ProfileStore::new();
        let existing = target.upsert(sql_profile("existing", "h-x"));
        let ids = target.import(&bundle);
        assert_ne!(ids[0], original_id);
        assert_ne!(ids[0], existing);
        assert_eq!(target.len(), 2);
    }

    #[test]
    fn store_round_trips_through_json() {
        let mut store = ProfileStore::new();
        let a = store.upsert(sql_profile("a", "h-a"));
        store.mark_used(a);
        let json = serde_json::to_string(&store).unwrap();
        let restored: ProfileStore = serde_json::from_str(&json).unwrap();
        assert_eq!(store, restored);
    }

    #[test]
    fn export_maps_cloud_iam_variants() {
        let aws = ConnectionProfile::aws_rds_iam(
            "rds",
            "db.us-west-2.rds.amazonaws.com",
            5432,
            DatabaseType::Postgres,
            "jane",
            "us-west-2",
        );
        let gcp = ConnectionProfile::gcp_cloud_sql_iam(
            "gcp",
            "10.0.0.1",
            5432,
            DatabaseType::Postgres,
            "alice@example.com",
        );
        match export_profile(&aws).authentication {
            ExportedAuthentication::AwsRdsIam {
                username,
                region,
                profile,
            } => {
                assert_eq!(username, "jane");
                assert_eq!(region, "us-west-2");
                assert!(profile.is_none());
            }
            other => panic!("unexpected: {other:?}"),
        }
        match export_profile(&gcp).authentication {
            ExportedAuthentication::GcpCloudSqlIam { username } => {
                assert_eq!(username, "alice@example.com");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(!export_profile(&aws).needs_secret());
        assert!(!export_profile(&gcp).needs_secret());
    }

    #[test]
    fn entra_profile_exports_without_username_or_secret() {
        let mut store = ProfileStore::new();
        store.upsert(ConnectionProfile::azure_sql_entra_browser(
            "prod",
            "prod.database.windows.net",
            "sales",
        ));
        let bundle = store.export();
        match &bundle.profiles[0].authentication {
            ExportedAuthentication::EntraBrowser => {}
            other => panic!("unexpected: {other:?}"),
        }
        assert!(!bundle.profiles[0].needs_secret());
        let _ = ConnectionEnvironment::Production;
    }

    #[test]
    fn export_bundle_carries_entity_graph_and_database_type() {
        let mut profile = sql_profile("pg", "db.example.com");
        profile.database_type = DatabaseType::Postgres;
        profile.client = Some("Acme".to_string());
        profile.project = Some("Billing".to_string());
        profile.favorite = true;
        let bundle = ProfileExportBundle {
            version: PROFILE_EXPORT_VERSION,
            profiles: vec![export_profile(&profile)],
            clients: vec![ExportedClientEntity {
                name: "Acme".to_string(),
                color_hex: Some("#112233".to_string()),
                image_path: None,
                safety: ExportedSafetyFeatures {
                    only_allow_select: true,
                    ..ExportedSafetyFeatures::default()
                },
            }],
            projects: vec![ExportedProjectEntity {
                name: "Billing".to_string(),
                color_hex: None,
                image_path: None,
                safety: ExportedSafetyFeatures::default(),
            }],
            environments: vec![ExportedEnvironmentEntity {
                name: "Production".to_string(),
                color_hex: None,
                image_path: None,
                safety: ExportedSafetyFeatures::default(),
            }],
            sql_auth_accounts: vec![ExportedSqlAuthAccount {
                name: "Shared login".to_string(),
                username: "dba".to_string(),
            }],
        };
        let json = serde_json::to_string(&bundle).unwrap();
        assert!(json.contains("\"Postgres\""));
        assert!(json.contains("\"Acme\""));
        assert!(!json.contains("password"));
        assert!(!json.contains("secret_ref"));
        let restored: ProfileExportBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(restored, bundle);
    }

    #[test]
    fn startup_sql_round_trips_through_export_json() {
        let mut profile = sql_profile("a", "h-a");
        profile.startup_sql = Some("SET NOCOUNT ON;\nGO\nSET ANSI_NULLS ON;".to_string());
        let mut source = ProfileStore::new();
        source.upsert(profile);
        let bundle = source.export();
        let json = serde_json::to_string(&bundle).unwrap();
        assert!(json.contains("SET NOCOUNT ON;"));
        assert!(!json.contains("password"));
        let restored: ProfileExportBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.profiles[0].startup_sql.as_deref(),
            Some("SET NOCOUNT ON;\nGO\nSET ANSI_NULLS ON;")
        );
        let mut target = ProfileStore::new();
        let ids = target.import(&restored);
        assert_eq!(
            target.get(ids[0]).unwrap().startup_sql.as_deref(),
            Some("SET NOCOUNT ON;\nGO\nSET ANSI_NULLS ON;")
        );
    }

    #[test]
    fn notes_round_trip_through_export_json() {
        let mut profile = sql_profile("a", "h-a");
        profile.notes = Some("Shared with QA.\nAsk #dba before writes.".to_string());
        let mut source = ProfileStore::new();
        source.upsert(profile);
        let json = serde_json::to_string(&source.export()).unwrap();
        let restored: ProfileExportBundle = serde_json::from_str(&json).unwrap();
        assert_eq!(
            restored.profiles[0].notes.as_deref(),
            Some("Shared with QA.\nAsk #dba before writes.")
        );
        let mut target = ProfileStore::new();
        let ids = target.import(&restored);
        assert_eq!(
            target.get(ids[0]).unwrap().notes.as_deref(),
            Some("Shared with QA.\nAsk #dba before writes.")
        );
    }

    #[test]
    fn legacy_profile_only_bundle_still_imports() {
        let json = r#"{"version":1,"profiles":[{"name":"a","server":"h-a","port":1433,"database":null,"environment":"LOCAL","client":null,"project":null,"authentication":{"sql-login":{"username":"sa"}},"encrypt":true,"trust_server_certificate":false,"timeout_seconds":30,"application_name":"SQLLY","auto_wrap_rollback":false,"block_all_but_select":false}]}"#;
        let bundle: ProfileExportBundle = serde_json::from_str(json).unwrap();
        assert!(bundle.clients.is_empty());
        let mut target = ProfileStore::new();
        let ids = target.import(&bundle);
        assert_eq!(ids.len(), 1);
        assert_eq!(
            target.get(ids[0]).unwrap().database_type,
            DatabaseType::SqlServer
        );
    }

    #[test]
    fn export_tunnel_profile_is_secret_free() {
        use crate::connection::{SshAuth, SshHop, SshTunnelConfig};
        let mut profile = sql_profile("tun", "db.internal");
        profile.ssh_tunnel = Some(SshTunnelConfig {
            target: SshHop {
                host: "bastion.corp".to_string(),
                port: 22,
                username: "jump".to_string(),
                auth: SshAuth::Password {
                    secret_ref: SecretRef {
                        provider: SecretProvider::MacOsKeychain,
                        key: "ssh-secret-id".to_string(),
                    },
                },
            },
            jump: None,
            keepalive: Default::default(),
        });
        let exported = export_profile(&profile);
        let json = serde_json::to_string(&exported).unwrap();
        assert!(json.contains("bastion.corp"));
        assert!(json.contains("jump"));
        assert!(!json.contains("secret_ref"));
        assert!(!json.contains("ssh-secret-id"));
    }

    #[test]
    fn import_tunnel_export_needs_ssh_password() {
        use crate::connection::{SshAuth, SshHop, SshTunnelConfig};
        let mut profile = sql_profile("tun", "db.internal");
        profile.authentication = AuthenticationType::Integrated;
        profile.ssh_tunnel = Some(SshTunnelConfig {
            target: SshHop {
                host: "bastion".to_string(),
                port: 22,
                username: "u".to_string(),
                auth: SshAuth::Password {
                    secret_ref: SecretRef {
                        provider: SecretProvider::MacOsKeychain,
                        key: "k".to_string(),
                    },
                },
            },
            jump: None,
            keepalive: Default::default(),
        });
        let exported = export_profile(&profile);
        assert!(exported.needs_secret());
    }

    #[test]
    fn export_strips_proxy_password_and_marks_needs_secret() {
        use crate::connection::{ProxyHop, ProxyKind, TransportChain};
        let mut profile = sql_profile("proxy", "db.internal");
        profile.transport = TransportChain {
            proxy: Some(ProxyHop {
                kind: ProxyKind::HttpConnect,
                host: "proxy.corp".to_string(),
                port: 8080,
                username: Some("user".to_string()),
                password: Some(SecretRef {
                    provider: SecretProvider::MacOsKeychain,
                    key: "proxy-key".to_string(),
                }),
            }),
            ssh: None,
            kube: None,
            ssm: None,
        };
        let exported = export_profile(&profile);
        let json = serde_json::to_string(&exported).unwrap();
        assert!(json.contains("proxy.corp"));
        assert!(!json.contains("proxy-key"));
        assert!(!json.contains("secret_ref"));
        assert!(exported.needs_secret());
    }

    #[test]
    fn export_round_trips_kube_port_forward() {
        use crate::connection::{KubePortForward, KubeTargetKind, TransportChain};
        let mut profile = sql_profile("kube", "ignored.internal");
        profile.transport = TransportChain::kube_only(KubePortForward {
            kubeconfig: Some("/home/me/.kube/config".to_string()),
            context: Some("dev".to_string()),
            namespace: "data".to_string(),
            target_kind: KubeTargetKind::Service,
            target_name: "postgres".to_string(),
            remote_port: 5432,
        });
        let exported = export_profile(&profile);
        let restored = exported.into_profile();
        assert_eq!(restored.transport.kube, profile.transport.kube);
    }

    #[test]
    fn export_round_trips_ssm_port_forward() {
        use crate::connection::{SsmPortForward, TransportChain};
        let mut profile = sql_profile("ssm", "db.internal");
        profile.transport = TransportChain::ssm_only(SsmPortForward {
            target: "i-0123456789abcdef0".to_string(),
            region: Some("eu-west-1".to_string()),
            profile: Some("ops".to_string()),
            remote_host: Some("10.1.2.3".to_string()),
        });
        let exported = export_profile(&profile);
        let json = serde_json::to_string(&exported).unwrap();
        let restored: ExportedProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.into_profile().transport.ssm, profile.transport.ssm);
    }

    #[test]
    fn export_round_trips_direct_tls_and_older_profiles_stay_off() {
        let mut profile = sql_profile("pg", "pg.internal");
        profile.tls_direct = true;
        let restored = export_profile(&profile).into_profile();
        assert!(restored.tls_direct);
        profile.tls_direct = false;
        let json = serde_json::to_string(&export_profile(&profile)).unwrap();
        assert!(!json.contains("tls_direct"), "{json}");
        let saved = serde_json::to_value(&profile).unwrap();
        assert!(saved.get("tls_direct").is_none());
        let old: crate::connection::ConnectionProfile = serde_json::from_value(saved).unwrap();
        assert!(!old.tls_direct);
    }

    #[test]
    fn export_round_trips_trusting_the_os_roots_and_older_profiles_stay_off() {
        let mut profile = sql_profile("cache", "cache.internal");
        profile.tls_trust_os_roots = true;
        let restored = export_profile(&profile).into_profile();
        assert!(restored.tls_trust_os_roots);
        profile.tls_trust_os_roots = false;
        let json = serde_json::to_string(&export_profile(&profile)).unwrap();
        assert!(!json.contains("tls_trust_os_roots"), "{json}");
        let saved = serde_json::to_value(&profile).unwrap();
        assert!(saved.get("tls_trust_os_roots").is_none());
        let old: crate::connection::ConnectionProfile = serde_json::from_value(saved).unwrap();
        assert!(!old.tls_trust_os_roots);
    }

    #[test]
    fn export_round_trips_the_ssh_keepalive_and_omits_the_default() {
        use crate::connection::{SshAuth, SshHop, SshKeepalive, SshTunnelConfig, TransportChain};
        let mut profile = sql_profile("tunneled", "db.internal");
        let tunnel = |keepalive| SshTunnelConfig {
            target: SshHop {
                host: "bastion".to_string(),
                port: 22,
                username: "me".to_string(),
                auth: SshAuth::Agent,
            },
            jump: None,
            keepalive,
        };
        profile.transport = TransportChain {
            proxy: None,
            ssh: Some(tunnel(SshKeepalive {
                interval_seconds: Some(10),
                max_missed: Some(6),
            })),
            kube: None,
            ssm: None,
        };
        let restored = export_profile(&profile).into_profile();
        assert_eq!(restored.transport.ssh, profile.transport.ssh);
        profile.transport.ssh = Some(tunnel(SshKeepalive::default()));
        let json = serde_json::to_string(&export_profile(&profile)).unwrap();
        assert!(!json.contains("keepalive"), "{json}");
        // A profile saved before keepalives existed decodes with the defaults.
        let old: SshTunnelConfig = serde_json::from_str(
            r#"{"target":{"host":"b","port":22,"username":"u","auth":"Agent"}}"#,
        )
        .unwrap();
        assert!(old.keepalive.is_default());
        assert_eq!(
            old.keepalive.interval(),
            Some(std::time::Duration::from_secs(30))
        );
    }
}
