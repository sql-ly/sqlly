use sqlly_core::{AuthenticationType, DatabaseType};

/// Engines the desktop app can connect to with its bundled native drivers. The
/// first eleven are first-party; the rest are wire-compatible labels grouped
/// by family (see `DatabaseType::compatible_with`).
pub const SELECTABLE_ENGINES: [DatabaseType; 30] = [
    DatabaseType::SqlServer,
    DatabaseType::AzureSqlDatabase,
    DatabaseType::Postgres,
    DatabaseType::MySql,
    DatabaseType::MariaDb,
    DatabaseType::Sqlite,
    DatabaseType::LibSql,
    DatabaseType::DuckDb,
    DatabaseType::ClickHouse,
    DatabaseType::Oracle,
    DatabaseType::Redis,
    DatabaseType::CockroachDb,
    DatabaseType::YugabyteDb,
    DatabaseType::AmazonAuroraPostgres,
    DatabaseType::AmazonRdsPostgres,
    DatabaseType::AzurePostgresFlexible,
    DatabaseType::CloudSqlPostgres,
    DatabaseType::Neon,
    DatabaseType::Supabase,
    DatabaseType::AmazonAurora,
    DatabaseType::AmazonRds,
    DatabaseType::AzureMySqlFlexible,
    DatabaseType::CloudSqlMySql,
    DatabaseType::PlanetScale,
    DatabaseType::TiDb,
    DatabaseType::Valkey,
    DatabaseType::KeyDb,
    DatabaseType::DragonflyDb,
    DatabaseType::AmazonElastiCache,
    DatabaseType::AmazonMemoryDb,
];

pub fn label_for(engine: DatabaseType) -> String {
    let key = match engine.compatible_with() {
        Some(DatabaseType::Postgres) => "connections.editor.engine_postgresql_compatible",
        Some(DatabaseType::MySql) => "connections.editor.engine_mysql_compatible",
        Some(DatabaseType::Redis) => "connections.editor.engine_redis_compatible",
        _ => return engine.display_name().to_string(),
    };
    crate::localization::format(key, &[("engine_name", engine.display_name())])
}

pub fn labels() -> Vec<String> {
    SELECTABLE_ENGINES
        .iter()
        .map(|engine| label_for(*engine))
        .collect()
}

/// What an engine-picker entry stands for: one engine, or the Redis-family
/// "Auto-detect" entry that resolves the flavor from the host name and the
/// server (see [`sqlly_core::redis_flavor`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineChoice {
    AutoDetect,
    Engine(DatabaseType),
}

pub fn auto_detect_label() -> String {
    crate::localization::text("connections.editor.engine_redis_auto_detect")
}

/// The engine picker's entries: [`labels`] with the Redis-family
/// Auto-detect entry heading the Redis-compatible group.
pub fn picker_labels() -> Vec<String> {
    let mut labels = labels();
    let at = SELECTABLE_ENGINES
        .iter()
        .position(|engine| engine.compatible_with() == Some(DatabaseType::Redis))
        .unwrap_or(labels.len());
    labels.insert(at, auto_detect_label());
    labels
}

/// The picker entry a confirmed label names.
pub fn choice_from_label(label: &str) -> Option<EngineChoice> {
    if label == auto_detect_label() {
        return Some(EngineChoice::AutoDetect);
    }
    from_label(label).map(EngineChoice::Engine)
}

/// The label the picker shows for a profile's engine.
pub fn picker_label(engine: DatabaseType, auto_detect: bool) -> String {
    if auto_detect && engine.is_command_engine() {
        auto_detect_label()
    } else {
        label_for(engine)
    }
}

/// The flavor an Auto-detect profile holds while it is edited: the managed
/// service its host names, else what an earlier connect detected (`current`
/// when it is a self-hosted flavor), else plain Redis.
pub fn auto_detect_engine(server: &str, current: DatabaseType) -> DatabaseType {
    if let Some(service) = sqlly_core::redis_flavor::from_host(server) {
        return service;
    }
    match current {
        DatabaseType::Valkey | DatabaseType::KeyDb | DatabaseType::DragonflyDb => current,
        _ => DatabaseType::Redis,
    }
}

