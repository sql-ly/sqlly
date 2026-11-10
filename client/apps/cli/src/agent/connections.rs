use crate::agent::CliError;
use serde::Deserialize;
use sqlly_core::{AuthenticationType, ConnectionEnvironment, ConnectionProfile, DatabaseType};
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
pub struct StoreFile {
    #[allow(dead_code)]
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<ConnectionProfile>,
    #[serde(default)]
    pub environments: Vec<EnvironmentRow>,
}

#[derive(Debug, Deserialize)]
pub struct EnvironmentRow {
    #[allow(dead_code)]
    pub id: uuid::Uuid,
    pub name: String,
    #[serde(default)]
    pub safety: serde_json::Value,
}

pub fn store_path() -> PathBuf {
    if let Ok(path) = std::env::var("SQLLY_CONNECTIONS_FILE")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("SQLLY")
        .join("connections.json")
}

/// The saved store plus the provisioned connections (a provisioning file or
/// `SQLLY_CONNECTION_*` variables; see `sqlly_core::provisioning`).
pub fn load_store(path: &Path) -> Result<StoreFile, CliError> {
    let mut store = load_saved_store(path)?;
    let provisioning =
        sqlly_core::provisioning::load_provisioned_connections(dirs::config_dir().map(|dir| {
            dir.join("SQLLY")
                .join(sqlly_core::provisioning::PROVISIONED_FILE_NAME)
        }));
    for warning in &provisioning.warnings {
        eprintln!("warning: provisioned connections: {warning}");
    }
    for profile in provisioning.profiles() {
        if !store.profiles.iter().any(|p| p.id == profile.id) {
            store.profiles.push(profile);
        }
    }
    Ok(store)
}

fn load_saved_store(path: &Path) -> Result<StoreFile, CliError> {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|err| {
            CliError::io(format!("failed to parse connections file: {err}"))
                .with_hint("open the app once to create the store")
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(StoreFile {
            version: 1,
            profiles: Vec::new(),
            environments: Vec::new(),
        }),
        Err(err) => Err(CliError::io(err.to_string())),
    }
}

pub fn resolve_connection(
    store: &StoreFile,
    name_or_id: &str,
) -> Result<ConnectionProfile, CliError> {
    if let Ok(id) = uuid::Uuid::parse_str(name_or_id)
        && let Some(profile) = store.profiles.iter().find(|p| p.id.0 == id)
    {
        return Ok(profile.clone());
    }

    let exact: Vec<_> = store
        .profiles
        .iter()
        .filter(|p| p.name == name_or_id)
        .collect();
    if exact.len() == 1 {
        return Ok(exact[0].clone());
    }
    if exact.len() > 1 {
        return Err(CliError::usage(format!(
            "ambiguous connection '{name_or_id}': multiple profiles share that name"
        )));
    }

    let lower = name_or_id.to_ascii_lowercase();
    let ci: Vec<_> = store
        .profiles
        .iter()
        .filter(|p| p.name.to_ascii_lowercase() == lower)
        .collect();
    match ci.len() {
        0 => Err(
            CliError::not_found(format!("connection not found: {name_or_id}"))
                .with_hint("run: sqlly-cli connections list"),
        ),
        1 => Ok(ci[0].clone()),
        _ => {
            let names: Vec<_> = ci.iter().map(|p| p.name.as_str()).collect();
            Err(CliError::usage(format!(
                "ambiguous connection '{name_or_id}': matches {}",
                names.join(", ")
            )))
        }
    }
}

/// Returns true when the profile targets a production environment.
///
/// Replace with `sqlly_intel::query::safety::production::ProductionGuard` once plan 149 lands.
pub fn is_production(store: &StoreFile, profile: &ConnectionProfile) -> bool {
    if profile.environment == ConnectionEnvironment::Production {
        return true;
    }

    store.environments.iter().any(|env| {
        env.name.eq_ignore_ascii_case("production")
            && env
                .safety
                .get("is_production")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
    })
}

pub fn summary(profile: &ConnectionProfile) -> serde_json::Value {
    serde_json::json!({
        "id": profile.id.0,
        "name": profile.name,
        "engine": engine_label(profile.database_type),
        "server": profile.server,
        "port": profile.port,
        "database": profile.database,
        "environment": profile.environment,
        "auth_provider": auth_provider_label(&profile.authentication),
    })
}

pub fn apply_database_override(
    mut profile: ConnectionProfile,
    database: Option<String>,
) -> ConnectionProfile {
    if let Some(db) = database {
        profile.database = Some(db);
    }
    profile
}

