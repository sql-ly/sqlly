use serde::Deserialize;
use sqlly_core::connection::{AuthenticationType, ConnectionEnvironment, ConnectionProfile};
use sqlly_intel::query::safety::ProductionGuard;
use uuid::Uuid;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SafetyView {
    #[serde(default)]
    pub only_allow_select: bool,
    #[serde(default)]
    pub is_production: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnvironmentView {
    pub name: String,
    #[serde(default)]
    pub safety: SafetyView,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct StoreView {
    #[serde(default)]
    pub profiles: Vec<ConnectionProfile>,
    #[serde(default)]
    pub environments: Vec<EnvironmentView>,
}

impl StoreView {
    pub fn parse(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|error| error.to_string())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn default_path() -> std::path::PathBuf {
        if let Ok(path) = std::env::var("SQLLY_MCP_STORE_PATH") {
            return std::path::PathBuf::from(path);
        }
        if let Some(support) = dirs::config_dir() {
            support.join("SQLLY").join("connections.json")
        } else {
            std::path::PathBuf::from("connections.json")
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn load() -> Result<Self, String> {
        let path = Self::default_path();
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        Self::parse(&text)
    }

    pub fn profile(&self, id: Uuid) -> Option<&ConnectionProfile> {
        self.profiles.iter().find(|profile| profile.id.0 == id)
    }

    pub fn profile_by_name(&self, name: &str) -> Option<&ConnectionProfile> {
        let matches: Vec<_> = self
            .profiles
            .iter()
            .filter(|profile| profile.name.eq_ignore_ascii_case(name))
            .collect();
        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
    }

    pub fn environment_name(env: ConnectionEnvironment) -> Option<&'static str> {
        env.entity_name()
    }

    fn environment_entity(&self, profile: &ConnectionProfile) -> Option<&EnvironmentView> {
        profile
            .environment
            .find_entity(&self.environments, |entity| &entity.name)
    }

    pub fn production_guard(&self, profile: &ConnectionProfile) -> ProductionGuard {
        let entity_flag = self
            .environment_entity(profile)
            .map(|entity| entity.safety.is_production)
            .unwrap_or(false);
        let env_is_production =
            entity_flag || profile.environment == ConnectionEnvironment::Production;
        ProductionGuard::resolve(env_is_production, profile.production_override)
    }

    pub fn forces_select_only(&self, profile: &ConnectionProfile) -> bool {
        profile.block_all_but_select
            || self
                .environment_entity(profile)
                .map(|entity| entity.safety.only_allow_select)
                .unwrap_or(false)
    }
}

/// Runtime-only copy with the keyring secret resolved into `InMemory`, for the
/// PostgreSQL/MySQL drivers that do not call `resolve_secret` themselves.
/// Never persisted, never logged.
#[cfg(not(target_arch = "wasm32"))]
pub fn hydrate_profile(mut profile: ConnectionProfile) -> Result<ConnectionProfile, String> {
    if matches!(
        profile.authentication,
        AuthenticationType::EntraBrowser
            | AuthenticationType::EntraDeviceCode
            | AuthenticationType::AwsRdsIam { .. }
            | AuthenticationType::GcpCloudSqlIam { .. }
    ) {
        return Ok(profile);
    }
    // SQL-login passwords and libSQL auth tokens share the keyring path.
    if let Some(secret_ref) = profile.authentication.secret_ref()
        && secret_ref.provider == sqlly_core::SecretProvider::MacOsKeychain
    {
        let password = sqlly_core::secret_resolver::resolve_secret(secret_ref)
            .map_err(|error| error.to_string())?;
        if let Some(secret_ref) = profile.authentication.secret_ref_mut() {
            secret_ref.provider = sqlly_core::SecretProvider::InMemory;
            secret_ref.key = password;
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    const FIXTURE: &str = r#"{
        "version": 1,
        "profiles": [
            {
                "id": "11111111-1111-1111-1111-111111111111",
                "name": "prod-db",
                "server": "prod.example.com",
                "port": 1433,
                "database": "app",
                "environment": "PRODUCTION",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            },
            {
                "id": "22222222-2222-2222-2222-222222222222",
                "name": "dev-db",
                "server": "localhost",
                "port": 1433,
                "database": "app",
                "environment": "DEV",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY",
                "production_override": true
            }
        ],
        "environments": [
            { "name": "Production", "safety": { "is_production": true } }
        ]
    }"#;

    #[test]
    fn parse_fixture() {
        let store = StoreView::parse(FIXTURE).unwrap();
        assert_eq!(store.profiles.len(), 2);
    }

    #[test]
    fn profile_by_name_case_insensitive() {
        let store = StoreView::parse(FIXTURE).unwrap();
        assert!(store.profile_by_name("DEV-DB").is_some());
        assert!(store.profile_by_name("missing").is_none());
    }

    #[test]
    fn profile_by_name_duplicate_returns_none() {
        let mut store = StoreView::parse(FIXTURE).unwrap();
        store.profiles.push(store.profiles[0].clone());
        assert!(store.profile_by_name("prod-db").is_none());
    }

    #[test]
    fn production_guard_production_profile() {
        let store = StoreView::parse(FIXTURE).unwrap();
        let profile = store.profile_by_name("prod-db").unwrap();
        assert!(store.production_guard(profile).is_production);
    }

    #[test]
    fn production_guard_override_true() {
        let store = StoreView::parse(FIXTURE).unwrap();
        let profile = store.profile_by_name("dev-db").unwrap();
        assert!(store.production_guard(profile).is_production);
    }

    #[test]
    fn production_without_entity_flag_still_production() {
        let store = StoreView::parse(
            r#"{
            "profiles": [{
                "id": "33333333-3333-3333-3333-333333333333",
                "name": "p",
                "server": "x",
                "port": 1,
                "environment": "PRODUCTION",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            }],
            "environments": []
        }"#,
        )
        .unwrap();
        let profile = &store.profiles[0];
        assert!(store.production_guard(profile).is_production);
    }

    #[test]
    fn forces_select_only_from_entity() {
        let store = StoreView::parse(
            r#"{
            "profiles": [{
                "id": "44444444-4444-4444-4444-444444444444",
                "name": "p",
                "server": "x",
                "port": 1,
                "environment": "DEV",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            }],
            "environments": [{ "name": "Development", "safety": { "only_allow_select": true } }]
        }"#,
        )
        .unwrap();
        assert!(store.forces_select_only(&store.profiles[0]));
    }

    /// The app seeds the built-in DEV environment entity as "Dev"; protections
    /// set on it must reach DEV profiles (they used to look for "Development").
    #[test]
    fn forces_select_only_from_built_in_dev_entity() {
        let store = StoreView::parse(
            r#"{
            "profiles": [{
                "id": "55555555-5555-5555-5555-555555555555",
                "name": "p",
                "server": "x",
                "port": 1,
                "environment": "DEV",
                "authentication": { "SqlLogin": { "username": "sa", "secret_ref": { "provider": "InMemory", "key": "x" } } },
                "encrypt": true,
                "trust_server_certificate": false,
                "timeout_seconds": 30,
                "application_name": "SQLLY"
            }],
            "environments": [{ "name": "Dev", "safety": { "only_allow_select": true } }]
        }"#,
        )
        .unwrap();
        assert!(store.forces_select_only(&store.profiles[0]));
    }

    #[test]
    fn hydrate_in_memory_passthrough() {
        let profile = ConnectionProfile::sql_login(
            "t",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "pw".into(),
            },
        );
        #[cfg(not(target_arch = "wasm32"))]
        {
            let hydrated = hydrate_profile(profile.clone()).unwrap();
            let AuthenticationType::SqlLogin { secret_ref, .. } = &hydrated.authentication else {
                panic!("expected sql login");
            };
            assert_eq!(secret_ref.key, "pw");
        }
        let _ = profile;
    }
}
