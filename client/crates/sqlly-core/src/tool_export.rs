//! Write SQLLY connections in other database tools' documented connection
//! formats, so a setup can move out of SQLLY as easily as it moves in (see
//! [`crate::tool_import`]).
//!
//! Every format is written without secrets: user names travel, passwords,
//! tokens and passphrases never do. Connections whose engine the target tool
//! cannot hold are reported in [`ToolExport::skipped`].

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;

use serde_json::{Map, Value, json};
use sqlly_lang::sqlmodel::{ConnectionKind, DatabaseType};
use uuid::Uuid;

use crate::connection::{ConnectionEnvironment, TlsMode};
use crate::profile_store::{ExportedAuthentication, ExportedProfile, ExportedSshAuth};
use crate::tool_import::SkippedConnection;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolExportFormat {
    /// pgAdmin 4 `servers.json` (Tools ▸ Import/Export Servers).
    PgAdminServersJson,
    /// Azure Data Studio `settings.json` connections, with a VS Code
    /// `mssql.connections` copy of the SQL Server entries.
    AzureDataStudioSettingsJson,
    /// PostgreSQL connection service file.
    PgServiceConf,
    /// DBeaver `data-sources.json`.
    DbeaverDataSourcesJson,
    /// JetBrains (DataGrip, IntelliJ) `dataSources.xml`.
    JetBrainsDataSourcesXml,
    /// DbGate connection environment variables.
    DbGateEnv,
}

impl ToolExportFormat {
    pub const ALL: [ToolExportFormat; 6] = [
        ToolExportFormat::PgAdminServersJson,
        ToolExportFormat::AzureDataStudioSettingsJson,
        ToolExportFormat::PgServiceConf,
        ToolExportFormat::DbeaverDataSourcesJson,
        ToolExportFormat::JetBrainsDataSourcesXml,
        ToolExportFormat::DbGateEnv,
    ];