pub fn effective_database(profile: &ConnectionProfile) -> Result<String, CliError> {
    if let Some(db) = profile.database.as_ref().filter(|d| !d.is_empty()) {
        return Ok(db.clone());
    }
    // The file engines (SQLite, DuckDB) hold one database whose schema is
    // `main`; libSQL / Turso is a network engine but serves one SQLite
    // database per endpoint, so its default is the same `main`.
    if matches!(
        profile.database_type,
        DatabaseType::Sqlite | DatabaseType::LibSql | DatabaseType::DuckDb
    ) {
        return Ok("main".to_string());
    }
    // ClickHouse sessions start in the `default` database.
    if profile.database_type == DatabaseType::ClickHouse {
        return Ok("default".to_string());
    }
    // Redis connections open on logical database 0.
    if profile.database_type.is_command_engine() {
        return Ok("0".to_string());
    }
    Err(CliError::usage(
        "connection has no database; pass --database",
    ))
}

fn engine_label(engine: DatabaseType) -> &'static str {
    match engine {
        DatabaseType::SqlServer => "SQL Server",
        DatabaseType::AzureSqlDatabase => "Azure SQL",
        DatabaseType::Postgres => "PostgreSQL",
        DatabaseType::MySql | DatabaseType::MariaDb => "MySQL/MariaDB",
        DatabaseType::Sqlite => "SQLite",
        other => other.display_name(),
    }
}

fn auth_provider_label(auth: &AuthenticationType) -> &'static str {
    match auth {
        AuthenticationType::SqlLogin { .. } => "sql",
        AuthenticationType::Integrated => "integrated",
        AuthenticationType::EntraDeviceCode => "entra-device-code",
        AuthenticationType::EntraBrowser => "entra-browser",
        AuthenticationType::AwsRdsIam { .. } => "aws-rds-iam",
        AuthenticationType::GcpCloudSqlIam { .. } => "gcp-cloud-sql-iam",
        AuthenticationType::AuthToken { .. } => "auth-token",
    }
}

trait CliErrorHint {
    fn with_hint(self, hint: impl Into<String>) -> Self;
}

impl CliErrorHint for CliError {
    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.body.hint = Some(hint.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    fn sample_profile(name: &str, env: ConnectionEnvironment) -> ConnectionProfile {
        let mut profile = ConnectionProfile::sql_login(
            name,
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::InMemory,
                key: "unit-test-secret-marker".to_string(),
            },
        );
        profile.name = name.to_string();
        profile.environment = env;
        profile
    }

    #[test]
    fn resolves_by_id_then_name() {
        let profile = sample_profile("dev", ConnectionEnvironment::Dev);
        let id = profile.id.0;
        let store = StoreFile {
            version: 1,
            profiles: vec![profile],
            environments: vec![],
        };
        assert_eq!(
            resolve_connection(&store, &id.to_string()).unwrap().name,
            "dev"
        );
        assert_eq!(resolve_connection(&store, "dev").unwrap().id.0, id);
    }

    #[test]
    fn ambiguous_case_insensitive_is_usage_error() {
        let store = StoreFile {
            version: 1,
            profiles: vec![
                sample_profile("Dev", ConnectionEnvironment::Dev),
                sample_profile("dev", ConnectionEnvironment::Test),
            ],
            environments: vec![],
        };
        let err = resolve_connection(&store, "DEV").unwrap_err();
        assert_eq!(err.exit, crate::agent::ExitCode::Usage);
    }

    #[test]
    fn missing_file_is_empty_store() {
        let path = std::env::temp_dir().join(format!(
            "sqlly-missing-store-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::remove_file(&path);
        let store = load_store(&path).unwrap();
        assert!(store.profiles.is_empty());
    }

    #[test]
    fn summary_has_no_secret_key() {
        let profile = sample_profile("dev", ConnectionEnvironment::Dev);
        let json = summary(&profile).to_string();
        assert!(!json.contains("unit-test-secret-marker"));
    }

    #[test]
    fn production_enum_flag() {
        let store = StoreFile {
            version: 1,
            profiles: vec![sample_profile("prod", ConnectionEnvironment::Production)],
            environments: vec![],
        };
        assert!(is_production(&store, &store.profiles[0]));
    }

    #[test]
    fn production_environment_name_fallback() {
        let store = StoreFile {
            version: 1,
            profiles: vec![sample_profile("dev", ConnectionEnvironment::Dev)],
            environments: vec![EnvironmentRow {
                id: uuid::Uuid::new_v4(),
                name: "Production".to_string(),
                safety: serde_json::json!({ "is_production": true }),
            }],
        };
        assert!(is_production(&store, &store.profiles[0]));
    }
}
