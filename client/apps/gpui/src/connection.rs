use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use sqlly_core::{
    AuthenticationType, ConnectionId, ConnectionProfile, ExportedClientEntity,
    ExportedEnvironmentEntity, ExportedProjectEntity, ExportedSafetyFeatures,
    ExportedSqlAuthAccount, ProfileExportBundle, SecretProvider, SecretRef, TransportChain,
    TunnelProfile, TunnelProfileId,
};
#[cfg(test)]
use sqlly_core::{PROFILE_EXPORT_VERSION, export_profile};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use uuid::Uuid;

use crate::entra_grouping::ServerGroupingMode;

/// `connections.json` is written at the shared config version; see
/// [`crate::config_version`].
const STORE_VERSION: u32 = crate::config_version::CONFIG_VERSION;

/// Query-protection options attached to a client / project / environment entity.
/// Port of `SafetyFeatures` in `ConnectionModel.swift` (8 fields, all default
/// `false`). The lenient decoder mirrors Swift's `decodeIfPresent ?? false`:
/// every field is `#[serde(default)]` so partial / legacy JSON still loads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SafetyFeatures {
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

/// A configurable client. Mirrors `ClientEntity` in `ConnectionModel.swift`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientEntity {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: SafetyFeatures,
}

/// A configurable project. Mirrors `ProjectEntity` in `ConnectionModel.swift`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectEntity {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: SafetyFeatures,
}

/// A configurable environment. Mirrors `EnvironmentEntity` in
/// `ConnectionModel.swift`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnvironmentEntity {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub color_hex: Option<String>,
    #[serde(default)]
    pub image_path: Option<String>,
    #[serde(default)]
    pub safety: SafetyFeatures,
}

/// Non-secret metadata for a reusable SQL authentication account. Passwords
/// are stored separately in the OS keychain under this stable id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqlAuthAccount {
    pub id: Uuid,
    pub name: String,
    pub username: String,
}

/// Fixed-UUID built-in environments seeded into every fresh store, mirroring
/// `ConnectionStoreData.builtInEnvironments` in `ConnectionModel.swift`
/// (`...0001` = Dev, `...0002` = Test, `...0003` = Staging, `...0004` =
/// Production).
pub fn built_in_environments() -> Vec<EnvironmentEntity> {
    let mk = |suffix: u8, name: &str, production: bool| EnvironmentEntity {
        id: Uuid::from_bytes([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, suffix]),
        name: name.to_string(),
        color_hex: None,
        image_path: None,
        safety: SafetyFeatures {
            is_production: production,
            ..Default::default()
        },
    };
    vec![
        mk(1, "Dev", false),
        mk(2, "Test", false),
        mk(3, "Staging", false),
        mk(4, "Production", true),
    ]
}

/// Fixed identifier for the browser preview's bundled Northwind sample
/// connection, so re-seeding stays idempotent across tab reloads.
#[cfg(target_arch = "wasm32")]
pub const BROWSER_SAMPLE_PROFILE_ID: Uuid = Uuid::from_bytes([
    0x5B, 0xAA, 0x11, 0xE5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x01,
]);

/// Ensure the browser preview's sample connection exists in the store and
/// return it. The browser query and metadata services ignore a profile's
/// server and credentials — a query runs against whatever database is open in
/// the tab — so this profile is only the explorer / query-bar handle the user
/// sees and connects to. Idempotent: seeded once, reused on later reloads.
#[cfg(target_arch = "wasm32")]
pub fn ensure_browser_sample_profile() -> ConnectionProfile {
    let store = ConnectionStore::global();
    if let Some(existing) = store.get(BROWSER_SAMPLE_PROFILE_ID) {
        return existing;
    }
    let mut profile = ConnectionProfile::sql_login(
        crate::localization::text("connections.store.browser_northwind_sample_name"),
        crate::localization::text("connections.store.browser_northwind_sample_server"),
        "preview",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.id = ConnectionId(BROWSER_SAMPLE_PROFILE_ID);
    profile.database = Some("main".to_string());
    store.add(profile.clone());
    profile
}

/// Fixed identifier for the browser's PGlite Chinook sample connection, so
/// re-seeding stays idempotent across tab reloads.
#[cfg(target_arch = "wasm32")]
pub const BROWSER_PGLITE_PROFILE_ID: Uuid = Uuid::from_bytes([
    0x5B, 0xAA, 0x11, 0xE5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x02,
]);

/// Ensure the PGlite Chinook sample connection exists in the store and return
/// it. Unlike the SQLite sample, this profile's engine type matters: it is
/// marked Postgres, which routes its queries and metadata to the in-tab
/// PGlite instance (a real Postgres with Chinook loaded by the host page) and
/// selects the Postgres dialect for IntelliSense. Server and credentials are
/// still cosmetic. Idempotent: seeded once, reused on later reloads.
#[cfg(target_arch = "wasm32")]
pub fn ensure_browser_pglite_profile() -> ConnectionProfile {
    use sqlly_core::DatabaseType;

    let store = ConnectionStore::global();
    if let Some(existing) = store.get(BROWSER_PGLITE_PROFILE_ID) {
        return existing;
    }
    let mut profile = ConnectionProfile::sql_login(
        crate::localization::text("connections.store.browser_chinook_sample_name"),
        crate::localization::text("connections.store.browser_chinook_sample_server"),
        "preview",
        SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        },
    );
    profile.id = ConnectionId(BROWSER_PGLITE_PROFILE_ID);
    profile.database_type = DatabaseType::Postgres;
    profile.port = 5432;
    profile.database = Some("postgres".to_string());
    store.add(profile.clone());
    profile
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionStoreData {
    pub version: u32,
    pub profiles: Vec<ConnectionProfile>,
    #[serde(default)]
    pub clients: Vec<ClientEntity>,
    #[serde(default)]
    pub projects: Vec<ProjectEntity>,
    #[serde(default)]
    pub environments: Vec<EnvironmentEntity>,
    #[serde(default)]
    pub sql_auth_accounts: Vec<SqlAuthAccount>,
    #[serde(default)]
    pub server_grouping: ServerGroupingMode,
    #[serde(default)]
    pub tunnel_profiles: Vec<TunnelProfile>,
}

impl Default for ConnectionStoreData {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            profiles: Vec::new(),
            clients: Vec::new(),
            projects: Vec::new(),
            environments: built_in_environments(),
            sql_auth_accounts: Vec::new(),
            server_grouping: ServerGroupingMode::default(),
            tunnel_profiles: Vec::new(),
        }
    }
}

pub struct ConnectionStore {
    inner: RwLock<ConnectionStoreData>,
    path: PathBuf,
    /// When set, the on-disk file was corrupt on load and must not be
    /// overwritten; holds the reason so startup can tell the user.
    load_error: Option<String>,
    /// When set, the connections and labels were handed over by a relay
    /// proxy for this session: they are read-only, live only in memory, and
    /// are never persisted (the proxy re-sends them on the next load).
    proxied: AtomicBool,
    /// Connections added for this session only (a connection URL opened
    /// from the command line, Gridex #18): listed like any other, never
    /// written to the saved connections file.
    unsaved: RwLock<std::collections::HashSet<Uuid>>,
}

/// Whether this session's connections come from a relay proxy (see
/// [`ConnectionStore::replace_with_proxied`]). Every add/edit/delete/import
/// affordance consults this and disables itself while it is true.
pub fn connections_are_proxied() -> bool {
    ConnectionStore::global().is_proxied()
}

/// Whether a profile came from a provisioning file or environment variable
/// (and is therefore never saved and read-only in the editors).
pub fn is_provisioned(profile: &ConnectionProfile) -> bool {
    profile.provenance == sqlly_core::ConnectionProvenance::Provisioned
}

/// Whether the stored connection `id` is provisioned.
pub fn connection_is_provisioned(id: Uuid) -> bool {
    ConnectionStore::global()
        .get(id)
        .is_some_and(|profile| is_provisioned(&profile))
}

static PROCESS_STORE: OnceLock<ConnectionStore> = OnceLock::new();