    /// Suggested file name for the save dialog.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::PgAdminServersJson => "servers.json",
            Self::AzureDataStudioSettingsJson => "settings.json",
            Self::PgServiceConf => "pg_service.conf",
            Self::DbeaverDataSourcesJson => "data-sources.json",
            Self::JetBrainsDataSourcesXml => "dataSources.xml",
            Self::DbGateEnv => "dbgate-connections.env",
        }
    }

    /// Stable, untranslated identifier.
    pub fn id(self) -> &'static str {
        match self {
            Self::PgAdminServersJson => "pgadmin",
            Self::AzureDataStudioSettingsJson => "azure-data-studio",
            Self::PgServiceConf => "pg-service",
            Self::DbeaverDataSourcesJson => "dbeaver",
            Self::JetBrainsDataSourcesXml => "jetbrains",
            Self::DbGateEnv => "dbgate",
        }
    }

    /// The target tool's product name (a proper name, never translated).
    pub fn tool_name(self) -> &'static str {
        match self {
            Self::PgAdminServersJson => "pgAdmin",
            Self::AzureDataStudioSettingsJson => "Azure Data Studio / VS Code",
            Self::PgServiceConf => "psql / libpq",
            Self::DbeaverDataSourcesJson => "DBeaver",
            Self::JetBrainsDataSourcesXml => "DataGrip",
            Self::DbGateEnv => "DbGate",
        }
    }

    /// Whether the target tool can hold a connection of this engine.
    pub fn supports(self, engine: DatabaseType) -> bool {
        let kind = engine.connection_kind();
        match self {
            Self::PgAdminServersJson | Self::PgServiceConf => kind == ConnectionKind::PostgresWire,
            Self::AzureDataStudioSettingsJson => matches!(
                kind,
                ConnectionKind::Tds | ConnectionKind::PostgresWire | ConnectionKind::MySqlWire
            ),
            Self::DbeaverDataSourcesJson | Self::JetBrainsDataSourcesXml => {
                jdbc_target(engine).is_some()
            }
            Self::DbGateEnv => dbgate_engine(engine).is_some(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExport {
    pub contents: String,
    pub exported: usize,
    pub skipped: Vec<SkippedConnection>,
}

pub fn export_for_tool(format: ToolExportFormat, profiles: &[ExportedProfile]) -> ToolExport {
    let mut sorted: Vec<&ExportedProfile> = profiles.iter().collect();
    sorted.sort_by(|a, b| a.name.cmp(&b.name));
    let mut skipped = Vec::new();
    let supported: Vec<&ExportedProfile> = sorted
        .into_iter()
        .filter(|p| {
            let ok = format.supports(p.database_type);
            if !ok {
                skipped.push(SkippedConnection {
                    name: p.name.clone(),
                    reason: crate::DisplayMessage::new(
                        "services.tool_export.skip_reasons.engine_unsupported",
                        format!(
                            "{} cannot hold {} connections",
                            format.tool_name(),
                            p.database_type.display_name()
                        ),
                    )
                    .with_arg("tool", format.tool_name())
                    .with_arg("engine_name", p.database_type.display_name()),
                });
            }
            ok
        })
        .collect();
    let contents = match format {
        ToolExportFormat::PgAdminServersJson => pgadmin(&supported),
        ToolExportFormat::AzureDataStudioSettingsJson => azure_data_studio(&supported),
        ToolExportFormat::PgServiceConf => pg_service(&supported),
        ToolExportFormat::DbeaverDataSourcesJson => dbeaver(&supported),
        ToolExportFormat::JetBrainsDataSourcesXml => jetbrains(&supported),
        ToolExportFormat::DbGateEnv => dbgate(&supported),
    };
    ToolExport {
        contents,
        exported: supported.len(),
        skipped,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers

fn user_name(profile: &ExportedProfile) -> Option<&str> {
    match &profile.authentication {
        ExportedAuthentication::SqlLogin { username }
        | ExportedAuthentication::AwsRdsIam { username, .. }
        | ExportedAuthentication::GcpCloudSqlIam { username } => {
            Some(username.as_str()).filter(|u| !u.is_empty())
        }
        _ => None,
    }
}

fn folder(profile: &ExportedProfile) -> Option<&str> {
    profile
        .project
        .as_deref()
        .or(profile.client.as_deref())
        .filter(|g| !g.is_empty())
}

/// The TLS mode a profile actually uses: the explicit mode, or the one the
/// two legacy switches imply.
fn effective_tls(profile: &ExportedProfile) -> Option<TlsMode> {
    match profile.tls_mode {
        TlsMode::Legacy if profile.encrypt && profile.trust_server_certificate => {
            Some(TlsMode::Require)
        }
        TlsMode::Legacy if profile.encrypt => Some(TlsMode::VerifyFull),
        TlsMode::Legacy => None,
        mode => Some(mode),
    }
}

fn libpq_sslmode(profile: &ExportedProfile) -> &'static str {
    match effective_tls(profile) {
        Some(TlsMode::Disable) => "disable",
        Some(TlsMode::Require) => "require",
        Some(TlsMode::VerifyCa) => "verify-ca",
        Some(TlsMode::VerifyFull) => "verify-full",
        Some(TlsMode::Legacy) | None => "prefer",
    }
}

fn path_text(path: &Option<std::path::PathBuf>) -> Option<String> {
    path.as_ref().map(|p| p.to_string_lossy().into_owned())
}

/// A stable id per (format, position, name), so the same export writes the
/// same file twice.
fn stable_uuid(format: &str, index: usize, name: &str) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("sqlly-export:{format}:{index}:{name}").as_bytes(),
    )
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".into());
    text.push('\n');
    text
}

// ---------------------------------------------------------------------------
// pgAdmin 4

fn pgadmin(profiles: &[&ExportedProfile]) -> String {
    let mut servers = Map::new();
    for (index, p) in profiles.iter().enumerate() {
        let sslmode = libpq_sslmode(p);
        let mut params = Map::new();
        params.insert("sslmode".into(), json!(sslmode));
        params.insert("connect_timeout".into(), json!(p.timeout_seconds));
        for (key, path) in [
            ("sslrootcert", &p.tls_ca_file),
            ("sslcert", &p.tls_client_cert_file),
            ("sslkey", &p.tls_client_key_file),
        ] {
            if let Some(path) = path_text(path) {
                params.insert(key.into(), json!(path));
            }
        }
        let mut server = Map::new();
        server.insert("Name".into(), json!(p.name));
        server.insert(
            "Group".into(),
            json!(folder(p).unwrap_or(crate::tool_import::pgadmin::DEFAULT_GROUP)),
        );
        server.insert("Host".into(), json!(p.server));
        server.insert("Port".into(), json!(p.port));
        server.insert(
            "MaintenanceDB".into(),
            json!(p.database.as_deref().unwrap_or("postgres")),
        );
        server.insert("Username".into(), json!(user_name(p).unwrap_or("")));
        // pgAdmin 6 and earlier read the top-level key; 7+ read the parameters.
        server.insert("SSLMode".into(), json!(sslmode));
        server.insert("ConnectionParameters".into(), Value::Object(params));
        if let Some(tunnel) = &p.ssh_tunnel {
            let hop = &tunnel.target;
            server.insert("UseSSHTunnel".into(), json!(1));
            server.insert("TunnelHost".into(), json!(hop.host));
            server.insert("TunnelPort".into(), json!(hop.port.to_string()));
            server.insert("TunnelUsername".into(), json!(hop.username));
            match &hop.auth {
                ExportedSshAuth::KeyFile { path } => {
                    server.insert("TunnelAuthentication".into(), json!(1));
                    server.insert("TunnelIdentityFile".into(), json!(path));
                }
                ExportedSshAuth::Agent => {
                    server.insert("TunnelAuthentication".into(), json!(1));
                }
                ExportedSshAuth::Password => {
                    server.insert("TunnelAuthentication".into(), json!(0));
                }
            }
        }
        servers.insert((index + 1).to_string(), Value::Object(server));
    }
    pretty(&json!({ "Servers": servers }))
}

// ---------------------------------------------------------------------------
// Azure Data Studio / VS Code MSSQL

fn ads_provider(engine: DatabaseType) -> &'static str {
    match engine.connection_kind() {
        ConnectionKind::PostgresWire => "PGSQL",
        ConnectionKind::MySqlWire => "MySQL",
        _ => "MSSQL",
    }
}

fn ads_auth(profile: &ExportedProfile) -> &'static str {
    match profile.authentication {
        ExportedAuthentication::Integrated => "Integrated",
        ExportedAuthentication::EntraBrowser | ExportedAuthentication::EntraDeviceCode => {
            "AzureMFA"
        }
        _ => "SqlLogin",
    }
}

