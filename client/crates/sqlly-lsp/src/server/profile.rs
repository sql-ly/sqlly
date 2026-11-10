//! `sqlly/setConnection` parameter parsing: map the client-supplied connection
//! params onto a full [`ConnectionProfile`], resolving the engine name and its
//! conventional default port.

use sqlly_core::ConnectionProfile;

/// Custom connection params for `sqlly/setConnection`.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ConnectionParams {
    server: String,
    /// Defaults to the engine's conventional port when omitted.
    port: Option<u16>,
    database: Option<String>,
    username: Option<String>,
    #[serde(default)]
    encrypt: bool,
    #[serde(default)]
    trust_server_certificate: bool,
    /// Engine name — `"sqlserver"`, `"postgres"`, `"mysql"`, `"sqlite"`, … —
    /// matched case-insensitively against the engine catalog. Without it the
    /// profile defaults to SQL Server, which silently made validation,
    /// completion syntax, and safety semantics T-SQL for every client that
    /// couldn't say otherwise.
    database_type: Option<String>,
}

/// Match a client-supplied engine name against [`DatabaseType::ALL`],
/// tolerating case and separator differences: `postgres`, `PostgreSQL`,
/// `sql-server`, and `SqlServer` all resolve.
///
/// [`DatabaseType::ALL`]: sqlly_lang::sqlmodel::DatabaseType::ALL
fn parse_database_type(name: &str) -> Option<sqlly_lang::sqlmodel::DatabaseType> {
    fn fold(s: &str) -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase()
    }
    let wanted = fold(name);
    sqlly_lang::sqlmodel::DatabaseType::ALL
        .into_iter()
        .find(|db| fold(&format!("{db:?}")) == wanted || fold(db.display_name()) == wanted)
}

/// The engine's conventional port, used when the client omits one.
fn default_port_for(db: sqlly_lang::sqlmodel::DatabaseType) -> u16 {
    use sqlly_lang::sqlmodel::DatabaseType::*;
    match db {
        SqlServer | AzureSqlDatabase => 1433,
        Postgres | CockroachDb | YugabyteDb => 5432,
        MySql | MariaDb | AmazonAurora | AmazonRds => 3306,
        Oracle => 1521,
        Db2 => 50000,
        // File-based and cloud-native engines have no meaningful default TCP
        // port here; 0 keeps the field honest instead of implying 1433.
        _ => 0,
    }
}

pub(super) fn build_profile(params: ConnectionParams) -> Result<ConnectionProfile, String> {
    use sqlly_core::{AuthenticationType, SecretProvider, SecretRef};
    let auth = match params.username {
        Some(username) => AuthenticationType::SqlLogin {
            username,
            secret_ref: SecretRef {
                provider: SecretProvider::Environment,
                key: "SQLLY_PASSWORD".to_string(),
            },
        },
        None => AuthenticationType::Integrated,
    };
    // An unrecognized engine name is an error, not a silent SQL Server
    // fallback — a client that says "postgres" and gets T-SQL validation is
    // exactly the failure this parameter exists to prevent.
    let database_type = match &params.database_type {
        Some(name) => parse_database_type(name).ok_or_else(|| {
            format!(
                "unknown databaseType {name:?}; expected one of {}",
                sqlly_lang::sqlmodel::DatabaseType::ALL
                    .iter()
                    .map(|db| format!("{db:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?,
        None => sqlly_lang::sqlmodel::DatabaseType::default(),
    };
    Ok(ConnectionProfile {
        id: sqlly_core::ConnectionId::new(),
        name: format!(
            "{} ({})",
            params.server,
            params.database.as_deref().unwrap_or("")
        ),
        server: params.server,
        port: params
            .port
            .unwrap_or_else(|| default_port_for(database_type)),
        database: params.database,
        database_type,
        environment: sqlly_core::ConnectionEnvironment::default(),
        client: None,
        project: None,
        authentication: auth,
        encrypt: params.encrypt,
        trust_server_certificate: params.trust_server_certificate,
        tls_mode: sqlly_core::TlsMode::Legacy,
        tls_ca_file: None,
        tls_client_cert_file: None,
        tls_client_key_file: None,
        timeout_seconds: 30,
        statement_timeout_seconds: None,
        application_name: "sqlly-lsp".to_string(),
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
        route: Default::default(),
        provenance: sqlly_core::ConnectionProvenance::default(),
        status: sqlly_core::ConnectionStatus::default(),
        ssh_tunnel: None,
        tunnel_profile: None,
        transport: sqlly_core::TransportChain::default(),
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
    })
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    fn params(database_type: Option<&str>, port: Option<u16>) -> ConnectionParams {
        ConnectionParams {
            server: "db.example".to_string(),
            port,
            database: Some("app".to_string()),
            username: Some("u".to_string()),
            encrypt: false,
            trust_server_certificate: false,
            database_type: database_type.map(str::to_string),
        }
    }

    /// `sqlly/setConnection` must honor the client's engine: the profile's
    /// database type drives validation dialect, completion syntax, and safety
    /// semantics, so defaulting everything to SQL Server made those features
    /// silently T-SQL for every other engine.
    #[test]
    fn set_connection_maps_database_type_and_default_port() {
        use sqlly_lang::sqlmodel::{DatabaseType, Dialect};
        let p = build_profile(params(Some("postgres"), None)).unwrap();
        assert_eq!(p.database_type, DatabaseType::Postgres);
        assert_eq!(p.port, 5432);
        assert_eq!(p.database_type.family(), Dialect::Postgres);

        // Spelling variants and explicit ports are honored.
        let p = build_profile(params(Some("PostgreSQL"), Some(6432))).unwrap();
        assert_eq!(p.database_type, DatabaseType::Postgres);
        assert_eq!(p.port, 6432);
        let p = build_profile(params(Some("sql-server"), None)).unwrap();
        assert_eq!(p.database_type, DatabaseType::SqlServer);
        assert_eq!(p.port, 1433);
        let p = build_profile(params(Some("mysql"), None)).unwrap();
        assert_eq!(p.database_type, DatabaseType::MySql);
        assert_eq!(p.port, 3306);
        let p = build_profile(params(Some("MariaDB"), None)).unwrap();
        assert_eq!(p.database_type, DatabaseType::MariaDb);

        // Omitted keeps the historical SQL Server default.
        let p = build_profile(params(None, None)).unwrap();
        assert_eq!(p.database_type, DatabaseType::SqlServer);

        // Unknown names error instead of silently validating as T-SQL.
        let err = build_profile(params(Some("mongodb"), None)).unwrap_err();
        assert!(err.contains("mongodb"), "{err}");
    }
}