/// The TLS mode after Auto-detect lands on `next`: arriving on a managed
/// AWS cache lifts a disabled mode to Require (MemoryDB, ElastiCache
/// Serverless and IAM sign-in all need TLS); any mode the user chose that
/// already encrypts stays.
pub fn tls_after_auto_detect(
    mode: sqlly_core::TlsMode,
    previous: DatabaseType,
    next: DatabaseType,
) -> sqlly_core::TlsMode {
    let aws = matches!(
        next,
        DatabaseType::AmazonElastiCache | DatabaseType::AmazonMemoryDb
    );
    if aws && next != previous && mode == sqlly_core::TlsMode::Disable {
        sqlly_core::TlsMode::Require
    } else {
        mode
    }
}

/// The dim line under the picker while Auto-detect is chosen: the flavor
/// detected so far, or what will be detected on connect.
pub fn auto_detect_hint(engine: DatabaseType) -> String {
    if engine == DatabaseType::Redis {
        return crate::localization::text("connections.editor.engine_auto_detect_hint");
    }
    let key = if engine.requires_tls() {
        "connections.editor.engine_auto_detected_tls"
    } else {
        "connections.editor.engine_auto_detected"
    };
    crate::localization::format(key, &[("engine_name", engine.display_name())])
}

/// The engine a picker label names (the localized [`label_for`] text, or
/// the bare display name).
pub fn from_label(label: &str) -> Option<DatabaseType> {
    SELECTABLE_ENGINES
        .into_iter()
        .find(|engine| label_for(*engine) == label || engine.display_name() == label)
}