/// SQL Server tools take a non-default port as `host,port`.
fn mssql_server(profile: &ExportedProfile) -> String {
    if profile.port == 0 || profile.port == 1433 || profile.server.contains('\\') {
        profile.server.clone()
    } else {
        format!("{},{}", profile.server, profile.port)
    }
}

fn azure_data_studio(profiles: &[&ExportedProfile]) -> String {
    let root_id = stable_uuid("ads-group", 0, "ROOT")
        .to_string()
        .to_uppercase();
    let folders: BTreeSet<&str> = profiles.iter().filter_map(|p| folder(p)).collect();
    let mut groups = vec![json!({ "name": "ROOT", "id": root_id })];
    let group_id = |name: &str| stable_uuid("ads-group", 1, name).to_string().to_uppercase();
    for name in &folders {
        groups.push(json!({ "name": name, "id": group_id(name), "parentId": root_id }));
    }

    let mut connections = Vec::new();
    let mut vscode = Vec::new();
    for (index, p) in profiles.iter().enumerate() {
        let provider = ads_provider(p.database_type);
        let gid = folder(p).map(group_id).unwrap_or_else(|| root_id.clone());
        let user = user_name(p).unwrap_or("");
        let mut options = Map::new();
        options.insert("connectionName".into(), json!(p.name));
        if provider == "MSSQL" {
            let encrypt = if p.encrypt { "Mandatory" } else { "Optional" };
            options.insert("server".into(), json!(mssql_server(p)));
            options.insert(
                "database".into(),
                json!(p.database.as_deref().unwrap_or("")),
            );
            options.insert("authenticationType".into(), json!(ads_auth(p)));
            options.insert("user".into(), json!(user));
            options.insert("encrypt".into(), json!(encrypt));
            options.insert(
                "trustServerCertificate".into(),
                json!(p.trust_server_certificate),
            );
            vscode.push(json!({
                "profileName": p.name,
                "server": mssql_server(p),
                "database": p.database.as_deref().unwrap_or(""),
                "authenticationType": ads_auth(p),
                "user": user,
                "password": "",
                "savePassword": false,
                "emptyPasswordInput": false,
                "encrypt": encrypt,
                "trustServerCertificate": p.trust_server_certificate,
            }));
        } else {
            options.insert("host".into(), json!(p.server));
            options.insert("port".into(), json!(p.port));
            options.insert("dbname".into(), json!(p.database.as_deref().unwrap_or("")));
            options.insert("user".into(), json!(user));
            if provider == "PGSQL" {
                options.insert("sslmode".into(), json!(libpq_sslmode(p)));
            }
        }
        options.insert("password".into(), json!(""));
        options.insert("savePassword".into(), json!(false));
        options.insert("groupId".into(), json!(gid));
        connections.push(json!({
            "options": options,
            "groupId": gid,
            "providerName": provider,
            "savePassword": false,
            "id": stable_uuid("ads-connection", index, &p.name).to_string(),
        }));
    }
    let mut root = Map::new();
    root.insert(
        crate::tool_import::azure_data_studio::ADS_GROUPS.into(),
        Value::Array(groups),
    );
    root.insert(
        crate::tool_import::azure_data_studio::ADS_CONNECTIONS.into(),
        Value::Array(connections),
    );
    if !vscode.is_empty() {
        root.insert(
            crate::tool_import::azure_data_studio::VSCODE_CONNECTIONS.into(),
            Value::Array(vscode),
        );
    }
    pretty(&Value::Object(root))
}