impl ConnectionStore {
    /// Process-wide store — loads `connections.json` once (including password
    /// migration). All UI mutations share this instance.
    pub fn global() -> &'static ConnectionStore {
        PROCESS_STORE.get_or_init(Self::load_default)
    }

    fn load_default() -> Self {
        let path = Self::default_path();
        let store = Self::from_loaded(path.clone(), Self::load(&path));
        #[cfg(not(target_arch = "wasm32"))]
        {
            let provisioning = sqlly_core::provisioning::load_provisioned_connections(
                path.parent()
                    .map(|dir| dir.join(sqlly_core::provisioning::PROVISIONED_FILE_NAME)),
            );
            for warning in &provisioning.warnings {
                eprintln!("warning: provisioned connections: {warning}");
            }
            store.add_provisioned(provisioning.profiles());
        }
        store
    }

    /// Add connections from a provisioning file or `SQLLY_CONNECTION_*`
    /// variables (see [`sqlly_core::provisioning`]). They are listed like
    /// saved connections but never persisted; a saved profile with the same
    /// id wins.
    pub fn add_provisioned(&self, profiles: Vec<ConnectionProfile>) {
        let mut guard = self.inner.write();
        for profile in profiles {
            if !guard.profiles.iter().any(|p| p.id == profile.id) {
                guard.profiles.push(profile);
            }
        }
    }

    #[allow(dead_code)]
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        Self::from_loaded(path.clone(), Self::load(&path))
    }

    fn from_loaded(path: PathBuf, loaded: Result<Option<ConnectionStoreData>, String>) -> Self {
        let (mut data, load_error) = match loaded {
            Ok(None) => (ConnectionStoreData::default(), None),
            Ok(Some(data)) => (data, None),
            Err(error) => (ConnectionStoreData::default(), Some(error)),
        };
        Self::ensure_built_in_environments(&mut data);
        if load_error.is_none() {
            Self::migrate_in_memory_sql_passwords(&path, &mut data);
            Self::migrate_version(&path, &mut data);
        }
        Self {
            inner: RwLock::new(data),
            path,
            load_error,
            proxied: AtomicBool::new(false),
            unsaved: RwLock::new(std::collections::HashSet::new()),
        }
    }

    /// Why the saved connections file could not be loaded, when it exists but
    /// is unreadable. Saving stays blocked for the session while this is set.
    pub fn load_error(&self) -> Option<&str> {
        self.load_error.as_deref()
    }

    fn persist_blocked(&self) -> bool {
        self.load_error.is_some()
    }

    /// Whether the store holds a relay proxy's read-only connection set.
    pub fn is_proxied(&self) -> bool {
        self.proxied.load(Ordering::SeqCst)
    }

    /// Replace every connection and label with the relay proxy's listing and
    /// lock the store: from here on nothing is persisted and every mutator
    /// is a no-op, so the browser can never drift from what the proxy serves.
    /// Health/status notes on the proxied profiles still update in memory.
    /// Only the browser build has a proxy session to feed it; the desktop
    /// build compiles it for its unit test.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub fn replace_with_proxied(&self, catalog: &sqlly_core::credential_proxy::ProxyCatalog) {
        fn safety(label: &sqlly_core::credential_proxy::ProxyLabel) -> SafetyFeatures {
            let mut exported = ExportedSafetyFeatures::default();
            label.safety.apply_to(&mut exported);
            import_safety_features(&exported)
        }
        let mut data = ConnectionStoreData {
            version: self.inner.read().version,
            profiles: catalog.profiles.clone(),
            clients: catalog
                .clients
                .iter()
                .map(|l| ClientEntity {
                    id: Uuid::new_v4(),
                    name: l.name.clone(),
                    color_hex: l.color_hex.clone(),
                    image_path: None,
                    safety: safety(l),
                })
                .collect(),
            projects: catalog
                .projects
                .iter()
                .map(|l| ProjectEntity {
                    id: Uuid::new_v4(),
                    name: l.name.clone(),
                    color_hex: l.color_hex.clone(),
                    image_path: None,
                    safety: safety(l),
                })
                .collect(),
            environments: catalog
                .environments
                .iter()
                .map(|l| EnvironmentEntity {
                    id: Uuid::new_v4(),
                    name: l.name.clone(),
                    color_hex: l.color_hex.clone(),
                    image_path: None,
                    safety: safety(l),
                })
                .collect(),
            sql_auth_accounts: Vec::new(),
            server_grouping: self.inner.read().server_grouping,
            tunnel_profiles: Vec::new(),
        };
        // The built-in environments always exist (Dev/Test/Staging/Production
        // are what the profile `environment` field names); the file's entries
        // override a built-in of the same name and add the rest.
        let mut environments = built_in_environments();
        for env in data.environments.drain(..) {
            match environments
                .iter_mut()
                .find(|e| e.name.eq_ignore_ascii_case(&env.name))
            {
                Some(existing) => {
                    existing.color_hex = env.color_hex.or(existing.color_hex.take());
                    existing.safety = env.safety;
                }
                None => environments.push(env),
            }
        }
        data.environments = environments;
        self.proxied.store(true, Ordering::SeqCst);
        *self.inner.write() = data;
    }

    /// Seeds the four fixed-UUID built-in environments when a loaded store has
    /// none (e.g. a pre-entity-management `connections.json` that predates the
    /// `environments` field). Mirrors Swift's `ConnectionStoreData.empty`
    /// factory, applied as a post-load normalization so upgrades always see the
    /// built-ins without forcing a rewrite on every launch.
    fn ensure_built_in_environments(data: &mut ConnectionStoreData) {
        if data.environments.is_empty() {
            data.environments = built_in_environments();
        }
    }

    /// Bring a store written by an older version up to [`STORE_VERSION`] and
    /// rewrite it. Profiles carry no tenant-scope settings (those live in
    /// `tenant-scoping.json`, reset by its own migration), and any stray keys
    /// an older or hand-edited file held were already dropped by the typed
    /// load, so the rewrite at the current version is the whole migration.
    fn migrate_version(path: &Path, data: &mut ConnectionStoreData) {
        if data.version >= STORE_VERSION {
            return;
        }
        data.version = STORE_VERSION;
        if let Err(error) = Self::save_to(path, data) {
            eprintln!("warning: failed to save connection store after version migration: {error}");
        }
    }

    /// Lifts legacy cleartext SQL Login passwords from `connections.json` into
    /// the platform secret store and rewrites the profile to a keychain
    /// account reference.
    fn migrate_in_memory_sql_passwords(path: &Path, data: &mut ConnectionStoreData) {
        let mut migrated = false;
        for profile in &mut data.profiles {
            // A libSQL auth token moves to the keyring the same way, but
            // owns no reusable SQL account (it has no user name).
            let username = match &profile.authentication {
                AuthenticationType::SqlLogin { username, .. } => Some(username.clone()),
                AuthenticationType::AuthToken { .. } => None,
                _ => continue,
            };
            let name = profile.name.clone();
            let Some(secret_ref) = profile.authentication.secret_ref_mut() else {
                continue;
            };
            if secret_ref.provider != SecretProvider::InMemory || secret_ref.key.is_empty() {
                continue;
            }
            let password = secret_ref.key.clone();
            match crate::sql_credentials::migrate_legacy_password(&password) {
                Ok(account_id) => {
                    secret_ref.provider = SecretProvider::MacOsKeychain;
                    secret_ref.key = account_id.to_string();
                    if let Some(username) = username {
                        let account = SqlAuthAccount {
                            id: account_id,
                            name,
                            username,
                        };
                        if !data
                            .sql_auth_accounts
                            .iter()
                            .any(|existing| existing.id == account_id)
                        {
                            data.sql_auth_accounts.push(account);
                        }
                    }
                    migrated = true;
                }
                Err(error) => {
                    eprintln!(
                        "warning: failed to migrate SQL password to keychain for \
                         connection \"{}\": {error}",
                        profile.name
                    );
                }
            }
        }
        if migrated && let Err(error) = Self::save_to(path, data) {
            eprintln!("warning: failed to save connection store after password migration: {error}");
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn default_path() -> PathBuf {
        if let Some(support) = dirs::config_dir() {
            support.join("SQLLY").join("connections.json")
        } else {
            PathBuf::from("connections.json")
        }
    }

    /// Browser build: nominal path only — load/save use localStorage.
    #[cfg(target_arch = "wasm32")]
    fn default_path() -> PathBuf {
        PathBuf::from("connections.json")
    }

    /// Missing file → `Ok(None)`; valid JSON → `Ok(Some)`; unreadable or invalid → `Err`.
    fn load(path: &Path) -> Result<Option<ConnectionStoreData>, String> {
        fn invalid_json(error: serde_json::Error) -> String {
            crate::localization::format(
                "connections.store.invalid_json",
                &[("error", &error.to_string())],
            )
        }
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = path;
            match crate::host_storage::load_text(
                crate::host_storage::StorageArea::Config,
                "connections.json",
            ) {
                None => Ok(None),
                Some(text) => serde_json::from_str(&text).map(Some).map_err(invalid_json),
            }
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => {
                    return Err(crate::localization::format(
                        "connections.store.read_failed",
                        &[("error", &error.to_string())],
                    ));
                }
            };
            serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(invalid_json)
        }
    }

    fn save_to(path: &Path, data: &ConnectionStoreData) -> Result<(), String> {
        #[cfg(target_arch = "wasm32")]
        return {
            let _ = path;
            let text = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
            crate::host_storage::store_text(
                crate::host_storage::StorageArea::Config,
                "connections.json",
                &text,
            )
        };
        #[cfg(not(target_arch = "wasm32"))]
        {
            let bytes = serde_json::to_vec_pretty(data).map_err(|e| e.to_string())?;
            crate::fs_safe::write_atomic(path, &bytes).map_err(|e| e.to_string())
        }
    }

    fn persist(&self) {
        if self.persist_blocked() || self.is_proxied() {
            return;
        }
        let mut data = self.inner.read().clone();
        // Provisioned connections are rebuilt from their file / environment
        // on every launch and never written to the user's saved store, and
        // session-only connections (whose password may sit in memory) are
        // never written at all.
        let unsaved = self.unsaved.read();
        data.profiles
            .retain(|p| !is_provisioned(p) && !unsaved.contains(&p.id.0));
        drop(unsaved);
        if let Err(e) = Self::save_to(&self.path, &data) {
            eprintln!("warning: failed to save connection store: {e}");
        }
    }

    /// Everything iCloud sync may read: the saved connections and labels,
    /// without provisioned profiles (they are never saved). `None` while the
    /// store must not be written (its file was unreadable at launch, or a
    /// relay proxy owns it this session), so sync leaves it alone entirely.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn sync_snapshot(&self) -> Option<ConnectionStoreData> {
        if self.persist_blocked() || self.is_proxied() {
            return None;
        }
        let mut data = self.inner.read().clone();
        // Session-only connections (opened from a URL) never leave this Mac.
        let unsaved = self.unsaved.read();
        data.profiles
            .retain(|p| !is_provisioned(p) && !unsaved.contains(&p.id.0));
        drop(unsaved);
        Some(data)
    }

    /// Applies iCloud sync's changes in one write and one save. Returns
    /// false (changing nothing) while the store must not be written.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn apply_sync(&self, change: impl FnOnce(&mut ConnectionStoreData)) -> bool {
        if self.persist_blocked() || self.is_proxied() {
            return false;
        }
        change(&mut self.inner.write());
        self.persist();
        true
    }

    pub fn list(&self) -> Vec<ConnectionProfile> {
        self.inner.read().profiles.clone()
    }

    pub fn get(&self, id: Uuid) -> Option<ConnectionProfile> {
        self.inner
            .read()
            .profiles
            .iter()
            .find(|p| p.id.0 == id)
            .cloned()
    }

    pub fn add(&self, mut profile: ConnectionProfile) {
        if self.is_proxied() {
            return;
        }
        if profile.id.0 == Uuid::nil() {
            profile.id = ConnectionId::new();
        }
        self.inner.write().profiles.push(profile);
        self.persist();
    }

    /// Add `profile` for this session only: it is listed and connectable like
    /// any other connection but never written to the saved connections file,
    /// so a password it holds in memory never reaches disk.
    pub fn add_unsaved(&self, mut profile: ConnectionProfile) {
        if self.is_proxied() {
            return;
        }
        if profile.id.0 == Uuid::nil() {
            profile.id = ConnectionId::new();
        }
        self.unsaved.write().insert(profile.id.0);
        self.inner.write().profiles.push(profile);
    }

    /// Whether `id` was added with [`Self::add_unsaved`].
    #[cfg(test)]
    pub fn is_unsaved(&self, id: Uuid) -> bool {
        self.unsaved.read().contains(&id)
    }

    /// Keep a session-only connection: the user saved it in the editor, so
    /// it now persists like any other (its password has gone to the keyring
    /// through the editor, not through this store).
    /// A profile still holding an in-memory secret (the URL's password)
    /// stays session-only, so that secret never reaches the file.
    pub fn keep_unsaved(&self, id: Uuid) {
        let holds_inline_secret = self.get(id).is_some_and(|p| {
            p.authentication
                .secret_ref()
                .is_some_and(|r| r.provider == SecretProvider::InMemory && !r.key.is_empty())
        });
        if !holds_inline_secret && self.unsaved.write().remove(&id) {
            self.persist();
        }
    }

    /// Insert `profile` directly after the profile `anchor`, so a duplicate
    /// lands beside its original in every list that keeps store order.
    /// Falls back to appending when `anchor` is no longer in the store.
    pub fn add_after(&self, anchor: Uuid, mut profile: ConnectionProfile) {
        if self.is_proxied() {
            return;
        }
        if profile.id.0 == Uuid::nil() {
            profile.id = ConnectionId::new();
        }
        {
            let mut guard = self.inner.write();
            match guard.profiles.iter().position(|p| p.id.0 == anchor) {
                Some(index) => guard.profiles.insert(index + 1, profile),
                None => guard.profiles.push(profile),
            }
        }
        self.persist();
    }

    pub fn update(&self, id: Uuid, profile: ConnectionProfile) {
        if self.is_proxied() {
            return;
        }
        let mut guard = self.inner.write();
        if let Some(existing) = guard.profiles.iter_mut().find(|p| p.id.0 == id) {
            // A provisioned connection stays provisioned (and so unsaved):
            // in-session tweaks such as masking apply until the app quits.
            let provisioned = is_provisioned(existing);
            *existing = profile;
            if provisioned {
                existing.provenance = sqlly_core::ConnectionProvenance::Provisioned;
            }
        }
        drop(guard);
        self.persist();
    }

    /// Persist a successful connection test without rewriting any connection
    /// fields (and, in particular, without ever hydrating a secret into the
    /// JSON-backed profile).
    pub fn record_test_success(&self, id: Uuid, now: DateTime<Utc>) -> bool {
        self.update_status(id, |status| status.record_test_success(now))
    }

    /// Persist a successful live connection. The core status object treats a
    /// real connection as stronger evidence than a standalone test.
    pub fn record_connected(&self, id: Uuid, now: DateTime<Utc>) -> bool {
        self.update_status(id, |status| status.record_connected(now))
    }

    /// Persist a bounded, secret-redacted failure summary for a saved profile.
    pub fn record_failure(&self, id: Uuid, now: DateTime<Utc>, summary: impl Into<String>) -> bool {
        let summary = summary.into();
        self.update_status(id, |status| status.record_failure(now, summary))
    }

    /// [`Self::record_failure`] for a presentation message: keyed messages
    /// keep their key and arguments so health views render them in the
    /// active language. Safe off the UI thread.
    pub fn record_failure_message(
        &self,
        id: Uuid,
        now: DateTime<Utc>,
        message: &sqlly_core::DisplayMessage,
    ) -> bool {
        self.update_status(id, |status| status.record_failure_message(now, message))
    }

    /// Persist the favorite star without rewriting any other profile field.
    pub fn set_favorite(&self, id: Uuid, favorite: bool) -> bool {
        let mut guard = self.inner.write();
        let Some(profile) = guard.profiles.iter_mut().find(|profile| profile.id.0 == id) else {
            return false;
        };
        profile.favorite = favorite;
        drop(guard);
        self.persist();
        true
    }

    /// Persist the flavor an Auto-detect Redis profile's server announced.
    /// Only touches profiles that still have Auto-detect on, and only
    /// `database_type`; returns whether anything changed.
    pub fn set_detected_engine(&self, id: Uuid, engine: sqlly_core::DatabaseType) -> bool {
        if self.is_proxied() {
            return false;
        }
        let mut guard = self.inner.write();
        let Some(profile) = guard.profiles.iter_mut().find(|profile| profile.id.0 == id) else {
            return false;
        };
        if !profile.engine_auto_detect || profile.database_type == engine {
            return false;
        }
        profile.database_type = engine;
        drop(guard);
        self.persist();
        true
    }

    fn update_status(
        &self,
        id: Uuid,
        update: impl FnOnce(&mut sqlly_core::ConnectionStatus),
    ) -> bool {
        let mut guard = self.inner.write();
        let Some(profile) = guard.profiles.iter_mut().find(|profile| profile.id.0 == id) else {
            return false;
        };
        update(&mut profile.status);
        drop(guard);
        self.persist();
        true
    }

    pub fn remove(&self, id: Uuid) {
        if self.is_proxied() {
            return;
        }
        let mut guard = self.inner.write();
        guard.profiles.retain(|p| p.id.0 != id);
        drop(guard);
        self.persist();
    }

    // ---- Entity management (clients / projects / environments) ----

    pub fn clients(&self) -> Vec<ClientEntity> {
        self.inner.read().clients.clone()
    }

    pub fn projects(&self) -> Vec<ProjectEntity> {
        self.inner.read().projects.clone()
    }

    pub fn environments(&self) -> Vec<EnvironmentEntity> {
        self.inner.read().environments.clone()
    }

    pub fn sql_auth_accounts(&self) -> Vec<SqlAuthAccount> {
        self.inner.read().sql_auth_accounts.clone()
    }

    pub fn upsert_sql_auth_account(&self, account: SqlAuthAccount) {
        let mut guard = self.inner.write();
        if let Some(existing) = guard
            .sql_auth_accounts
            .iter_mut()
            .find(|existing| existing.id == account.id)
        {
            *existing = account;
        } else {
            guard.sql_auth_accounts.push(account);
        }
        drop(guard);
        self.persist();
    }

    pub fn remove_sql_auth_account(&self, id: Uuid) {
        self.inner
            .write()
            .sql_auth_accounts
            .retain(|account| account.id != id);
        self.persist();
    }

    pub fn server_grouping(&self) -> ServerGroupingMode {
        self.inner.read().server_grouping
    }

    pub fn set_server_grouping(&self, mode: ServerGroupingMode) {
        self.inner.write().server_grouping = mode;
        self.persist();
    }

    /// Upserts a client entity by id (replace if present, else append).
    pub fn upsert_client(&self, entity: ClientEntity) {
        if self.is_proxied() {
            return;
        }
        let mut guard = self.inner.write();
        upsert_by_id(&mut guard.clients, entity);
        drop(guard);
        self.persist();
    }

    pub fn remove_client(&self, id: Uuid) {
        if self.is_proxied() {
            return;
        }
        self.inner.write().clients.retain(|e| e.id != id);
        self.persist();
    }

    pub fn upsert_project(&self, entity: ProjectEntity) {
        if self.is_proxied() {
            return;
        }
        let mut guard = self.inner.write();
        upsert_by_id(&mut guard.projects, entity);
        drop(guard);
        self.persist();
    }

    pub fn remove_project(&self, id: Uuid) {
        if self.is_proxied() {
            return;
        }
        self.inner.write().projects.retain(|e| e.id != id);
        self.persist();
    }

    pub fn upsert_environment(&self, entity: EnvironmentEntity) {
        if self.is_proxied() {
            return;
        }
        let mut guard = self.inner.write();
        upsert_by_id(&mut guard.environments, entity);
        drop(guard);
        self.persist();
    }

    pub fn remove_environment(&self, id: Uuid) {
        if self.is_proxied() {
            return;
        }
        self.inner.write().environments.retain(|e| e.id != id);
        self.persist();
    }

    pub fn tunnel_profiles(&self) -> Vec<TunnelProfile> {
        self.inner.read().tunnel_profiles.clone()
    }

    pub fn tunnel_profile(&self, id: TunnelProfileId) -> Option<TunnelProfile> {
        self.inner
            .read()
            .tunnel_profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    pub fn upsert_tunnel_profile(&self, profile: TunnelProfile) {
        let mut guard = self.inner.write();
        if let Some(existing) = guard
            .tunnel_profiles
            .iter_mut()
            .find(|p| p.id == profile.id)
        {
            *existing = profile;
        } else {
            guard.tunnel_profiles.push(profile);
        }
        drop(guard);
        self.persist();
    }

    /// Removes a tunnel profile and clears dangling references on connections.
    /// Returns how many connections referenced the profile.
    pub fn remove_tunnel_profile(&self, id: TunnelProfileId) -> usize {
        let mut guard = self.inner.write();
        guard.tunnel_profiles.retain(|p| p.id != id);
        let mut cleared = 0usize;
        for profile in &mut guard.profiles {
            if profile.tunnel_profile == Some(id) {
                profile.tunnel_profile = None;
                cleared += 1;
            }
        }
        drop(guard);
        self.persist();
        cleared
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_sql_login(
        name: impl Into<String>,
        server: impl Into<String>,
        port: u16,
        database: Option<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        encrypt: bool,
        trust_certificate: bool,
    ) -> ConnectionProfile {
        let username: String = username.into();
        let secret_ref = SecretRef {
            provider: SecretProvider::InMemory,
            key: password.into(),
        };
        ConnectionProfile {
            id: ConnectionId::new(),
            name: name.into(),
            server: server.into(),
            port,
            database,
            database_type: Default::default(),
            environment: Default::default(),
            client: None,
            project: None,
            authentication: AuthenticationType::SqlLogin {
                username,
                secret_ref,
            },
            encrypt,
            trust_server_certificate: trust_certificate,
            tls_mode: sqlly_core::TlsMode::Legacy,
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
            provenance: Default::default(),
            status: Default::default(),
            route: Default::default(),
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
            masking: sqlly_core::MaskingOverrides::default(),
            tls_trust_os_roots: false,
            tls_direct: false,
        }
    }

    /// Resolves a named tunnel profile into `profile.transport` before connect/test.
    pub fn resolve_tunnel_profile(profile: &mut ConnectionProfile) -> Result<(), String> {
        let Some(id) = profile.tunnel_profile else {
            return Ok(());
        };
        let Some(tunnel) = ConnectionStore::global().tunnel_profile(id) else {
            return Err(crate::localization::text(
                "connections.store.tunnel_profile_missing",
            ));
        };
        profile.transport = tunnel.chain;
        profile.ssh_tunnel = profile.transport.ssh.clone();
        Ok(())
    }

    /// Produces a secret-free export bundle of every saved connection and its
    /// client/project/environment graph. Passwords and secret references are
    /// omitted; SQL logins retain only the username.
    ///
    /// This was Export Connections' file before both exports moved to the
    /// one connection file (`credential_export`). Import Connections still
    /// reads it; tests write it to have a real old file to read.
    #[cfg(test)]
    pub fn export_bundle(&self) -> ProfileExportBundle {
        let guard = self.inner.read();
        ProfileExportBundle {
            version: PROFILE_EXPORT_VERSION,
            profiles: guard.profiles.iter().map(export_profile).collect(),
            clients: guard.clients.iter().map(export_client_entity).collect(),
            projects: guard.projects.iter().map(export_project_entity).collect(),
            environments: guard
                .environments
                .iter()
                .map(export_environment_entity)
                .collect(),
            sql_auth_accounts: guard
                .sql_auth_accounts
                .iter()
                .map(|account| ExportedSqlAuthAccount {
                    name: account.name.clone(),
                    username: account.username.clone(),
                })
                .collect(),
        }
    }

    /// Merges a secret-free export bundle into this store. Profiles always
    /// receive fresh ids; entities merge by name; existing ids are never
    /// overwritten. Returns the number of profiles imported.
    pub fn import_bundle(&self, bundle: &ProfileExportBundle) -> usize {
        let mut guard = self.inner.write();
        for exported in &bundle.clients {
            merge_client_entity(&mut guard, exported);
        }
        for exported in &bundle.projects {
            merge_project_entity(&mut guard, exported);
        }
        for exported in &bundle.environments {
            merge_environment_entity(&mut guard, exported);
        }
        for exported in &bundle.sql_auth_accounts {
            merge_sql_auth_account(&mut guard, exported);
        }
        let mut imported = 0usize;
        for exported in &bundle.profiles {
            guard.profiles.push(exported.into_profile());
            imported += 1;
        }
        drop(guard);
        if imported > 0
            || !bundle.clients.is_empty()
            || !bundle.projects.is_empty()
            || !bundle.environments.is_empty()
            || !bundle.sql_auth_accounts.is_empty()
        {
            self.persist();
        }
        imported
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// A client / project / environment from a credential file, with its icon
/// already written to disk. `None` fields leave an existing entity's value.
#[derive(Debug, Clone)]
pub struct SecuredEntity {
    pub name: String,
    pub color_hex: Option<String>,
    pub image_path: Option<String>,
    /// Only the listed flags change.
    pub safety: sqlly_core::credential_import::CredentialSafety,
}

#[cfg(not(target_arch = "wasm32"))]
/// A credential-file import ready to persist: every secret is already a
/// keyring reference and every image is already a file.
#[derive(Debug, Clone, Default)]
pub struct SecuredCredentialImport {
    pub clients: Vec<SecuredEntity>,
    pub projects: Vec<SecuredEntity>,
    pub environments: Vec<SecuredEntity>,
    /// Upserted by id (an id may belong to an existing account).
    pub sql_auth_accounts: Vec<SqlAuthAccount>,
    /// Upserted by id (an id may belong to an existing tunnel profile).
    pub tunnel_profiles: Vec<TunnelProfile>,
    pub profiles: Vec<ConnectionProfile>,
}

#[cfg(not(target_arch = "wasm32"))]
impl ConnectionStore {
    /// Where imported entity icons are written, next to `connections.json`.
    pub fn entity_images_dir(&self) -> PathBuf {
        self.path
            .parent()
            .map(|dir| dir.join("entity-images"))
            .unwrap_or_else(|| PathBuf::from("entity-images"))
    }

    /// Merges a secured credential import in one write and one save. Entities
    /// merge by name; accounts and tunnel profiles upsert by id; profiles are
    /// appended. Refuses (changing nothing) when the on-disk store was corrupt
    /// at load, since the result could not be saved.
    pub fn apply_credential_import(&self, import: SecuredCredentialImport) -> Result<(), String> {
        if self.persist_blocked() {
            return Err(crate::localization::text(
                "connections.store.import_blocked_by_unreadable_store",
            ));
        }
        let mut guard = self.inner.write();
        for entity in &import.clients {
            merge_secured_entity(&mut guard.clients, entity);
        }
        for entity in &import.projects {
            merge_secured_entity(&mut guard.projects, entity);
        }
        for entity in &import.environments {
            merge_secured_entity(&mut guard.environments, entity);
        }
        for account in import.sql_auth_accounts {
            match guard
                .sql_auth_accounts
                .iter_mut()
                .find(|a| a.id == account.id)
            {
                Some(existing) => *existing = account,
                None => guard.sql_auth_accounts.push(account),
            }
        }
        for tunnel in import.tunnel_profiles {
            match guard.tunnel_profiles.iter_mut().find(|t| t.id == tunnel.id) {
                Some(existing) => *existing = tunnel,
                None => guard.tunnel_profiles.push(tunnel),
            }
        }
        guard.profiles.extend(import.profiles);
        drop(guard);
        self.persist();
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
/// The shape shared by client, project, and environment entities.
trait NamedEntity {
    fn entity_name(&self) -> &str;
    fn look_mut(
        &mut self,
    ) -> (
        &mut Option<String>,
        &mut Option<String>,
        &mut SafetyFeatures,
    );
    fn named(name: String) -> Self;
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! named_entity {
    ($ty:ty) => {
        impl NamedEntity for $ty {
            fn entity_name(&self) -> &str {
                &self.name
            }
            fn look_mut(
                &mut self,
            ) -> (
                &mut Option<String>,
                &mut Option<String>,
                &mut SafetyFeatures,
            ) {
                (&mut self.color_hex, &mut self.image_path, &mut self.safety)
            }
            fn named(name: String) -> Self {
                Self {
                    id: Uuid::new_v4(),
                    name,
                    color_hex: None,
                    image_path: None,
                    safety: SafetyFeatures::default(),
                }
            }
        }
    };
}
#[cfg(not(target_arch = "wasm32"))]
named_entity!(ClientEntity);
#[cfg(not(target_arch = "wasm32"))]
named_entity!(ProjectEntity);
#[cfg(not(target_arch = "wasm32"))]
named_entity!(EnvironmentEntity);

#[cfg(not(target_arch = "wasm32"))]
fn merge_secured_entity<T: NamedEntity>(items: &mut Vec<T>, entity: &SecuredEntity) {
    let index = match items
        .iter()
        .position(|e| e.entity_name().eq_ignore_ascii_case(&entity.name))
    {
        Some(index) => index,
        None => {
            items.push(T::named(entity.name.clone()));
            items.len() - 1
        }
    };
    let (color, image, safety) = items[index].look_mut();
    if let Some(hex) = &entity.color_hex {
        *color = Some(hex.clone());
    }
    if let Some(path) = &entity.image_path {
        *image = Some(path.clone());
    }
    let mut merged = export_safety_features(safety);
    entity.safety.apply_to(&mut merged);
    *safety = import_safety_features(&merged);
}

trait IdentifiableEntity {
    fn entity_id(&self) -> Uuid;
}

impl IdentifiableEntity for ClientEntity {
    fn entity_id(&self) -> Uuid {
        self.id
    }
}
impl IdentifiableEntity for ProjectEntity {
    fn entity_id(&self) -> Uuid {
        self.id
    }
}
impl IdentifiableEntity for EnvironmentEntity {
    fn entity_id(&self) -> Uuid {
        self.id
    }
}

/// Replaces the entity with a matching id if present, otherwise appends it.
fn upsert_by_id<T: IdentifiableEntity>(items: &mut Vec<T>, entity: T) {
    let id = entity.entity_id();
    if let Some(existing) = items.iter_mut().find(|e| e.entity_id() == id) {
        *existing = entity;
    } else {
        items.push(entity);
    }
}

pub(crate) fn export_safety_features(safety: &SafetyFeatures) -> ExportedSafetyFeatures {
    ExportedSafetyFeatures {
        use_outer_transaction_rollback_scope: safety.use_outer_transaction_rollback_scope,
        only_allow_select: safety.only_allow_select,
        use_confirmation_dialog: safety.use_confirmation_dialog,
        require_ai_review_for_data_changes: safety.require_ai_review_for_data_changes,
        allow_unconstrained_update: safety.allow_unconstrained_update,
        allow_unconstrained_delete: safety.allow_unconstrained_delete,
        require_touch_id_to_connect: safety.require_touch_id_to_connect,
        require_touch_id_for_non_select: safety.require_touch_id_for_non_select,
        is_production: safety.is_production,
    }
}

fn import_safety_features(safety: &ExportedSafetyFeatures) -> SafetyFeatures {
    SafetyFeatures {
        use_outer_transaction_rollback_scope: safety.use_outer_transaction_rollback_scope,
        only_allow_select: safety.only_allow_select,
        use_confirmation_dialog: safety.use_confirmation_dialog,
        require_ai_review_for_data_changes: safety.require_ai_review_for_data_changes,
        allow_unconstrained_update: safety.allow_unconstrained_update,
        allow_unconstrained_delete: safety.allow_unconstrained_delete,
        require_touch_id_to_connect: safety.require_touch_id_to_connect,
        require_touch_id_for_non_select: safety.require_touch_id_for_non_select,
        is_production: safety.is_production,
    }
}

#[cfg(test)]
fn export_client_entity(entity: &ClientEntity) -> ExportedClientEntity {
    ExportedClientEntity {
        name: entity.name.clone(),
        color_hex: entity.color_hex.clone(),
        image_path: entity.image_path.clone(),
        safety: export_safety_features(&entity.safety),
    }
}

#[cfg(test)]
fn export_project_entity(entity: &ProjectEntity) -> ExportedProjectEntity {
    ExportedProjectEntity {
        name: entity.name.clone(),
        color_hex: entity.color_hex.clone(),
        image_path: entity.image_path.clone(),
        safety: export_safety_features(&entity.safety),
    }
}

#[cfg(test)]
fn export_environment_entity(entity: &EnvironmentEntity) -> ExportedEnvironmentEntity {
    ExportedEnvironmentEntity {
        name: entity.name.clone(),
        color_hex: entity.color_hex.clone(),
        image_path: entity.image_path.clone(),
        safety: export_safety_features(&entity.safety),
    }
}

fn merge_client_entity(data: &mut ConnectionStoreData, exported: &ExportedClientEntity) {
    if let Some(existing) = data
        .clients
        .iter_mut()
        .find(|entity| entity.name.eq_ignore_ascii_case(&exported.name))
    {
        existing.color_hex = exported.color_hex.clone();
        existing.image_path = exported.image_path.clone();
        existing.safety = import_safety_features(&exported.safety);
        return;
    }
    data.clients.push(ClientEntity {
        id: Uuid::new_v4(),
        name: exported.name.clone(),
        color_hex: exported.color_hex.clone(),
        image_path: exported.image_path.clone(),
        safety: import_safety_features(&exported.safety),
    });
}

fn merge_project_entity(data: &mut ConnectionStoreData, exported: &ExportedProjectEntity) {
    if let Some(existing) = data
        .projects
        .iter_mut()
        .find(|entity| entity.name.eq_ignore_ascii_case(&exported.name))
    {
        existing.color_hex = exported.color_hex.clone();
        existing.image_path = exported.image_path.clone();
        existing.safety = import_safety_features(&exported.safety);
        return;
    }
    data.projects.push(ProjectEntity {
        id: Uuid::new_v4(),
        name: exported.name.clone(),
        color_hex: exported.color_hex.clone(),
        image_path: exported.image_path.clone(),
        safety: import_safety_features(&exported.safety),
    });
}

fn merge_environment_entity(data: &mut ConnectionStoreData, exported: &ExportedEnvironmentEntity) {
    if let Some(existing) = data
        .environments
        .iter_mut()
        .find(|entity| entity.name.eq_ignore_ascii_case(&exported.name))
    {
        existing.color_hex = exported.color_hex.clone();
        existing.image_path = exported.image_path.clone();
        existing.safety = import_safety_features(&exported.safety);
        return;
    }
    data.environments.push(EnvironmentEntity {
        id: Uuid::new_v4(),
        name: exported.name.clone(),
        color_hex: exported.color_hex.clone(),
        image_path: exported.image_path.clone(),
        safety: import_safety_features(&exported.safety),
    });
}

fn merge_sql_auth_account(data: &mut ConnectionStoreData, exported: &ExportedSqlAuthAccount) {
    if data.sql_auth_accounts.iter().any(|account| {
        account.username.eq_ignore_ascii_case(&exported.username)
            && account.name.eq_ignore_ascii_case(&exported.name)
    }) {
        return;
    }
    data.sql_auth_accounts.push(SqlAuthAccount {
        id: Uuid::new_v4(),
        name: exported.name.clone(),
        username: exported.username.clone(),
    });
}

/// Returns true when serialized export JSON contains no secret material.
pub fn export_bundle_json_is_secret_free(json: &str) -> bool {
    let lower = json.to_ascii_lowercase();
    !lower.contains("secret_ref")
        && !lower.contains("macoskeychain")
        && !lower.contains("\"password\"")
        && !lower.contains("runtime_entra_access_token")
        && !lower.contains("runtime_cloud_iam_token")
}

impl Default for ConnectionStore {
    fn default() -> Self {
        Self::load_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_sql_login_sets_in_memory_secret() {
        let profile = ConnectionStore::create_sql_login(
            "test",
            "localhost",
            1433,
            None,
            "sa",
            "password123",
            true,
            false,
        );
        match &profile.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
                ..
            } => {
                assert_eq!(username, "sa");
                assert_eq!(secret_ref.provider, SecretProvider::InMemory);
                assert_eq!(secret_ref.key, "password123");
            }
            _ => panic!("expected SqlLogin"),
        }
        assert_eq!(profile.server, "localhost");
        assert_eq!(profile.port, 1433);
    }

    /// Gridex #18: a connection opened from a connection URL is listed for
    /// the session but its profile, and the password it holds in memory,
    /// never reach the saved file, even when other saves happen.
    #[test]
    fn unsaved_connections_never_reach_the_file() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-unsaved-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let saved = ConnectionStore::create_sql_login(
            "saved",
            "localhost",
            3306,
            None,
            "app",
            "",
            false,
            false,
        );
        store.add(saved.clone());
        let parsed =
            sqlly_core::parse_connection_url("mysql://app:url-secret@db.example:3307/sales")
                .unwrap();
        let unsaved = sqlly_core::connect_url::unsaved_profile(&parsed).unwrap();
        let unsaved_id = unsaved.id.0;
        store.add_unsaved(unsaved);
        assert!(store.is_unsaved(unsaved_id));
        assert!(store.get(unsaved_id).is_some());
        assert_eq!(store.list().len(), 2);
        // Any later save rewrites the file.
        let _ = store.record_connected(unsaved_id, Utc::now());
        store.update(saved.id.0, saved.clone());

        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(!text.contains("url-secret"), "{text}");
        assert!(!text.contains("db.example"), "{text}");
        let reopened = ConnectionStore::from_path(&tmp);
        assert!(reopened.get(unsaved_id).is_none());
        assert!(reopened.get(saved.id.0).is_some());
        let _ = std::fs::remove_file(&tmp);
    }

    /// iCloud sync reads this snapshot: a session-only connection opened
    /// from a URL must not be in it.
    #[test]
    fn sync_snapshot_leaves_out_unsaved_connections() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-sync-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let parsed =
            sqlly_core::parse_connection_url("mysql://app:url-secret@db.example:3307/sales")
                .unwrap();
        let unsaved = sqlly_core::connect_url::unsaved_profile(&parsed).unwrap();
        let id = unsaved.id.0;
        store.add_unsaved(unsaved);
        let snapshot = store.sync_snapshot().expect("store is writable");
        assert!(snapshot.profiles.iter().all(|p| p.id.0 != id));
        let _ = std::fs::remove_file(&tmp);
    }

    /// Saving a session-only connection from the editor keeps it, so the
    /// password the editor wrote to the keyring belongs to a saved profile.
    #[test]
    fn saving_an_unsaved_connection_keeps_it() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-keep-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let parsed =
            sqlly_core::parse_connection_url("mysql://app:url-secret@db.example:3307/sales")
                .unwrap();
        let unsaved = sqlly_core::connect_url::unsaved_profile(&parsed).unwrap();
        let id = unsaved.id.0;
        store.add_unsaved(unsaved.clone());
        // Still carrying the URL's password inline: it must stay unsaved.
        store.keep_unsaved(id);
        assert!(store.is_unsaved(id));
        // The editor's save swaps in a keyring-backed secret, then keeps it.
        let mut edited = unsaved;
        edited.authentication = sqlly_core::AuthenticationType::SqlLogin {
            username: "app".into(),
            secret_ref: sqlly_core::SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: Uuid::new_v4().to_string(),
            },
        };
        store.update(id, edited);
        store.keep_unsaved(id);
        assert!(!store.is_unsaved(id));
        let reopened = ConnectionStore::from_path(&tmp);
        assert!(reopened.get(id).is_some());
        let text = std::fs::read_to_string(&tmp).unwrap();
        assert!(!text.contains("url-secret"), "{text}");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn health_updates_persist_without_rewriting_profile_fields() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-health-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let profile = ConnectionStore::create_sql_login(
            "health",
            "localhost",
            1433,
            None,
            "sa",
            "memory-only-secret",
            true,
            false,
        );
        let id = profile.id.0;
        store.add(profile);
        let tested_at = Utc::now();

        assert!(store.record_failure(id, tested_at, "password=hunter2 refused"));
        let failed = store.get(id).unwrap();
        assert_eq!(
            failed.status.last_error_summary.as_deref(),
            Some("password=<redacted> refused")
        );
        assert!(store.record_test_success(id, tested_at));

        let reopened = ConnectionStore::from_path(&tmp);
        let healthy = reopened.get(id).unwrap();
        assert_eq!(healthy.name, "health");
        assert_eq!(
            healthy.status.last_test_status,
            Some(sqlly_core::ConnectionTestStatus::Succeeded)
        );
        assert!(healthy.status.last_error_summary.is_none());
        assert!(!reopened.record_connected(Uuid::new_v4(), tested_at));
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn store_add_update_remove_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-store-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        assert!(store.list().is_empty());

        let profile = ConnectionStore::create_sql_login(
            "test",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        );
        let id = profile.id.0;
        store.add(profile);
        assert_eq!(store.list().len(), 1);

        let mut updated = store.get(id).unwrap();
        updated.server = "changed".to_string();
        store.update(id, updated);
        assert_eq!(store.get(id).unwrap().server, "changed");

        store.remove(id);
        assert!(store.list().is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn add_after_inserts_beside_the_anchor_and_appends_when_the_anchor_is_gone() {
        let tmp =
            std::env::temp_dir().join(format!("sqlly-test-add-after-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let make = |name: &str| {
            ConnectionStore::create_sql_login(name, "localhost", 1433, None, "sa", "", true, false)
        };
        let first = make("first");
        let last = make("last");
        let first_id = first.id.0;
        store.add(first);
        store.add(last);

        let copy = make("first (copy)");
        let copy_id = copy.id.0;
        store.add_after(first_id, copy);
        let names: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["first", "first (copy)", "last"]);

        // The order survives a reload — it is what the persisted file holds.
        let reopened = ConnectionStore::from_path(&tmp);
        let names: Vec<String> = reopened.list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["first", "first (copy)", "last"]);
        assert!(reopened.get(copy_id).is_some());

        // A vanished anchor degrades to a plain append rather than a lost save.
        store.add_after(Uuid::new_v4(), make("orphan"));
        let names: Vec<String> = store.list().into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["first", "first (copy)", "last", "orphan"]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn migrate_in_memory_sql_password_to_keychain() {
        keyring::mock::install();
        let tmp = std::env::temp_dir().join(format!("sqlly-test-migrate-pwd-{}", Uuid::new_v4()));
        let legacy_password = "legacy-cleartext-password";
        let profile_id = Uuid::new_v4();
        let data = ConnectionStoreData {
            version: STORE_VERSION,
            profiles: vec![ConnectionProfile {
                id: ConnectionId(profile_id),
                name: "Legacy".to_string(),
                server: "localhost".to_string(),
                port: 1433,
                database: None,
                database_type: Default::default(),
                environment: Default::default(),
                client: None,
                project: None,
                authentication: AuthenticationType::SqlLogin {
                    username: "sa".to_string(),
                    secret_ref: SecretRef {
                        provider: SecretProvider::InMemory,
                        key: legacy_password.to_string(),
                    },
                },
                encrypt: true,
                trust_server_certificate: false,
                tls_mode: sqlly_core::TlsMode::Legacy,
                tls_ca_file: None,
                tls_client_cert_file: None,
                tls_client_key_file: None,
                timeout_seconds: 30,
                statement_timeout_seconds: None,
                application_name: "SQLLY".to_string(),
                auto_wrap_rollback: false,
                block_all_but_select: false,
                confirm_data_changes: false,
                allow_unbounded_mutations: false,
                production_override: None,
                rollback_policy_defaulted: false,
                entra_account: None,
                runtime_entra_access_token: None,
                runtime_cloud_iam_token: None,
                enforce_read_only: false,
                provenance: Default::default(),
                status: Default::default(),
                route: Default::default(),
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
                masking: sqlly_core::MaskingOverrides::default(),
                tls_trust_os_roots: false,
                tls_direct: false,
            }],
            clients: Vec::new(),
            projects: Vec::new(),
            environments: built_in_environments(),
            sql_auth_accounts: Vec::new(),
            server_grouping: ServerGroupingMode::default(),
            tunnel_profiles: Vec::new(),
        };
        let json = serde_json::to_string_pretty(&data).unwrap();
        std::fs::write(&tmp, json).unwrap();

        let store = ConnectionStore::from_path(&tmp);
        let profile = store.get(profile_id).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = &profile.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
        let account_id = Uuid::parse_str(&secret_ref.key).expect("uuid key");
        assert_eq!(
            crate::sql_credentials::load_password(account_id)
                .unwrap()
                .as_deref(),
            Some(legacy_password)
        );
        let accounts = store.sql_auth_accounts();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].id, account_id);
        assert_eq!(accounts[0].username, "sa");

        let disk = std::fs::read_to_string(&tmp).unwrap();
        assert!(!disk.contains(legacy_password));

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn older_store_versions_are_rewritten_at_the_current_version() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-version-{}", Uuid::new_v4()));
        std::fs::write(&tmp, r#"{"version":1,"profiles":[]}"#).unwrap();
        let store = ConnectionStore::from_path(&tmp);
        assert_eq!(store.inner.read().version, STORE_VERSION);
        let disk: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tmp).unwrap()).unwrap();
        assert_eq!(disk["version"], STORE_VERSION);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn migrate_skips_empty_in_memory_sql_password() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-migrate-empty-{}", Uuid::new_v4()));
        let profile_id = Uuid::new_v4();
        let data = ConnectionStoreData {
            version: STORE_VERSION,
            profiles: vec![ConnectionProfile {
                id: ConnectionId(profile_id),
                name: "Sample".to_string(),
                server: "preview".to_string(),
                port: 0,
                database: None,
                database_type: Default::default(),
                environment: Default::default(),
                client: None,
                project: None,
                authentication: AuthenticationType::SqlLogin {
                    username: "preview".to_string(),
                    secret_ref: SecretRef {
                        provider: SecretProvider::InMemory,
                        key: String::new(),
                    },
                },
                encrypt: false,
                trust_server_certificate: false,
                tls_mode: sqlly_core::TlsMode::Legacy,
                tls_ca_file: None,
                tls_client_cert_file: None,
                tls_client_key_file: None,
                timeout_seconds: 30,
                statement_timeout_seconds: None,
                application_name: "SQLLY".to_string(),
                auto_wrap_rollback: false,
                block_all_but_select: false,
                confirm_data_changes: false,
                allow_unbounded_mutations: false,
                production_override: None,
                rollback_policy_defaulted: false,
                entra_account: None,
                runtime_entra_access_token: None,
                runtime_cloud_iam_token: None,
                enforce_read_only: false,
                provenance: Default::default(),
                status: Default::default(),
                route: Default::default(),
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
                masking: sqlly_core::MaskingOverrides::default(),
                tls_trust_os_roots: false,
                tls_direct: false,
            }],
            clients: Vec::new(),
            projects: Vec::new(),
            environments: built_in_environments(),
            sql_auth_accounts: Vec::new(),
            server_grouping: ServerGroupingMode::default(),
            tunnel_profiles: Vec::new(),
        };
        std::fs::write(&tmp, serde_json::to_string_pretty(&data).unwrap()).unwrap();

        let store = ConnectionStore::from_path(&tmp);
        let profile = store.get(profile_id).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = &profile.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert!(secret_ref.key.is_empty());
        assert!(store.sql_auth_accounts().is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn migrate_skips_profiles_already_on_keychain() {
        keyring::mock::install();
        let tmp = std::env::temp_dir().join(format!("sqlly-test-migrate-skip-{}", Uuid::new_v4()));
        let account_id = Uuid::new_v4();
        crate::sql_credentials::store_password(account_id, "stored-secret").unwrap();
        let profile_id = Uuid::new_v4();
        let data = ConnectionStoreData {
            version: STORE_VERSION,
            profiles: vec![ConnectionProfile {
                id: ConnectionId(profile_id),
                name: "Saved".to_string(),
                server: "localhost".to_string(),
                port: 1433,
                database: None,
                database_type: Default::default(),
                environment: Default::default(),
                client: None,
                project: None,
                authentication: AuthenticationType::SqlLogin {
                    username: "sa".to_string(),
                    secret_ref: SecretRef {
                        provider: SecretProvider::MacOsKeychain,
                        key: account_id.to_string(),
                    },
                },
                encrypt: true,
                trust_server_certificate: false,
                tls_mode: sqlly_core::TlsMode::Legacy,
                tls_ca_file: None,
                tls_client_cert_file: None,
                tls_client_key_file: None,
                timeout_seconds: 30,
                statement_timeout_seconds: None,
                application_name: "SQLLY".to_string(),
                auto_wrap_rollback: false,
                block_all_but_select: false,
                confirm_data_changes: false,
                allow_unbounded_mutations: false,
                production_override: None,
                rollback_policy_defaulted: false,
                entra_account: None,
                runtime_entra_access_token: None,
                runtime_cloud_iam_token: None,
                enforce_read_only: false,
                provenance: Default::default(),
                status: Default::default(),
                route: Default::default(),
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
                masking: sqlly_core::MaskingOverrides::default(),
                tls_trust_os_roots: false,
                tls_direct: false,
            }],
            clients: Vec::new(),
            projects: Vec::new(),
            environments: built_in_environments(),
            sql_auth_accounts: vec![SqlAuthAccount {
                id: account_id,
                name: "Saved".to_string(),
                username: "sa".to_string(),
            }],
            server_grouping: ServerGroupingMode::default(),
            tunnel_profiles: Vec::new(),
        };
        std::fs::write(&tmp, serde_json::to_string_pretty(&data).unwrap()).unwrap();

        let store = ConnectionStore::from_path(&tmp);
        assert_eq!(store.sql_auth_accounts().len(), 1);
        assert_eq!(store.sql_auth_accounts()[0].id, account_id);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn sql_auth_account_metadata_round_trips_without_password() {
        let tmp =
            std::env::temp_dir().join(format!("sqlly-test-sql-accounts-{}.json", Uuid::new_v4()));
        let id = Uuid::new_v4();
        let store = ConnectionStore::from_path(&tmp);
        store.upsert_sql_auth_account(SqlAuthAccount {
            id,
            name: "Reporting".to_string(),
            username: "report_user".to_string(),
        });

        let reloaded = ConnectionStore::from_path(&tmp);
        assert_eq!(
            reloaded.sql_auth_accounts(),
            vec![SqlAuthAccount {
                id,
                name: "Reporting".to_string(),
                username: "report_user".to_string(),
            }]
        );
        let json = std::fs::read_to_string(&tmp).unwrap();
        assert!(!json.contains("password"));

        reloaded.remove_sql_auth_account(id);
        assert!(reloaded.sql_auth_accounts().is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn built_in_environments_have_fixed_ids() {
        let envs = built_in_environments();
        assert_eq!(envs.len(), 4);
        assert_eq!(envs[0].name, "Dev");
        assert_eq!(envs[1].name, "Test");
        assert_eq!(envs[2].name, "Staging");
        assert_eq!(envs[3].name, "Production");
        assert_eq!(
            envs[0].id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()
        );
        assert_eq!(
            envs[3].id,
            Uuid::parse_str("00000000-0000-0000-0000-000000000004").unwrap()
        );
    }

    #[test]
    fn fresh_store_seeds_built_in_environments() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-envs-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        assert_eq!(store.environments().len(), 4);
        assert_eq!(store.environments()[0].name, "Dev");
        assert!(store.clients().is_empty());
        assert!(store.projects().is_empty());
        assert_eq!(store.server_grouping(), ServerGroupingMode::None);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn client_upsert_replace_and_remove() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-cl-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let id = Uuid::new_v4();
        store.upsert_client(ClientEntity {
            id,
            name: "Acme".to_string(),
            color_hex: None,
            image_path: None,
            safety: SafetyFeatures::default(),
        });
        assert_eq!(store.clients().len(), 1);
        assert_eq!(store.clients()[0].name, "Acme");

        // Replace by id.
        store.upsert_client(ClientEntity {
            id,
            name: "Acme Co".to_string(),
            color_hex: Some("#1e88e5".to_string()),
            image_path: Some("/icons/acme.png".to_string()),
            safety: SafetyFeatures {
                only_allow_select: true,
                ..Default::default()
            },
        });
        assert_eq!(store.clients().len(), 1);
        assert_eq!(store.clients()[0].name, "Acme Co");
        assert_eq!(store.clients()[0].color_hex.as_deref(), Some("#1e88e5"));
        assert_eq!(
            store.clients()[0].image_path.as_deref(),
            Some("/icons/acme.png")
        );
        assert!(store.clients()[0].safety.only_allow_select);

        // A second, distinct id appends rather than replacing.
        let id2 = Uuid::new_v4();
        store.upsert_client(ClientEntity {
            id: id2,
            name: "Beta".to_string(),
            color_hex: None,
            image_path: None,
            safety: SafetyFeatures::default(),
        });
        assert_eq!(store.clients().len(), 2);

        store.remove_client(id);
        assert_eq!(store.clients().len(), 1);
        assert_eq!(store.clients()[0].name, "Beta");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn project_and_environment_crud_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-pe-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let pid = Uuid::new_v4();
        store.upsert_project(ProjectEntity {
            id: pid,
            name: "Apollo".to_string(),
            color_hex: None,
            image_path: Some("/icons/apollo.svg".to_string()),
            safety: SafetyFeatures::default(),
        });
        assert_eq!(store.projects().len(), 1);
        assert_eq!(
            store.projects()[0].image_path.as_deref(),
            Some("/icons/apollo.svg")
        );

        // Built-in environments are still present after project edits.
        assert_eq!(store.environments().len(), 4);
        let eid = store.environments()[0].id;
        store.upsert_environment(EnvironmentEntity {
            id: eid,
            name: "Dev (renamed)".to_string(),
            color_hex: Some("#ff0000".to_string()),
            image_path: Some("/icons/dev.webp".to_string()),
            safety: SafetyFeatures::default(),
        });
        // Upserting a built-in id replaces it in place (no duplicate).
        assert_eq!(store.environments().len(), 4);
        assert_eq!(store.environments()[0].name, "Dev (renamed)");
        assert_eq!(
            store.environments()[0].image_path.as_deref(),
            Some("/icons/dev.webp")
        );

        store.remove_project(pid);
        assert!(store.projects().is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn server_grouping_persists_across_reopen() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-grp-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        assert_eq!(store.server_grouping(), ServerGroupingMode::None);
        store.set_server_grouping(ServerGroupingMode::ByClient);
        assert_eq!(store.server_grouping(), ServerGroupingMode::ByClient);

        // Reopen from the same path -> persisted value survives.
        let reopened = ConnectionStore::from_path(&tmp);
        assert_eq!(reopened.server_grouping(), ServerGroupingMode::ByClient);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn legacy_store_json_without_entity_fields_loads() {
        // A pre-entity-management connections.json carries only version+profiles.
        let tmp = std::env::temp_dir().join(format!("sqlly-test-leg-{}.json", Uuid::new_v4()));
        let legacy = serde_json::json!({
            "version": 1u32,
            "profiles": [],
        });
        std::fs::write(&tmp, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let store = ConnectionStore::from_path(&tmp);
        // Missing fields default: built-in environments seeded, grouping None.
        assert_eq!(store.environments().len(), 4);
        assert_eq!(store.server_grouping(), ServerGroupingMode::None);
        assert!(store.clients().is_empty());
        assert!(store.projects().is_empty());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn missing_store_allows_persist() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-missing-store-{}", Uuid::new_v4()));
        assert!(!tmp.exists());

        let store = ConnectionStore::from_path(&tmp);
        let profile = ConnectionStore::create_sql_login(
            "first",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        );
        store.add(profile);

        assert!(tmp.exists());
        let reopened = ConnectionStore::from_path(&tmp);
        assert_eq!(reopened.list().len(), 1);
        assert_eq!(reopened.list()[0].name, "first");
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn corrupt_store_refuses_persist_without_clobbering() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-corrupt-store-{}", Uuid::new_v4()));
        let corrupt = "{ not json";
        std::fs::write(&tmp, corrupt).unwrap();

        let store = ConnectionStore::from_path(&tmp);
        assert!(
            store.load_error().is_some(),
            "an unreadable store must keep its reason so startup can report it"
        );
        store.add(ConnectionStore::create_sql_login(
            "blocked",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        ));
        store.upsert_client(ClientEntity {
            id: Uuid::new_v4(),
            name: "Acme".to_string(),
            color_hex: None,
            image_path: None,
            safety: SafetyFeatures::default(),
        });

        assert_eq!(
            std::fs::read_to_string(&tmp).unwrap(),
            corrupt,
            "corrupt store must not be overwritten"
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn bracketed_profile_names_round_trip() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-bracket-{}", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let profile = ConnectionStore::create_sql_login(
            "Server [prod]]",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        );
        let id = profile.id.0;
        store.add(profile);

        let reopened = ConnectionStore::from_path(&tmp);
        assert_eq!(reopened.get(id).unwrap().name, "Server [prod]]");
        let _ = std::fs::remove_file(&tmp);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn persist_leaves_no_tmp_files() {
        let dir = std::env::temp_dir().join(format!("sqlly-test-atomic-dir-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let tmp = dir.join("connections.json");
        let store = ConnectionStore::from_path(&tmp);
        store.add(ConnectionStore::create_sql_login(
            "atomic",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        ));

        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files must be renamed away");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provisioned_connections_are_listed_but_never_saved() {
        let dir = std::env::temp_dir().join(format!("sqlly-test-provisioned-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("connections.json");
        let store = ConnectionStore::from_path(&path);
        let vars: std::collections::BTreeMap<String, String> = [
            ("SENTINEL_PW", "sentinel-secret-value"),
            (
                "SQLLY_CONNECTION_FLEET_CACHE",
                "redis+sentinel://:literal-secret-value@s1:26379,s2/mymaster?sentinelPassword=${SENTINEL_PW}",
            ),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let provisioning = sqlly_core::provisioning::provision(None, &vars);
        assert!(
            provisioning.warnings.is_empty(),
            "{:?}",
            provisioning.warnings
        );
        store.add_provisioned(provisioning.profiles());
        // Adding twice (a second load) does not duplicate.
        store.add_provisioned(provisioning.profiles());
        let saved = ConnectionProfile::sql_login(
            "saved",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::Environment,
                key: "SA_PASSWORD".into(),
            },
        );
        store.add(saved);
        assert_eq!(store.list().len(), 2);
        let provisioned_id = provisioning.connections[0].profile.id.0;
        assert!(connection_is_provisioned_in(&store, provisioned_id));

        // An in-session edit keeps it provisioned and unsaved.
        let mut edited = store.get(provisioned_id).unwrap();
        edited.provenance = sqlly_core::ConnectionProvenance::Manual;
        edited.favorite = true;
        store.update(provisioned_id, edited);
        assert!(connection_is_provisioned_in(&store, provisioned_id));
        assert!(store.get(provisioned_id).unwrap().favorite);

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"saved\""), "{text}");
        for absent in [
            "FLEET_CACHE",
            "mymaster",
            "literal-secret-value",
            "sentinel-secret-value",
        ] {
            assert!(!text.contains(absent), "{absent} leaked into {text}");
        }
        let reloaded = ConnectionStore::from_path(&path);
        assert_eq!(reloaded.list().len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn connection_is_provisioned_in(store: &ConnectionStore, id: Uuid) -> bool {
        store.get(id).is_some_and(|p| is_provisioned(&p))
    }

    #[cfg(unix)]
    #[test]
    fn persisted_store_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = std::env::temp_dir().join(format!("sqlly-test-mode-{}.json", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        store.add(ConnectionStore::create_sql_login(
            "secure",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        ));

        let mode = std::fs::metadata(&tmp).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn export_bundle_omits_secrets_and_import_assigns_fresh_ids() {
        keyring::mock::install();
        let tmp = std::env::temp_dir().join(format!("sqlly-test-export-{}", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let mut profile = ConnectionStore::create_sql_login(
            "prod",
            "db.example.com",
            1433,
            Some("sales".to_string()),
            "sa",
            "super-secret-password",
            true,
            false,
        );
        let original_id = profile.id.0;
        profile.client = Some("Acme".to_string());
        store.add(profile);
        store.upsert_client(ClientEntity {
            id: Uuid::new_v4(),
            name: "Acme".to_string(),
            color_hex: Some("#123456".to_string()),
            image_path: None,
            safety: SafetyFeatures {
                only_allow_select: true,
                ..SafetyFeatures::default()
            },
        });

        let bundle = store.export_bundle();
        let json = serde_json::to_string_pretty(&bundle).unwrap();
        assert!(export_bundle_json_is_secret_free(&json));
        assert!(!json.contains("super-secret-password"));
        assert!(!json.contains("secret_ref"));

        let target = ConnectionStore::from_path(tmp.with_extension("import-target.json"));
        let imported = target.import_bundle(&bundle);
        assert_eq!(imported, 1);
        let profiles = target.list();
        assert_eq!(profiles.len(), 1);
        assert_ne!(profiles[0].id.0, original_id);
        assert_eq!(profiles[0].client.as_deref(), Some("Acme"));
        assert_eq!(
            target
                .clients()
                .iter()
                .find(|client| client.name == "Acme")
                .map(|client| client.safety.only_allow_select),
            Some(true)
        );
        let _ = std::fs::remove_file(&tmp);
        let _ = std::fs::remove_file(tmp.with_extension("import-target.json"));
    }

    #[test]
    fn import_bundle_merges_entities_by_name_without_overwriting_profile_ids() {
        let tmp = std::env::temp_dir().join(format!("sqlly-test-import-{}", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let existing = ConnectionStore::create_sql_login(
            "existing",
            "localhost",
            1433,
            None,
            "sa",
            "pwd",
            true,
            false,
        );
        let existing_id = existing.id.0;
        store.add(existing);

        let mut incoming = store.export_bundle();
        incoming.profiles[0].name = "imported".to_string();
        incoming.clients.push(sqlly_core::ExportedClientEntity {
            name: "Shared".to_string(),
            color_hex: Some("#abcdef".to_string()),
            image_path: None,
            safety: sqlly_core::ExportedSafetyFeatures {
                use_confirmation_dialog: true,
                ..sqlly_core::ExportedSafetyFeatures::default()
            },
        });

        let imported = store.import_bundle(&incoming);
        assert_eq!(imported, 1);
        assert_eq!(store.list().len(), 2);
        assert_eq!(store.get(existing_id).unwrap().name, "existing");
        assert!(
            store
                .clients()
                .iter()
                .any(|client| client.name == "Shared" && client.safety.use_confirmation_dialog)
        );
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn tunnel_profile_upsert_persists_and_remove_clears_references() {
        use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig, TransportChain};

        let tmp = std::env::temp_dir().join(format!("sqlly-tunnel-profiles-{}", Uuid::new_v4()));
        let store = ConnectionStore::from_path(&tmp);
        let profile_id = Uuid::new_v4();
        let tunnel_id = Uuid::new_v4();
        let mut connection = ConnectionProfile::sql_login(
            "db",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".to_string(),
            },
        );
        connection.id = ConnectionId(profile_id);
        connection.tunnel_profile = Some(tunnel_id);
        store.add(connection);

        let tunnel = TunnelProfile {
            id: tunnel_id,
            name: "bastion".to_string(),
            chain: TransportChain {
                proxy: None,
                ssh: Some(SshTunnelConfig {
                    target: SshHop {
                        host: "jump".to_string(),
                        port: 22,
                        username: "u".to_string(),
                        auth: SshAuth::Agent,
                    },
                    jump: None,
                    keepalive: Default::default(),
                }),
                kube: None,
                ssm: None,
            },
        };
        store.upsert_tunnel_profile(tunnel);
        assert_eq!(store.tunnel_profiles().len(), 1);

        let cleared = store.remove_tunnel_profile(tunnel_id);
        assert_eq!(cleared, 1);
        assert!(store.tunnel_profiles().is_empty());
        assert!(store.get(profile_id).unwrap().tunnel_profile.is_none());

        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod proxied_tests {
    use super::*;
    use sqlly_core::ConnectionProvenance;

    const FILE: &str = r##"{
      "format": "sqlly-credentials",
      "version": 1,
      "clients": [ { "name": "Acme", "color": "#1E88E5" } ],
      "environments": [ { "name": "Production", "color": "#C62828", "safety": { "only_allow_select": true } } ],
      "connections": [
        { "name": "Ops", "engine": "sql-server", "server": "sql01", "client": "Acme",
          "environment": "production",
          "authentication": { "type": "sql-login", "username": "ops", "password": "ops-secret" } }
      ]
    }"##;

    /// The proxy's listing replaces the store wholesale, becomes read-only,
    /// and never reaches disk.
    #[test]
    fn replace_with_proxied_locks_the_store_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("connections.json");
        let store = ConnectionStore::from_path(&path);
        store.add(ConnectionProfile::sql_login(
            "local",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        ));
        assert!(path.exists());
        let before = std::fs::read_to_string(&path).unwrap();

        let catalog = sqlly_core::credential_proxy::ProxyCatalog::from_credential_text(FILE)
            .unwrap()
            .client_listing();
        store.replace_with_proxied(&catalog);
        assert!(store.is_proxied());

        let profiles = store.list();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].name, "Ops");
        assert_eq!(profiles[0].provenance, ConnectionProvenance::Proxied);
        assert_eq!(store.clients().len(), 1);
        assert_eq!(store.clients()[0].color_hex.as_deref(), Some("#1E88E5"));
        let production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .unwrap();
        assert_eq!(production.color_hex.as_deref(), Some("#C62828"));
        assert!(production.safety.only_allow_select);
        // Built-in environments still fill the gaps.
        assert!(store.environments().iter().any(|e| e.name == "Dev"));

        // Mutators are no-ops and nothing was written.
        let id = profiles[0].id.0;
        store.remove(id);
        assert_eq!(store.list().len(), 1);
        store.add(profiles[0].clone());
        assert_eq!(store.list().len(), 1);
        store.remove_client(store.clients()[0].id);
        assert_eq!(store.clients().len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }
}