pub fn default_port(engine: DatabaseType) -> u16 {
    match engine {
        DatabaseType::CockroachDb => 26257,
        DatabaseType::YugabyteDb => 5433,
        DatabaseType::TiDb => 4000,
        DatabaseType::ClickHouse => 8123,
        DatabaseType::Oracle => 1521,
        // sqld's port; a pasted libsql:// or https:// URL sets 443 itself.
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

/// Engines that open a local file (or `:memory:`) instead of dialing a
/// server: no port, no credentials, the Server field holds the path.
pub fn is_file_engine(engine: DatabaseType) -> bool {
    matches!(engine, DatabaseType::Sqlite | DatabaseType::DuckDb)
}

/// The credentials-row note for a file engine.
pub fn file_engine_credentials_note(engine: DatabaseType) -> String {
    if engine == DatabaseType::DuckDb {
        return crate::localization::text("connections.editor.duckdb_credentials_note");
    }
    crate::localization::format(
        "connections.editor.file_engine_credentials_note",
        &[("engine_name", engine.display_name())],
    )
}

/// Placeholder for the Database field: Oracle needs a service name (or a
/// `SID:` prefixed SID) there, everywhere else it is optional.
pub fn database_placeholder(engine: DatabaseType) -> String {
    crate::localization::text(match engine {
        DatabaseType::Oracle => "connections.editor.oracle_database_placeholder",
        _ => "connections.editor.optional_database_placeholder",
    })
}

/// Engines whose editors offer the four-way TLS mode select (plus CA file and
/// client certificate rows) instead of the two legacy checkboxes: everything
/// speaking the PostgreSQL or MySQL wire protocol, ClickHouse and libSQL
/// over HTTPS (same rustls configuration), and the Redis family
/// (`rediss://`). SQL Server keeps its checkboxes (tiberius TLS is
/// separate).
pub fn uses_tls_modes(engine: DatabaseType) -> bool {
    use sqlly_core::ConnectionKind;
    matches!(
        engine.connection_kind(),
        ConnectionKind::PostgresWire
            | ConnectionKind::MySqlWire
            | ConnectionKind::ClickHouseHttp
            | ConnectionKind::HranaHttp
            | ConnectionKind::Resp
    )
}

/// Engines that sign in with a bearer token instead of a user name and
/// password: libSQL / Turso. The editor shows one "Auth token" field.
pub fn uses_auth_token(engine: DatabaseType) -> bool {
    engine.connection_kind() == sqlly_core::ConnectionKind::HranaHttp
}

/// Engines whose connection editor shows the key-browser delimiter field:
/// the Redis family, whose "schema" is the naming convention of its keys.
pub fn uses_key_delimiter(engine: DatabaseType) -> bool {
    engine.is_command_engine()
}

/// The editor's TLS state — `(mode, encrypt, trust_server_certificate)` —
/// after a pasted connection string is applied over `current`. An explicit
/// mode in the string wins (`sslmode=verify-ca`, `rediss://` meaning
/// Require, `redis://` meaning Disable). A `rediss://` paste never lowers a
/// form that already verifies the certificate (Verify CA / Verify full):
/// the scheme only says "TLS", so the stronger setting the user chose
/// stays.
pub fn tls_after_paste(
    current: (sqlly_core::TlsMode, bool, bool),
    parsed: &sqlly_core::ParsedConnectionUrl,
) -> (sqlly_core::TlsMode, bool, bool) {
    use sqlly_core::TlsMode;
    let (mut mode, mut encrypt, mut trust) = current;
    let verifying = matches!(current.0, TlsMode::VerifyCa | TlsMode::VerifyFull);
    if parsed.engine.is_command_engine() && verifying && parsed.tls_mode == Some(TlsMode::Require) {
        return current;
    }
    if let Some(e) = parsed.encrypt {
        encrypt = e;
        // Keep the select coherent with the legacy flag when the string
        // carried no explicit sslmode word.
        mode = if e {
            TlsMode::VerifyFull
        } else {
            TlsMode::Disable
        };
    }
    if let Some(t) = parsed.trust_server_certificate {
        trust = t;
    }
    if let Some(explicit) = parsed.tls_mode {
        mode = explicit;
        (encrypt, trust) = explicit.legacy_flags();
    }
    (mode, encrypt, trust)
}

pub fn derived_connection_name(parsed: &sqlly_core::ParsedConnectionUrl) -> String {
    if let Some(database) = &parsed.database {
        if parsed.server.is_empty() {
            database.clone()
        } else {
            format!("{database} @ {}", parsed.server)
        }
    } else if !parsed.server.is_empty() {
        parsed.server.clone()
    } else {
        crate::localization::text("connections.editor.derived_connection_name")
    }
}

/// When switching to a TLS-required engine, keep encryption enabled.
pub fn encrypt_after_engine_change(current: bool, next: DatabaseType) -> bool {
    current || next.requires_tls()
}

pub fn authentication_supported(engine: DatabaseType, auth: &AuthenticationType) -> bool {
    match auth {
        AuthenticationType::SqlLogin { .. } => !is_file_engine(engine) && !uses_auth_token(engine),
        AuthenticationType::AuthToken { .. } => uses_auth_token(engine),
        AuthenticationType::Integrated => engine == DatabaseType::SqlServer,
        // Redis: Azure Cache for Redis and Azure Managed Redis accept Entra
        // access tokens (they are Redis-typed profiles on Azure hosts).
        AuthenticationType::EntraBrowser | AuthenticationType::EntraDeviceCode => matches!(
            engine,
            DatabaseType::SqlServer | DatabaseType::AzureSqlDatabase | DatabaseType::Redis
        ),
        // AWS IAM: RDS/Aurora (PostgreSQL, MySQL) and ElastiCache / MemoryDB
        // (Redis). Google IAM: Cloud SQL only.
        AuthenticationType::AwsRdsIam { .. } => matches!(
            engine.connection_kind(),
            sqlly_lang::sqlmodel::ConnectionKind::PostgresWire
                | sqlly_lang::sqlmodel::ConnectionKind::MySqlWire
                | sqlly_lang::sqlmodel::ConnectionKind::Resp
        ),
        AuthenticationType::GcpCloudSqlIam { .. } => matches!(
            engine.connection_kind(),
            sqlly_lang::sqlmodel::ConnectionKind::PostgresWire
                | sqlly_lang::sqlmodel::ConnectionKind::MySqlWire
        ),
    }
}

/// Preserve a custom port while replacing an untouched engine default.
pub fn port_after_engine_change(
    current_port: &str,
    previous: DatabaseType,
    next: DatabaseType,
) -> String {
    if current_port.trim().is_empty()
        || current_port.trim() == default_port(previous).to_string()
        || is_file_engine(previous)
    {
        let port = default_port(next);
        if port == 0 {
            String::new()
        } else {
            port.to_string()
        }
    } else {
        current_port.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{SecretProvider, SecretRef};

    #[test]
    fn every_engine_label_resolves_back_to_its_engine() {
        let labels = labels();
        assert_eq!(labels.len(), SELECTABLE_ENGINES.len());
        for (engine, label) in SELECTABLE_ENGINES.iter().zip(&labels) {
            assert_eq!(from_label(label), Some(*engine));
        }
        assert_eq!(from_label("not an engine"), None);
    }

    #[test]
    fn auto_detect_heads_the_redis_compatible_group() {
        let labels = picker_labels();
        assert_eq!(labels.len(), SELECTABLE_ENGINES.len() + 1);
        let at = labels
            .iter()
            .position(|l| *l == auto_detect_label())
            .unwrap();
        assert_eq!(labels[at + 1], label_for(DatabaseType::Valkey));
        assert_eq!(
            choice_from_label(&labels[at]),
            Some(EngineChoice::AutoDetect)
        );
        assert_eq!(
            choice_from_label(&label_for(DatabaseType::KeyDb)),
            Some(EngineChoice::Engine(DatabaseType::KeyDb))
        );
        assert!(
            labels
                .iter()
                .filter(|l| l.to_lowercase().contains("auto"))
                .any(|l| *l == auto_detect_label())
        );
    }

    #[test]
    fn auto_detect_follows_the_host_then_keeps_a_detected_flavor() {
        let elasticache = "clustercfg.a.b.use1.cache.amazonaws.com";
        assert_eq!(
            auto_detect_engine(elasticache, DatabaseType::Redis),
            DatabaseType::AmazonElastiCache
        );
        // Leaving the AWS host drops the managed service again.
        assert_eq!(
            auto_detect_engine("10.0.0.5", DatabaseType::AmazonElastiCache),
            DatabaseType::Redis
        );
        // A flavor an earlier connect detected survives editing.
        assert_eq!(
            auto_detect_engine("10.0.0.5", DatabaseType::Valkey),
            DatabaseType::Valkey
        );
        assert_eq!(
            picker_label(DatabaseType::Valkey, true),
            auto_detect_label()
        );
        assert_eq!(
            picker_label(DatabaseType::Postgres, true),
            label_for(DatabaseType::Postgres)
        );
    }

    #[test]
    fn auto_detect_onto_aws_lifts_disabled_tls_only() {
        use sqlly_core::TlsMode;
        let (redis, ec) = (DatabaseType::Redis, DatabaseType::AmazonElastiCache);
        assert_eq!(
            tls_after_auto_detect(TlsMode::Disable, redis, ec),
            TlsMode::Require
        );
        assert_eq!(
            tls_after_auto_detect(TlsMode::Disable, redis, DatabaseType::AmazonMemoryDb),
            TlsMode::Require
        );
        assert_eq!(
            tls_after_auto_detect(TlsMode::VerifyFull, redis, ec),
            TlsMode::VerifyFull
        );
        // Re-detecting the same service leaves a deliberate choice alone.
        assert_eq!(
            tls_after_auto_detect(TlsMode::Disable, ec, ec),
            TlsMode::Disable
        );
        assert_eq!(
            tls_after_auto_detect(TlsMode::Disable, ec, DatabaseType::Redis),
            TlsMode::Disable
        );
    }

    #[test]
    fn auto_detect_hint_names_tls_for_aws_services() {
        assert!(auto_detect_hint(DatabaseType::AmazonMemoryDb).contains("TLS"));
        assert!(!auto_detect_hint(DatabaseType::AmazonElastiCache).contains("TLS"));
        assert!(!auto_detect_hint(DatabaseType::Valkey).contains("TLS"));
        assert!(auto_detect_hint(DatabaseType::Redis).contains("connect"));
    }

    #[test]
    fn engine_search_is_case_insensitive_contains() {
        use gpui_component::searchable_list::SearchableListItem as _;
        let matching: Vec<_> = labels()
            .into_iter()
            .filter(|label| label.matches("memorydb"))
            .collect();
        assert_eq!(matching, vec![label_for(DatabaseType::AmazonMemoryDb)]);
        assert!(label_for(DatabaseType::Valkey).matches("VALK"));
    }

    #[test]
    fn a_rediss_url_pasted_over_tls_disable_ends_with_require() {
        use sqlly_core::TlsMode;
        let parsed =
            sqlly_core::parse_connection_url("rediss://app:pw@cache.example.com:6380/2").unwrap();
        assert_eq!(parsed.engine, DatabaseType::Redis);
        let (mode, encrypt, trust) = tls_after_paste((TlsMode::Disable, false, false), &parsed);
        assert_eq!(mode, TlsMode::Require);
        assert!(encrypt && trust, "the legacy flags follow the mode");
        // The valkeys spelling and a bare host behave the same.
        let parsed = sqlly_core::parse_connection_url("valkeys://cache.example.com").unwrap();
        assert_eq!(
            tls_after_paste((TlsMode::Disable, false, false), &parsed).0,
            TlsMode::Require
        );
    }

    #[test]
    fn a_rediss_paste_never_lowers_a_verifying_mode_and_redis_means_plaintext() {
        use sqlly_core::TlsMode;
        let tls = sqlly_core::parse_connection_url("rediss://cache.example.com").unwrap();
        for mode in [TlsMode::VerifyCa, TlsMode::VerifyFull] {
            let (encrypt, trust) = mode.legacy_flags();
            assert_eq!(
                tls_after_paste((mode, encrypt, trust), &tls),
                (mode, encrypt, trust),
                "{mode:?} kept"
            );
        }
        assert_eq!(
            tls_after_paste((TlsMode::Require, true, true), &tls).0,
            TlsMode::Require
        );
        // `redis://` is an explicit request for plaintext.
        let plain = sqlly_core::parse_connection_url("redis://cache.example.com").unwrap();
        assert_eq!(
            tls_after_paste((TlsMode::VerifyFull, true, false), &plain),
            (TlsMode::Disable, false, false)
        );
        // Other engines keep their explicit sslmode words.
        let pg = sqlly_core::parse_connection_url("postgres://h/db?sslmode=require").unwrap();
        assert_eq!(
            tls_after_paste((TlsMode::VerifyFull, true, false), &pg).0,
            TlsMode::Require
        );
    }

    fn sql_login() -> AuthenticationType {
        AuthenticationType::SqlLogin {
            username: "user".into(),
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "secret".into(),
            },
        }
    }

    #[test]
    fn derived_name_from_parsed_url() {
        let parsed = sqlly_core::ParsedConnectionUrl {
            server: "localhost".into(),
            database: Some("appdb".into()),
            ..Default::default()
        };
        assert_eq!(derived_connection_name(&parsed), "appdb @ localhost");
    }

    #[test]
    fn selectable_engines_are_exactly_the_native_driver_set() {
        assert!(
            SELECTABLE_ENGINES
                .iter()
                .all(|engine| engine.has_native_driver())
        );
        assert_eq!(from_label("PostgreSQL"), Some(DatabaseType::Postgres));
        assert_eq!(from_label("Oracle Database"), Some(DatabaseType::Oracle));
        assert_eq!(from_label("IBM Db2"), None);
        assert_eq!(
            SELECTABLE_ENGINES.len(),
            DatabaseType::ALL
                .iter()
                .filter(|engine| engine.has_native_driver())
                .count()
        );
        let mut labels_seen = std::collections::BTreeSet::new();
        for engine in SELECTABLE_ENGINES {
            assert_eq!(from_label(&label_for(engine)), Some(engine));
            assert_eq!(from_label(engine.display_name()), Some(engine));
            assert!(labels_seen.insert(label_for(engine)));
        }
    }

    #[test]
    fn default_ports_follow_documented_engine_ports() {
        assert_eq!(default_port(DatabaseType::CockroachDb), 26257);
        assert_eq!(default_port(DatabaseType::YugabyteDb), 5433);
        assert_eq!(default_port(DatabaseType::TiDb), 4000);
        assert_eq!(default_port(DatabaseType::Neon), 5432);
        assert_eq!(default_port(DatabaseType::PlanetScale), 3306);
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::CockroachDb),
            "26257"
        );
    }

    #[test]
    fn encrypt_after_engine_change_for_tls_required_engines() {
        assert!(encrypt_after_engine_change(false, DatabaseType::Neon));
        assert!(encrypt_after_engine_change(true, DatabaseType::Postgres));
        assert!(!encrypt_after_engine_change(false, DatabaseType::Postgres));
    }

    #[test]
    fn ports_follow_engine_defaults_without_overwriting_custom_values() {
        assert_eq!(
            port_after_engine_change("1433", DatabaseType::SqlServer, DatabaseType::Postgres),
            "5432"
        );
        assert_eq!(
            port_after_engine_change("15433", DatabaseType::SqlServer, DatabaseType::Postgres),
            "15433"
        );
        assert_eq!(
            port_after_engine_change("", DatabaseType::Sqlite, DatabaseType::MySql),
            "3306"
        );
    }

    #[test]
    fn duckdb_is_a_selectable_file_engine() {
        assert!(is_file_engine(DatabaseType::DuckDb));
        assert_eq!(default_port(DatabaseType::DuckDb), 0);
        assert!(labels().contains(&"DuckDB".to_string()));
        assert_eq!(from_label("DuckDB"), Some(DatabaseType::DuckDb));
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::DuckDb),
            ""
        );
        assert!(file_engine_credentials_note(DatabaseType::DuckDb).contains(":memory:"));
        assert!(file_engine_credentials_note(DatabaseType::Sqlite).starts_with("SQLite"));
    }

    #[test]
    fn clickhouse_is_a_selectable_network_engine_on_8123() {
        assert!(!is_file_engine(DatabaseType::ClickHouse));
        assert_eq!(default_port(DatabaseType::ClickHouse), 8123);
        assert!(labels().contains(&"ClickHouse".to_string()));
        assert_eq!(from_label("ClickHouse"), Some(DatabaseType::ClickHouse));
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::ClickHouse),
            "8123"
        );
        assert!(uses_tls_modes(DatabaseType::ClickHouse));
        assert!(authentication_supported(
            DatabaseType::ClickHouse,
            &sql_login()
        ));
        assert!(!authentication_supported(
            DatabaseType::ClickHouse,
            &AuthenticationType::Integrated
        ));
    }

    #[test]
    fn oracle_is_a_selectable_network_engine_on_1521() {
        assert!(!is_file_engine(DatabaseType::Oracle));
        assert_eq!(default_port(DatabaseType::Oracle), 1521);
        assert!(labels().contains(&"Oracle Database".to_string()));
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::Oracle),
            "1521"
        );
        assert!(!uses_tls_modes(DatabaseType::Oracle));
        assert!(database_placeholder(DatabaseType::Oracle).contains("SID:"));
        assert_eq!(database_placeholder(DatabaseType::Postgres), "(optional)");
        assert!(authentication_supported(DatabaseType::Oracle, &sql_login()));
        assert!(!authentication_supported(
            DatabaseType::Oracle,
            &AuthenticationType::Integrated
        ));
    }

    #[test]
    fn libsql_is_a_selectable_token_engine_on_8080() {
        assert!(!is_file_engine(DatabaseType::LibSql));
        assert!(uses_auth_token(DatabaseType::LibSql));
        assert!(!uses_auth_token(DatabaseType::Sqlite));
        assert!(!uses_auth_token(DatabaseType::ClickHouse));
        assert_eq!(default_port(DatabaseType::LibSql), 8080);
        assert!(labels().contains(&"libSQL / Turso".to_string()));
        assert_eq!(from_label("libSQL / Turso"), Some(DatabaseType::LibSql));
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::LibSql),
            "8080"
        );
        assert!(uses_tls_modes(DatabaseType::LibSql));
        let token = AuthenticationType::AuthToken {
            secret_ref: SecretRef {
                provider: SecretProvider::InMemory,
                key: "tok".into(),
            },
        };
        assert!(authentication_supported(DatabaseType::LibSql, &token));
        assert!(!authentication_supported(
            DatabaseType::LibSql,
            &sql_login()
        ));
        assert!(!authentication_supported(DatabaseType::Postgres, &token));
        assert!(!authentication_supported(
            DatabaseType::LibSql,
            &AuthenticationType::Integrated
        ));
    }

    #[test]
    fn redis_family_is_selectable_on_6379_with_tls_modes_and_a_delimiter() {
        for engine in [
            DatabaseType::Redis,
            DatabaseType::Valkey,
            DatabaseType::KeyDb,
            DatabaseType::DragonflyDb,
            DatabaseType::AmazonElastiCache,
            DatabaseType::AmazonMemoryDb,
        ] {
            assert!(!is_file_engine(engine), "{engine:?}");
            assert_eq!(default_port(engine), 6379);
            assert!(uses_tls_modes(engine));
            assert!(uses_key_delimiter(engine));
            assert!(authentication_supported(engine, &sql_login()));
            assert!(!authentication_supported(
                engine,
                &AuthenticationType::Integrated
            ));
            assert_eq!(from_label(&label_for(engine)), Some(engine));
        }
        assert_eq!(from_label("Redis"), Some(DatabaseType::Redis));
        assert_eq!(label_for(DatabaseType::Valkey), "Redis-compatible · Valkey");
        assert_eq!(
            port_after_engine_change("5432", DatabaseType::Postgres, DatabaseType::Redis),
            "6379"
        );
        assert!(!uses_key_delimiter(DatabaseType::Postgres));
    }

    #[test]
    fn authentication_matrix_matches_driver_capabilities() {
        assert!(authentication_supported(
            DatabaseType::Postgres,
            &sql_login()
        ));
        assert!(!authentication_supported(
            DatabaseType::Sqlite,
            &sql_login()
        ));
        assert!(authentication_supported(
            DatabaseType::SqlServer,
            &AuthenticationType::Integrated
        ));
        assert!(!authentication_supported(
            DatabaseType::MySql,
            &AuthenticationType::Integrated
        ));
        assert!(authentication_supported(
            DatabaseType::AzureSqlDatabase,
            &AuthenticationType::EntraBrowser
        ));
        // Azure Redis signs in with Entra; the other Redis-protocol services
        // (Valkey, ElastiCache, MemoryDB, …) do not.
        assert!(authentication_supported(
            DatabaseType::Redis,
            &AuthenticationType::EntraBrowser
        ));
        assert!(authentication_supported(
            DatabaseType::Redis,
            &AuthenticationType::EntraDeviceCode
        ));
        assert!(!authentication_supported(
            DatabaseType::AmazonElastiCache,
            &AuthenticationType::EntraBrowser
        ));
        assert!(!authentication_supported(
            DatabaseType::Valkey,
            &AuthenticationType::EntraBrowser
        ));
        assert!(authentication_supported(
            DatabaseType::Postgres,
            &AuthenticationType::AwsRdsIam {
                username: "jane".into(),
                region: "us-west-2".into(),
                profile: None,
            }
        ));
        assert!(!authentication_supported(
            DatabaseType::SqlServer,
            &AuthenticationType::GcpCloudSqlIam {
                username: "alice".into(),
            }
        ));
        // ElastiCache / MemoryDB IAM: AWS IAM on every Redis-protocol label;
        // Google IAM never (Cloud SQL only).
        let aws_iam = AuthenticationType::AwsRdsIam {
            username: "iam-user".into(),
            region: "us-east-1".into(),
            profile: None,
        };
        assert!(authentication_supported(DatabaseType::Redis, &aws_iam));
        assert!(authentication_supported(
            DatabaseType::AmazonElastiCache,
            &aws_iam
        ));
        assert!(authentication_supported(
            DatabaseType::AmazonMemoryDb,
            &aws_iam
        ));
        assert!(!authentication_supported(
            DatabaseType::Redis,
            &AuthenticationType::GcpCloudSqlIam {
                username: "alice".into(),
            }
        ));
    }
}