// ---------------------------------------------------------------------------
// pg_service.conf

/// A service name libpq and shells accept: letters, digits, `_`, `-`, `.`.
fn identifier(name: &str, keep: impl Fn(char) -> bool, fallback: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if keep(c) {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        fallback.to_string()
    } else {
        out
    }
}

fn dedupe(base: String, used: &mut HashSet<String>) -> String {
    let mut candidate = base.clone();
    let mut n = 2;
    while !used.insert(candidate.to_ascii_lowercase()) {
        candidate = format!("{base}_{n}");
        n += 1;
    }
    candidate
}

fn single_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn pg_service(profiles: &[&ExportedProfile]) -> String {
    let mut out = String::from(
        "# PostgreSQL connection service file written by SQLLY.\n\
         # Save it as ~/.pg_service.conf (or point PGSERVICEFILE at it) and connect\n\
         # with: psql \"service=<name>\". Passwords are not included; keep them in\n\
         # ~/.pgpass or enter them when asked.\n",
    );
    let mut used = HashSet::new();
    for p in profiles {
        let service = dedupe(
            identifier(
                &p.name,
                |c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'),
                "service",
            ),
            &mut used,
        );
        let _ = writeln!(out, "\n[{service}]");
        let _ = writeln!(out, "host={}", single_line(&p.server));
        let _ = writeln!(out, "port={}", p.port);
        if let Some(db) = &p.database {
            let _ = writeln!(out, "dbname={}", single_line(db));
        }
        if let Some(user) = user_name(p) {
            let _ = writeln!(out, "user={}", single_line(user));
        }
        let _ = writeln!(out, "sslmode={}", libpq_sslmode(p));
        for (key, path) in [
            ("sslrootcert", &p.tls_ca_file),
            ("sslcert", &p.tls_client_cert_file),
            ("sslkey", &p.tls_client_key_file),
        ] {
            if let Some(path) = path_text(path) {
                let _ = writeln!(out, "{key}={}", single_line(&path));
            }
        }
        if p.timeout_seconds > 0 {
            let _ = writeln!(out, "connect_timeout={}", p.timeout_seconds);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// JDBC (DBeaver and JetBrains)

struct JdbcTarget {
    /// DBeaver provider and driver ids.
    dbeaver: (&'static str, &'static str),
    /// JetBrains driver-ref and driver class.
    jetbrains: (&'static str, &'static str),
}

fn jdbc_target(engine: DatabaseType) -> Option<JdbcTarget> {
    let target = |dbeaver, jetbrains| Some(JdbcTarget { dbeaver, jetbrains });
    match engine.connection_kind() {
        ConnectionKind::PostgresWire => target(
            ("postgresql", "postgres-jdbc"),
            ("postgresql", "org.postgresql.Driver"),
        ),
        ConnectionKind::MySqlWire if engine == DatabaseType::MariaDb => {
            target(("mysql", "mariaDB"), ("mariadb", "org.mariadb.jdbc.Driver"))
        }
        ConnectionKind::MySqlWire => {
            target(("mysql", "mysql8"), ("mysql.8", "com.mysql.cj.jdbc.Driver"))
        }
        ConnectionKind::Tds => target(
            ("sqlserver", "microsoft"),
            (
                "sqlserver.ms",
                "com.microsoft.sqlserver.jdbc.SQLServerDriver",
            ),
        ),
        ConnectionKind::SqliteFile => target(
            ("sqlite", "sqlite_jdbc"),
            ("sqlite.xerial", "org.sqlite.JDBC"),
        ),
        ConnectionKind::DuckDbFile => target(
            ("generic", "duckdb_jdbc"),
            ("duckdb", "org.duckdb.DuckDBDriver"),
        ),
        ConnectionKind::ClickHouseHttp => target(
            ("clickhouse", "com_clickhouse"),
            ("clickhouse", "com.clickhouse.jdbc.ClickHouseDriver"),
        ),
        _ => None,
    }
}

fn jdbc_url(p: &ExportedProfile) -> String {
    let db = p.database.as_deref().unwrap_or("");
    let host_port = format!("{}:{}", p.server, p.port);
    match p.database_type.connection_kind() {
        ConnectionKind::PostgresWire => {
            let mut url = format!("jdbc:postgresql://{host_port}/{db}");
            if effective_tls(p).is_some() {
                url.push_str(&format!("?sslmode={}", libpq_sslmode(p)));
            }
            url
        }
        ConnectionKind::MySqlWire if p.database_type == DatabaseType::MariaDb => {
            format!("jdbc:mariadb://{host_port}/{db}")
        }
        ConnectionKind::MySqlWire => format!("jdbc:mysql://{host_port}/{db}"),
        ConnectionKind::Tds => {
            let mut url = if p.server.contains('\\') {
                format!("jdbc:sqlserver://{}", p.server)
            } else {
                format!("jdbc:sqlserver://{host_port}")
            };
            if !db.is_empty() {
                url.push_str(&format!(";database={db}"));
            }
            if matches!(p.authentication, ExportedAuthentication::Integrated) {
                url.push_str(";integratedSecurity=true");
            }
            if p.encrypt {
                url.push_str(";encrypt=true");
            }
            if p.trust_server_certificate {
                url.push_str(";trustServerCertificate=true");
            }
            url
        }
        ConnectionKind::SqliteFile => format!("jdbc:sqlite:{}", p.server),
        ConnectionKind::DuckDbFile => format!("jdbc:duckdb:{}", p.server),
        ConnectionKind::ClickHouseHttp => format!("jdbc:clickhouse://{host_port}/{db}"),
        _ => String::new(),
    }
}

fn is_file_engine(engine: DatabaseType) -> bool {
    matches!(
        engine.connection_kind(),
        ConnectionKind::SqliteFile | ConnectionKind::DuckDbFile
    )
}

// ---------------------------------------------------------------------------
// DBeaver

fn dbeaver_type(environment: ConnectionEnvironment) -> Option<&'static str> {
    match environment {
        ConnectionEnvironment::Local | ConnectionEnvironment::Dev => Some("dev"),
        ConnectionEnvironment::Test | ConnectionEnvironment::Staging => Some("test"),
        ConnectionEnvironment::Production => Some("prod"),
        _ => None,
    }
}

fn dbeaver(profiles: &[&ExportedProfile]) -> String {
    let mut folders = Map::new();
    let mut connections = Map::new();
    for (index, p) in profiles.iter().enumerate() {
        let Some(target) = jdbc_target(p.database_type) else {
            continue;
        };
        let (provider, driver) = target.dbeaver;
        let mut config = Map::new();
        if is_file_engine(p.database_type) {
            config.insert("database".into(), json!(p.server));
        } else {
            config.insert("host".into(), json!(p.server));
            config.insert("port".into(), json!(p.port.to_string()));
            if let Some(db) = &p.database {
                config.insert("database".into(), json!(db));
            }
        }
        config.insert("url".into(), json!(jdbc_url(p)));
        config.insert("configurationType".into(), json!("MANUAL"));
        if let Some(kind) = dbeaver_type(p.environment) {
            config.insert("type".into(), json!(kind));
        }
        config.insert("auth-model".into(), json!("native"));
        if let Some(user) = user_name(p) {
            config.insert("user".into(), json!(user));
        }
        if let Some(tunnel) = &p.ssh_tunnel {
            let hop = &tunnel.target;
            let mut props = Map::new();
            props.insert("host".into(), json!(hop.host));
            props.insert("port".into(), json!(hop.port));
            match &hop.auth {
                ExportedSshAuth::KeyFile { path } => {
                    props.insert("authType".into(), json!("PUBLIC_KEY"));
                    props.insert("keyPath".into(), json!(path));
                }
                ExportedSshAuth::Agent => {
                    props.insert("authType".into(), json!("AGENT"));
                }
                ExportedSshAuth::Password => {
                    props.insert("authType".into(), json!("PASSWORD"));
                }
            }
            config.insert(
                "handlers".into(),
                json!({ "ssh_tunnel": {
                    "type": "TUNNEL",
                    "enabled": true,
                    "user": hop.username,
                    "save-password": false,
                    "properties": props,
                }}),
            );
        }
        let mut entry = Map::new();
        entry.insert("provider".into(), json!(provider));
        entry.insert("driver".into(), json!(driver));
        entry.insert("name".into(), json!(p.name));
        entry.insert("save-password".into(), json!(false));
        if p.block_all_but_select {
            entry.insert("read-only".into(), json!(true));
        }
        if let Some(name) = folder(p) {
            folders.insert(name.to_string(), json!({}));
            entry.insert("folder".into(), json!(name));
        }
        entry.insert("configuration".into(), Value::Object(config));
        let id = stable_uuid("dbeaver", index, &p.name).simple().to_string();
        connections.insert(format!("{driver}-{}", &id[..16]), Value::Object(entry));
    }
    pretty(&json!({ "folders": folders, "connections": connections }))
}

// ---------------------------------------------------------------------------
// JetBrains

fn xml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn jetbrains(profiles: &[&ExportedProfile]) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<project version=\"4\">\n  <component name=\"DataSourceManagerImpl\" format=\"xml\" multifile-model=\"true\">\n",
    );
    for (index, p) in profiles.iter().enumerate() {
        let Some(target) = jdbc_target(p.database_type) else {
            continue;
        };
        let (driver_ref, driver_class) = target.jetbrains;
        let group = folder(p)
            .map(|g| format!(" group=\"{}\"", xml_escape(g)))
            .unwrap_or_default();
        let read_only = if p.block_all_but_select {
            " read-only=\"true\""
        } else {
            ""
        };
        let _ = writeln!(
            out,
            "    <data-source source=\"LOCAL\" name=\"{}\"{group}{read_only} uuid=\"{}\">",
            xml_escape(&p.name),
            stable_uuid("jetbrains", index, &p.name)
        );
        let _ = writeln!(out, "      <driver-ref>{driver_ref}</driver-ref>");
        let _ = writeln!(out, "      <synchronize>true</synchronize>");
        let _ = writeln!(out, "      <jdbc-driver>{driver_class}</jdbc-driver>");
        let _ = writeln!(
            out,
            "      <jdbc-url>{}</jdbc-url>",
            xml_escape(&jdbc_url(p))
        );
        let _ = writeln!(out, "      <working-dir>$ProjectFileDir$</working-dir>");
        let _ = writeln!(out, "    </data-source>");
    }
    out.push_str("  </component>\n</project>\n");
    out
}

// ---------------------------------------------------------------------------
// DbGate

/// Only the engine ids DbGate documents for environment-variable
/// connections are written.
fn dbgate_engine(engine: DatabaseType) -> Option<&'static str> {
    Some(match engine.connection_kind() {
        ConnectionKind::PostgresWire if engine == DatabaseType::CockroachDb => {
            "cockroach@dbgate-plugin-postgres"
        }
        ConnectionKind::PostgresWire => "postgres@dbgate-plugin-postgres",
        ConnectionKind::MySqlWire if engine == DatabaseType::MariaDb => {
            "mariadb@dbgate-plugin-mysql"
        }
        ConnectionKind::MySqlWire => "mysql@dbgate-plugin-mysql",
        ConnectionKind::Tds => "mssql@dbgate-plugin-mssql",
        ConnectionKind::SqliteFile => "sqlite@dbgate-plugin-sqlite",
        ConnectionKind::Resp => "redis@dbgate-plugin-redis",
        _ => return None,
    })
}

fn env_value(value: &str) -> String {
    let value = single_line(value);
    if value.is_empty()
        || value
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '#' | '"' | '\'' | '$'))
    {
        format!("\"{}\"", value.replace('"', "'"))
    } else {
        value
    }
}

fn dbgate(profiles: &[&ExportedProfile]) -> String {
    let mut used = HashSet::new();
    let ids: Vec<String> = profiles
        .iter()
        .map(|p| {
            let base = identifier(
                &p.name.to_ascii_lowercase(),
                |c| c.is_ascii_alphanumeric() || c == '_',
                "con",
            );
            let base = if base.starts_with(|c: char| c.is_ascii_digit()) {
                format!("con_{base}")
            } else {
                base
            };
            dedupe(base, &mut used)
        })
        .collect();
    let mut out = String::from(
        "# DbGate connections written by SQLLY. Load them as environment variables\n\
         # (docker run --env-file, or a .env file beside DbGate). Passwords are not\n\
         # included; add PASSWORD_<id>=... lines or enter them in DbGate.\n",
    );
    let _ = writeln!(out, "CONNECTIONS={}", ids.join(","));
    for (p, id) in profiles.iter().zip(&ids) {
        let Some(engine) = dbgate_engine(p.database_type) else {
            continue;
        };
        out.push('\n');
        let mut line = |key: &str, value: &str| {
            let _ = writeln!(out, "{key}_{id}={}", env_value(value));
        };
        line("LABEL", &p.name);
        line("ENGINE", engine);
        if is_file_engine(p.database_type) {
            line("FILE", &p.server);
        } else {
            line("SERVER", &p.server);
            if p.port > 0 {
                line("PORT", &p.port.to_string());
            }
            if let Some(db) = &p.database {
                line("DATABASE", db);
            }
        }
        if let Some(user) = user_name(p) {
            line("USER", user);
        }
        if p.block_all_but_select {
            line("READONLY", "1");
        }
        if matches!(p.authentication, ExportedAuthentication::Integrated) {
            line("AUTH_TYPE", "sspi");
        }
        if p.trust_server_certificate {
            line("SSL_TRUST_CERTIFICATE", "1");
        }
        match effective_tls(p) {
            Some(TlsMode::Disable) | Some(TlsMode::Legacy) | None => {}
            Some(mode) => {
                line("USE_SSL", "1");
                if matches!(mode, TlsMode::VerifyCa | TlsMode::VerifyFull) {
                    line("SSL_REJECT_UNAUTHORIZED", "1");
                }
                for (key, path) in [
                    ("SSL_CA_FILE", &p.tls_ca_file),
                    ("SSL_CERT_FILE", &p.tls_client_cert_file),
                    ("SSL_KEY_FILE", &p.tls_client_key_file),
                ] {
                    if let Some(path) = path_text(path) {
                        line(key, &path);
                    }
                }
            }
        }
        if let Some(tunnel) = &p.ssh_tunnel {
            let hop = &tunnel.target;
            line("USE_SSH", "1");
            line("SSH_HOST", &hop.host);
            line("SSH_PORT", &hop.port.to_string());
            line("SSH_LOGIN", &hop.username);
            match &hop.auth {
                ExportedSshAuth::KeyFile { path } => {
                    line("SSH_MODE", "keyFile");
                    line("SSH_KEY_FILE", path);
                }
                ExportedSshAuth::Agent => line("SSH_MODE", "agent"),
                ExportedSshAuth::Password => line("SSH_MODE", "userPassword"),
            }
        }
    }
    out
}
