//! Import connection definitions from other database tools' configuration files.
//!
//! TablePlus key names reconciled against fixture exports on 2026-08-30:
//! `ConnectionName`, `Driver`, `DatabaseHost`, `DatabasePort`, `DatabaseUser`,
//! `DatabaseName`, `DatabasePath`, `GroupID`, `Enviroment` (vendor typo),
//! `Environment`, `tLSMode`, `isUseSSL`, `SocketType`, `isOverSSH`,
//! `ServerAddress`, `ServerPort`, `ServerUser`, `usePrivateKey`,
//! `ServerPrivateKeyName`, `DatabasePassword`, `ServerPassword`.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Cursor;

use plist::Value as PlistValue;
use serde_json::Value;
use sqlly_lang::sqlmodel::DatabaseType;

use crate::connection::{ConnectionEnvironment, ConnectionRoute};
use crate::connection_url::{ParseUrlError, ParsedConnectionUrl, parse_connection_url};
use crate::profile_store::{
    ExportedAuthentication, ExportedProfile, ExportedProjectEntity, ExportedSafetyFeatures,
    ExportedSshAuth, ExportedSshHop, ExportedSshTunnel, PROFILE_EXPORT_VERSION,
    ProfileExportBundle,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolFileKind {
    JetBrainsDataSourcesXml,
    DbeaverDataSourcesJson,
    /// TablePlus `Connections.plist` (XML or binary property list) from
    /// `~/Library/Application Support/com.tinyapp.TablePlus/Data/`.
    TablePlusConnectionsPlist,
    /// TablePlus "Export Connections" file saved without passwords and
    /// without a file password (a plain JSON array of connection objects).
    TablePlusConnectionJson,
    /// Navicat "Export Connections" (`.ncx`, XML `<Connections>`/`<Connection>`).
    NavicatNcx,
    /// Redis Insight "Export databases" (a JSON array, or the same array
    /// base64-encoded). Imported with its secrets through the credential
    /// importer (see [`redis_insight`]), not through the preview bundle.
    RedisInsightConnectionsJson,
    /// pgAdmin 4 `servers.json` (Import/Export Servers).
    PgAdminServersJson,
    /// Azure Data Studio or VS Code (MSSQL extension) `settings.json`.
    AzureDataStudioSettingsJson,
    /// PostgreSQL connection service file (`pg_service.conf`).
    PgServiceConf,
    /// DbGate connection definitions as environment variables (`.env`).
    DbGateEnv,
}

pub mod azure_data_studio;
pub mod dbgate;
pub mod pg_service;
pub mod pgadmin;
pub mod redis_insight;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedConnection {
    pub name: String,
    pub parsed: ParsedConnectionUrl,
    pub group: Option<String>,
    pub environment: ConnectionEnvironment,
    pub read_only: bool,
    pub had_password: bool,
    pub has_ssh_handler: bool,
    pub ssh_tunnel: Option<ExportedSshTunnel>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedConnection {
    pub name: String,
    /// Why the connection was left out: a keyed clause the UI words in its
    /// language (the fallback is English).
    pub reason: crate::DisplayMessage,
}

fn skip_reason(key: &'static str, fallback: impl Into<String>) -> crate::DisplayMessage {
    crate::DisplayMessage::new(key, fallback)
}

/// A JetBrains data source's JDBC URL that could not be read, as a keyed
/// skip clause per parser outcome. Only the `url` crate's own explanation is
/// passed through as raw `{detail}` text.
fn jdbc_url_skip_reason(error: &ParseUrlError) -> crate::DisplayMessage {
    match error {
        ParseUrlError::Empty => skip_reason(
            "services.tool_import.skip_reasons.jdbc_url_empty",
            "the JDBC URL is empty",
        ),
        ParseUrlError::Unrecognized => skip_reason(
            "services.tool_import.skip_reasons.jdbc_url_unrecognized",
            "the JDBC URL format is not recognized",
        ),
        ParseUrlError::UnsupportedEngine(engine) => skip_reason(
            "services.tool_import.skip_reasons.engine_unsupported",
            format!("unsupported engine: {engine}"),
        )
        .with_arg("engine_name", engine),
        ParseUrlError::InvalidPort(port) => skip_reason(
            "services.tool_import.skip_reasons.jdbc_url_invalid_port",
            format!("the JDBC URL has an invalid port: {port}"),
        )
        .with_arg("port", port),
        ParseUrlError::Url(detail) => skip_reason(
            "services.tool_import.skip_reasons.jdbc_url_unparsed",
            format!("could not parse JDBC URL: {detail}"),
        )
        .with_arg("detail", detail),
    }
}

/// A URL-configured DBeaver connection whose URL could not be read, as a
/// keyed skip clause per parser outcome (see [`jdbc_url_skip_reason`]).
fn dbeaver_url_skip_reason(error: &ParseUrlError) -> crate::DisplayMessage {
    match error {
        ParseUrlError::Empty => skip_reason(
            "services.tool_import.skip_reasons.url_empty",
            "the connection URL is empty",
        ),
        ParseUrlError::Unrecognized => skip_reason(
            "services.tool_import.skip_reasons.url_unrecognized",
            "the connection URL format is not recognized",
        ),
        ParseUrlError::UnsupportedEngine(engine) => skip_reason(
            "services.tool_import.skip_reasons.url_engine_unsupported",
            format!("the connection URL names an unsupported engine: {engine}"),
        )
        .with_arg("engine_name", engine),
        ParseUrlError::InvalidPort(port) => skip_reason(
            "services.tool_import.skip_reasons.url_invalid_port",
            format!("the connection URL has an invalid port: {port}"),
        )
        .with_arg("port", port),
        ParseUrlError::Url(detail) => skip_reason(
            "services.tool_import.skip_reasons.url_unparsed",
            format!("could not parse URL: {detail}"),
        )
        .with_arg("detail", detail),
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ToolImportPreview {
    pub kind: Option<ToolFileKind>,
    pub connections: Vec<ImportedConnection>,
    pub skipped: Vec<SkippedConnection>,
}

fn default_port(engine: DatabaseType) -> u16 {
    crate::credential_proxy::default_port(engine)
}

/// A libpq `sslmode` word as a TLS mode; `allow` and `prefer` (opportunistic
/// TLS) have no explicit SQLLY mode and keep the engine default.
fn libpq_tls_mode(mode: &str) -> Option<crate::connection::TlsMode> {
    use crate::connection::TlsMode;
    match mode.trim().to_ascii_lowercase().as_str() {
        "disable" => Some(TlsMode::Disable),
        "require" => Some(TlsMode::Require),
        "verify-ca" => Some(TlsMode::VerifyCa),
        "verify-full" => Some(TlsMode::VerifyFull),
        _ => None,
    }
}

fn tls_mode_encrypts(mode: crate::connection::TlsMode) -> bool {
    !matches!(
        mode,
        crate::connection::TlsMode::Disable | crate::connection::TlsMode::Legacy
    )
}

/// Key used for duplicate detection: (engine, lowercase host, effective port, lowercase database).
pub fn dedupe_key(engine: DatabaseType, server: &str, port: u16, database: Option<&str>) -> String {
    format!(
        "{}|{}|{}|{}",
        engine as u8,
        server.to_ascii_lowercase(),
        port,
        database.unwrap_or("").to_ascii_lowercase()
    )
}

pub fn detect_kind(file_name: &str, contents: &str) -> Option<ToolFileKind> {
    let lower = file_name.to_ascii_lowercase();
    if (lower.ends_with(".json") || lower.ends_with(".jsonc"))
        && azure_data_studio::looks_like(contents)
    {
        return Some(ToolFileKind::AzureDataStudioSettingsJson);
    }
    if lower.ends_with(".json") && pgadmin::looks_like(contents) {
        return Some(ToolFileKind::PgAdminServersJson);
    }
    if dbgate::looks_like(contents) {
        return Some(ToolFileKind::DbGateEnv);
    }
    if pg_service::looks_like(file_name, contents) {
        return Some(ToolFileKind::PgServiceConf);
    }
    if lower.ends_with(".xml") && contents.contains("<data-source") {
        return Some(ToolFileKind::JetBrainsDataSourcesXml);
    }
    if lower.ends_with(".ncx")
        || (lower.ends_with(".xml")
            && contents.contains("<Connections")
            && contents.contains("<Connection "))
    {
        return Some(ToolFileKind::NavicatNcx);
    }
    if lower.ends_with(".json")
        && let Ok(value) = serde_json::from_str::<Value>(contents)
        && value.get("connections").is_some()
    {
        return Some(ToolFileKind::DbeaverDataSourcesJson);
    }
    if lower.ends_with(".plist") && (contents.starts_with("bplist") || contents.contains("<plist"))
    {
        return Some(ToolFileKind::TablePlusConnectionsPlist);
    }
    if !lower.ends_with(".tableplusconnection") && redis_insight::looks_like_redis_insight(contents)
    {
        return Some(ToolFileKind::RedisInsightConnectionsJson);
    }
    if lower.ends_with(".tableplusconnection")
        && let Ok(value) = serde_json::from_str::<Value>(contents)
    {
        match value {
            Value::Array(_) => return Some(ToolFileKind::TablePlusConnectionJson),
            Value::Object(obj) if obj.get("connections").is_some() => {
                return Some(ToolFileKind::TablePlusConnectionJson);
            }
            _ => {}
        }
    }
    None
}

fn strip_password(mut parsed: ParsedConnectionUrl) -> (ParsedConnectionUrl, bool) {
    let had = parsed.password.take().is_some();
    (parsed, had)
}

fn provider_to_engine(provider: &str, driver: &str) -> Option<DatabaseType> {
    let driver = driver.to_ascii_lowercase();
    match provider.to_ascii_lowercase().as_str() {
        "postgresql" => Some(DatabaseType::Postgres),
        // DBeaver files MariaDB under the MySQL provider with its own driver.
        "mysql" if driver.contains("maria") => Some(DatabaseType::MariaDb),
        "mysql" => Some(DatabaseType::MySql),
        "mariadb" => Some(DatabaseType::MariaDb),
        "sqlserver" => Some(DatabaseType::SqlServer),
        "sqlite" => Some(DatabaseType::Sqlite),
        "clickhouse" => Some(DatabaseType::ClickHouse),
        "generic" | "duckdb" if driver.contains("duckdb") => Some(DatabaseType::DuckDb),
        _ => None,
    }
}

fn dbeaver_environment(type_name: &str) -> ConnectionEnvironment {
    match type_name.to_ascii_lowercase().as_str() {
        "dev" => ConnectionEnvironment::Dev,
        "test" => ConnectionEnvironment::Test,
        "prod" | "production" => ConnectionEnvironment::Production,
        "staging" => ConnectionEnvironment::Staging,
        "local" => ConnectionEnvironment::Local,
        _ => ConnectionEnvironment::Unset,
    }
}

fn parse_ssh_port(value: &Value) -> u16 {
    match value {
        Value::Number(n) => n.as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(22),
        Value::String(s) => s.parse().unwrap_or(22),
        _ => 22,
    }
}

fn map_dbeaver_ssh(handler: &Value) -> (bool, Option<ExportedSshTunnel>) {
    let Some(handler_obj) = handler.as_object() else {
        return (false, None);
    };
    let enabled = handler_obj
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    // DBeaver writes the tunnel settings under `properties`; older files
    // (and hand-written ones) put them on the handler itself.
    let mut merged = handler_obj.clone();
    if let Some(props) = handler_obj.get("properties").and_then(Value::as_object) {
        for (key, value) in props {
            merged.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
    let obj = &merged;
    if !enabled {
        return (true, None);
    }
    let host = obj
        .get("host")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if host.is_empty() {
        return (true, None);
    }
    let port = obj.get("port").map(parse_ssh_port).unwrap_or(22);
    let username = obj
        .get("user")
        .or_else(|| obj.get("username"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let auth_type = obj
        .get("authType")
        .or_else(|| obj.get("auth-type"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    let auth = if auth_type.contains("KEY") || auth_type.contains("PUBLIC") {
        let path = obj
            .get("keyPath")
            .or_else(|| obj.get("privateKey"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if path.is_empty() {
            ExportedSshAuth::Agent
        } else {
            ExportedSshAuth::KeyFile { path }
        }
    } else if auth_type.contains("PASSWORD") || obj.get("password").is_some() {
        ExportedSshAuth::Password
    } else {
        ExportedSshAuth::Agent
    };
    let tunnel = ExportedSshTunnel {
        target: ExportedSshHop {
            host,
            port,
            username,
            auth,
        },
        jump: None,
        keepalive: Default::default(),
    };
    (true, Some(tunnel))
}

fn json_had_password(config: &Value) -> bool {
    config.get("password").is_some_and(|v| !v.is_null())
        || config
            .get("auth-properties")
            .and_then(Value::as_object)
            .and_then(|o| o.get("password"))
            .is_some_and(|v| !v.is_null())
}

pub fn parse_tool_file_bytes(
    kind: ToolFileKind,
    bytes: &[u8],
) -> Result<ToolImportPreview, String> {
    match kind {
        ToolFileKind::TablePlusConnectionsPlist => {
            parse_tableplus_plist_with_groups(bytes, &HashMap::new())
        }
        other => {
            let contents = std::str::from_utf8(bytes).map_err(|e| e.to_string())?;
            parse_tool_file(other, contents)
        }
    }
}

pub fn parse_tool_file(kind: ToolFileKind, contents: &str) -> Result<ToolImportPreview, String> {
    match kind {
        ToolFileKind::JetBrainsDataSourcesXml => parse_jetbrains_xml(contents),
        ToolFileKind::DbeaverDataSourcesJson => parse_dbeaver_json(contents),
        ToolFileKind::TablePlusConnectionJson => parse_tableplus_json(contents),
        ToolFileKind::NavicatNcx => parse_navicat_ncx(contents),
        ToolFileKind::TablePlusConnectionsPlist => {
            parse_tableplus_plist_with_groups(contents.as_bytes(), &HashMap::new())
        }
        ToolFileKind::PgAdminServersJson => pgadmin::parse(contents),
        ToolFileKind::AzureDataStudioSettingsJson => azure_data_studio::parse(contents),
        ToolFileKind::PgServiceConf => pg_service::parse(contents),
        ToolFileKind::DbGateEnv => dbgate::parse(contents),
        ToolFileKind::RedisInsightConnectionsJson => Err(
            "Redis Insight exports carry passwords and certificates; import them with redis_insight::convert and the credential importer"
                .into(),
        ),
    }
}

/// Reads `ConnectionGroups.plist` (`ID` → `Name` mapping).
pub fn parse_tableplus_groups_plist(bytes: &[u8]) -> Result<HashMap<String, String>, String> {
    let root = PlistValue::from_reader(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut groups = HashMap::new();
    let entries = match &root {
        PlistValue::Array(items) => items.iter().collect::<Vec<_>>(),
        PlistValue::Dictionary(dict) => dict
            .get("Groups")
            .or_else(|| dict.get("groups"))
            .and_then(PlistValue::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    for entry in entries {
        let PlistValue::Dictionary(dict) = entry else {
            continue;
        };
        let id = dict
            .get("ID")
            .or_else(|| dict.get("id"))
            .and_then(plist_string)
            .unwrap_or_default();
        let name = dict
            .get("Name")
            .or_else(|| dict.get("name"))
            .and_then(plist_string)
            .unwrap_or_default();
        if !id.is_empty() && !name.is_empty() {
            groups.insert(id, name);
        }
    }
    Ok(groups)
}

pub fn parse_tableplus_plist_with_groups(
    bytes: &[u8],
    groups: &HashMap<String, String>,
) -> Result<ToolImportPreview, String> {
    let root = PlistValue::from_reader(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::TablePlusConnectionsPlist),
        ..Default::default()
    };
    let entries = tableplus_connection_entries_plist(&root)?;
    for dict in entries {
        let record = tableplus_record_from_plist_dict(&dict);
        match tableplus_record_to_connection(record, groups) {
            Ok(conn) => preview.connections.push(conn),
            Err(skip) => preview.skipped.push(skip),
        }
    }
    Ok(preview)
}

/// One TablePlus connection after key normalisation.
#[derive(Debug, Default)]
struct TablePlusRecord {
    name: Option<String>,
    driver: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    user: Option<String>,
    database: Option<String>,
    path: Option<String>,
    group_id: Option<String>,
    environment: Option<String>,
    tls_mode: Option<i64>,
    use_ssl: Option<bool>,
    socket_type: Option<i64>,
    over_ssh: Option<bool>,
    ssh_host: Option<String>,
    ssh_port: Option<u16>,
    ssh_user: Option<String>,
    ssh_use_key: Option<bool>,
    ssh_key_name: Option<String>,
    had_password: bool,
}

fn tableplus_key(key: &str) -> Option<&'static str> {
    Some(match key.to_ascii_lowercase().as_str() {
        "connectionname" | "name" => "name",
        "driver" => "driver",
        "databasehost" | "host" => "host",
        "databaseport" | "port" => "port",
        "databaseuser" | "user" => "user",
        "databasename" | "database" => "database",
        "databasepath" | "path" => "path",
        "groupid" => "group_id",
        "enviroment" | "environment" | "env" => "environment",
        "tlsmode" => "tls_mode",
        "isusessl" | "usessl" => "use_ssl",
        "sockettype" => "socket_type",
        "isoverssh" | "overssh" => "over_ssh",
        "serveraddress" | "sshhost" => "ssh_host",
        "serverport" | "sshport" => "ssh_port",
        "serveruser" | "sshuser" => "ssh_user",
        "useprivatekey" => "ssh_use_key",
        "serverprivatekeyname" | "sshprivatekeypath" => "ssh_key_name",
        "databasepassword" | "password" | "serverpassword" => "had_password",
        _ => return None,
    })
}

fn tableplus_driver_to_engine(driver: &str) -> Option<DatabaseType> {
    match driver.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" => Some(DatabaseType::Postgres),
        "mysql" => Some(DatabaseType::MySql),
        "mariadb" => Some(DatabaseType::MariaDb),
        "microsoftsqlserver" | "sqlserver" | "mssql" => Some(DatabaseType::SqlServer),
        "sqlite" => Some(DatabaseType::Sqlite),
        "cockroachdb" => Some(DatabaseType::CockroachDb),
        _ => None,
    }
}

fn tableplus_tls(parsed: &mut ParsedConnectionUrl, tls_mode: Option<i64>, use_ssl: Option<bool>) {
    let encrypt = tls_mode.map(|mode| mode >= 1).or(use_ssl).unwrap_or(false);
    parsed.encrypt = Some(encrypt);
    parsed.trust_server_certificate = Some(matches!(tls_mode, Some(1)));
}

fn tableplus_ssh_tunnel(rec: &TablePlusRecord) -> (bool, Option<ExportedSshTunnel>) {
    let enabled = rec.socket_type == Some(1)
        || rec.over_ssh == Some(true)
        || rec.ssh_host.as_ref().is_some_and(|host| !host.is_empty());
    if !enabled {
        return (false, None);
    }
    let host = rec.ssh_host.clone().unwrap_or_default();
    if host.is_empty() {
        return (true, None);
    }
    let auth = if rec.ssh_use_key == Some(true) {
        ExportedSshAuth::KeyFile {
            path: rec.ssh_key_name.clone().unwrap_or_default(),
        }
    } else {
        ExportedSshAuth::Password
    };
    let tunnel = ExportedSshTunnel {
        target: ExportedSshHop {
            host,
            port: rec.ssh_port.unwrap_or(22),
            username: rec.ssh_user.clone().unwrap_or_default(),
            auth,
        },
        jump: None,
        keepalive: Default::default(),
    };
    (true, Some(tunnel))
}

fn tableplus_record_to_connection(
    rec: TablePlusRecord,
    groups: &HashMap<String, String>,
) -> Result<ImportedConnection, SkippedConnection> {
    let name = rec.name.clone().unwrap_or_else(|| "Unnamed".to_string());
    let driver = rec.driver.clone().unwrap_or_default();
    let Some(engine) = tableplus_driver_to_engine(&driver) else {
        return Err(SkippedConnection {
            name,
            reason: skip_reason(
                "services.tool_import.skip_reasons.driver_unsupported",
                format!("{driver} is not supported"),
            )
            .with_arg("driver", &driver),
        });
    };

    let (has_ssh_handler, ssh_tunnel) = tableplus_ssh_tunnel(&rec);
    let group = rec.group_id.as_ref().and_then(|id| groups.get(id).cloned());
    let environment = dbeaver_environment(rec.environment.as_deref().unwrap_or_default());

    let mut parsed = if engine == DatabaseType::Sqlite {
        let server = rec
            .path
            .or(rec.host)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| SkippedConnection {
                name: name.clone(),
                reason: skip_reason(
                    "services.tool_import.skip_reasons.sqlite_path_missing",
                    "sqlite connection missing database path",
                ),
            })?;
        ParsedConnectionUrl {
            engine,
            server,
            port: None,
            ..Default::default()
        }
    } else {
        let server = rec
            .host
            .filter(|host| !host.is_empty())
            .ok_or_else(|| SkippedConnection {
                name: name.clone(),
                reason: skip_reason("services.tool_import.skip_reasons.no_host", "no host"),
            })?;
        ParsedConnectionUrl {
            engine,
            server,
            port: rec.port,
            database: rec.database,
            username: rec.user,
            ..Default::default()
        }
    };
    tableplus_tls(&mut parsed, rec.tls_mode, rec.use_ssl);

    Ok(ImportedConnection {
        name,
        parsed,
        group,
        environment,
        read_only: false,
        had_password: rec.had_password,
        has_ssh_handler,
        ssh_tunnel,
    })
}

fn tableplus_connection_entries_plist(root: &PlistValue) -> Result<Vec<plist::Dictionary>, String> {
    match root {
        PlistValue::Array(items) => Ok(items
            .iter()
            .filter_map(PlistValue::as_dictionary)
            .cloned()
            .collect()),
        PlistValue::Dictionary(dict) => dict
            .get("Connections")
            .or_else(|| dict.get("connections"))
            .and_then(PlistValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(PlistValue::as_dictionary)
                    .cloned()
                    .collect()
            })
            .ok_or_else(|| "plist missing connection array".to_string()),
        _ => Err("unrecognised TablePlus plist shape".to_string()),
    }
}

fn plist_string(value: &PlistValue) -> Option<String> {
    match value {
        PlistValue::String(s) => Some(s.clone()),
        PlistValue::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

fn plist_u16(value: &PlistValue) -> Option<u16> {
    match value {
        PlistValue::Integer(i) => i.as_unsigned().and_then(|v| u16::try_from(v).ok()),
        PlistValue::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn plist_i64(value: &PlistValue) -> Option<i64> {
    match value {
        PlistValue::Integer(i) => i.as_signed(),
        PlistValue::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn plist_bool(value: &PlistValue) -> Option<bool> {
    match value {
        PlistValue::Boolean(b) => Some(*b),
        PlistValue::Integer(i) => i.as_signed().map(|v| v != 0),
        PlistValue::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn tableplus_record_from_plist_dict(dict: &plist::Dictionary) -> TablePlusRecord {
    let mut rec = TablePlusRecord::default();
    for (key, value) in dict {
        let Some(field) = tableplus_key(key) else {
            continue;
        };
        apply_tableplus_plist_field(&mut rec, field, value);
    }
    rec
}

fn tableplus_record_from_json_object(obj: &serde_json::Map<String, Value>) -> TablePlusRecord {
    let mut rec = TablePlusRecord::default();
    for (key, value) in obj {
        let Some(field) = tableplus_key(key) else {
            continue;
        };
        apply_tableplus_json_field(&mut rec, field, value);
    }
    rec
}

fn apply_tableplus_plist_field(rec: &mut TablePlusRecord, field: &str, value: &PlistValue) {
    match field {
        "had_password" => {
            if plist_string(value).is_some_and(|s| !s.is_empty()) {
                rec.had_password = true;
            }
        }
        "name" => rec.name = plist_string(value),
        "driver" => rec.driver = plist_string(value),
        "host" => rec.host = plist_string(value),
        "port" => rec.port = plist_u16(value),
        "user" => rec.user = plist_string(value),
        "database" => rec.database = plist_string(value),
        "path" => rec.path = plist_string(value),
        "group_id" => rec.group_id = plist_string(value),
        "environment" => rec.environment = plist_string(value),
        "tls_mode" => rec.tls_mode = plist_i64(value),
        "use_ssl" => rec.use_ssl = plist_bool(value),
        "socket_type" => rec.socket_type = plist_i64(value),
        "over_ssh" => rec.over_ssh = plist_bool(value),
        "ssh_host" => rec.ssh_host = plist_string(value),
        "ssh_port" => rec.ssh_port = plist_u16(value),
        "ssh_user" => rec.ssh_user = plist_string(value),
        "ssh_use_key" => rec.ssh_use_key = plist_bool(value),
        "ssh_key_name" => rec.ssh_key_name = plist_string(value),
        _ => {}
    }
}

fn apply_tableplus_json_field(rec: &mut TablePlusRecord, field: &str, value: &Value) {
    match field {
        "had_password" => {
            let present =
                value.as_str().is_some_and(|s| !s.is_empty()) || value.as_bool() == Some(true);
            if present {
                rec.had_password = true;
            }
        }
        "name" => rec.name = json_string(value),
        "driver" => rec.driver = json_string(value),
        "host" => rec.host = json_string(value),
        "port" => rec.port = json_u16(value),
        "user" => rec.user = json_string(value),
        "database" => rec.database = json_string(value),
        "path" => rec.path = json_string(value),
        "group_id" => rec.group_id = json_string(value),
        "environment" => rec.environment = json_string(value),
        "tls_mode" => rec.tls_mode = json_i64(value),
        "use_ssl" => rec.use_ssl = value.as_bool(),
        "socket_type" => rec.socket_type = json_i64(value),
        "over_ssh" => rec.over_ssh = value.as_bool(),
        "ssh_host" => rec.ssh_host = json_string(value),
        "ssh_port" => rec.ssh_port = json_u16(value),
        "ssh_user" => rec.ssh_user = json_string(value),
        "ssh_use_key" => rec.ssh_use_key = value.as_bool(),
        "ssh_key_name" => rec.ssh_key_name = json_string(value),
        _ => {}
    }
}

fn json_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => n.as_u64().map(|v| v.to_string()),
        _ => None,
    }
}

fn json_u16(value: &Value) -> Option<u16> {
    match value {
        Value::Number(n) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

fn parse_tableplus_json(contents: &str) -> Result<ToolImportPreview, String> {
    let root: Value = serde_json::from_str(contents).map_err(|e| e.to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::TablePlusConnectionJson),
        ..Default::default()
    };
    let (entries, inline_groups) = match root {
        Value::Array(items) => (items, HashMap::new()),
        Value::Object(obj) => {
            let groups = obj
                .get("groups")
                .or_else(|| obj.get("Groups"))
                .and_then(Value::as_array)
                .map(|items| tableplus_groups_from_json(items.as_slice()))
                .unwrap_or_default();
            let entries = obj
                .get("connections")
                .or_else(|| obj.get("Connections"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            (entries, groups)
        }
        _ => return Err("unrecognised TablePlus export shape".to_string()),
    };
    for entry in entries {
        let Value::Object(obj) = entry else {
            continue;
        };
        let record = tableplus_record_from_json_object(&obj);
        match tableplus_record_to_connection(record, &inline_groups) {
            Ok(conn) => preview.connections.push(conn),
            Err(skip) => preview.skipped.push(skip),
        }
    }
    Ok(preview)
}

fn tableplus_groups_from_json(items: &[Value]) -> HashMap<String, String> {
    let mut groups = HashMap::new();
    for entry in items {
        let Value::Object(obj) = entry else {
            continue;
        };
        let id = obj
            .get("ID")
            .or_else(|| obj.get("id"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let name = obj
            .get("Name")
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if !id.is_empty() && !name.is_empty() {
            groups.insert(id, name);
        }
    }
    groups
}

fn parse_navicat_ncx(contents: &str) -> Result<ToolImportPreview, String> {
    let doc = roxmltree::Document::parse(contents).map_err(|e| e.to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::NavicatNcx),
        ..Default::default()
    };

    for node in doc.descendants().filter(|n| n.has_tag_name("Connection")) {
        let connection_name = node
            .attribute("ConnectionName")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Unnamed")
            .to_string();

        if node.attribute("HTTP").map(str::trim) == Some("true") {
            preview.skipped.push(SkippedConnection {
                name: connection_name,
                reason: skip_reason(
                    "services.tool_import.skip_reasons.http_tunnel_unsupported",
                    "HTTP tunnel connections are not supported",
                ),
            });
            continue;
        }

        let conn_type = node.attribute("ConnType").unwrap_or_default();
        let Some(engine) = navicat_conn_type_to_engine(conn_type) else {
            preview.skipped.push(SkippedConnection {
                name: connection_name,
                reason: skip_reason(
                    "services.tool_import.skip_reasons.connection_type_unsupported",
                    format!("{conn_type} is not supported"),
                )
                .with_arg("connection_type", conn_type),
            });
            continue;
        };

        let had_password = node.attribute("Password").is_some();
        let group = navicat_group(node.attribute("ProjectUUID"));
        let (has_ssh_handler, ssh_tunnel) = navicat_ssh_from_node(&node);

        let parsed = if engine == DatabaseType::Sqlite {
            let Some(server) = node
                .attribute("DatabaseFileName")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                preview.skipped.push(SkippedConnection {
                    name: connection_name.clone(),
                    reason: skip_reason(
                        "services.tool_import.skip_reasons.sqlite_path_missing",
                        "sqlite connection missing database path",
                    ),
                });
                continue;
            };
            ParsedConnectionUrl {
                engine,
                server: server.to_string(),
                port: None,
                ..Default::default()
            }
        } else {
            let Some(server) = node
                .attribute("Host")
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                preview.skipped.push(SkippedConnection {
                    name: connection_name.clone(),
                    reason: skip_reason("services.tool_import.skip_reasons.no_host", "no host"),
                });
                continue;
            };
            let port = node.attribute("Port").and_then(|p| p.trim().parse().ok());
            let integrated = node
                .attribute("MSSQLAuthenMode")
                .is_some_and(|mode| mode.eq_ignore_ascii_case("Windows"));
            ParsedConnectionUrl {
                engine,
                server: server.to_string(),
                port,
                database: node
                    .attribute("Database")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
                username: if integrated {
                    None
                } else {
                    node.attribute("UserName")
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                },
                integrated_security: integrated,
                ..Default::default()
            }
        };

        let mut parsed = parsed;
        navicat_tls_from_node(&mut parsed, engine, &node);

        preview.connections.push(ImportedConnection {
            name: connection_name,
            parsed,
            group,
            environment: ConnectionEnvironment::Unset,
            read_only: false,
            had_password,
            has_ssh_handler,
            ssh_tunnel,
        });
    }

    Ok(preview)
}

fn navicat_conn_type_to_engine(conn_type: &str) -> Option<DatabaseType> {
    match conn_type.to_ascii_uppercase().as_str() {
        "MYSQL" => Some(DatabaseType::MySql),
        "MARIADB" => Some(DatabaseType::MariaDb),
        "PGSQL" => Some(DatabaseType::Postgres),
        "SQLSERVER" => Some(DatabaseType::SqlServer),
        "SQLITE" => Some(DatabaseType::Sqlite),
        _ => None,
    }
}

fn navicat_group(project_uuid: Option<&str>) -> Option<String> {
    let value = project_uuid?.trim();
    if value.is_empty() {
        return None;
    }
    if value.contains('/') || value.chars().any(|c| c.is_ascii_alphabetic()) {
        Some(value.to_string())
    } else {
        None
    }
}

fn navicat_tls_from_node(
    parsed: &mut ParsedConnectionUrl,
    engine: DatabaseType,
    node: &roxmltree::Node<'_, '_>,
) {
    let ssl = node.attribute("SSL").map(str::trim) == Some("true");
    let pg_mode = node
        .attribute("SSL_PGSSLMode")
        .unwrap_or_default()
        .to_ascii_uppercase();
    let encrypt = ssl
        || (engine == DatabaseType::Postgres
            && matches!(pg_mode.as_str(), "REQUIRE" | "VERIFY-CA" | "VERIFY-FULL"));
    parsed.encrypt = Some(encrypt);
    parsed.trust_server_certificate = Some(
        node.attribute("SSL_WeakCertValidation").map(str::trim) == Some("true")
            || node.attribute("SSL_AllowInvalidHostName").map(str::trim) == Some("true"),
    );
}

fn navicat_ssh_from_node(node: &roxmltree::Node<'_, '_>) -> (bool, Option<ExportedSshTunnel>) {
    if node.attribute("SSH").map(str::trim) != Some("true") {
        return (false, None);
    }
    let host = node
        .attribute("SSH_Host")
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if host.is_empty() {
        return (true, None);
    }
    let auth = if node
        .attribute("SSH_AuthenMethod")
        .is_some_and(|method| method.eq_ignore_ascii_case("PUBLICKEY"))
    {
        ExportedSshAuth::KeyFile {
            path: node
                .attribute("SSH_PrivateKey")
                .map(str::trim)
                .unwrap_or_default()
                .to_string(),
        }
    } else {
        ExportedSshAuth::Password
    };
    let port = node
        .attribute("SSH_Port")
        .and_then(|p| p.trim().parse().ok())
        .unwrap_or(22);
    let tunnel = ExportedSshTunnel {
        target: ExportedSshHop {
            host,
            port,
            username: node
                .attribute("SSH_UserName")
                .map(str::trim)
                .unwrap_or_default()
                .to_string(),
            auth,
        },
        jump: None,
        keepalive: Default::default(),
    };
    (true, Some(tunnel))
}

fn parse_jetbrains_xml(contents: &str) -> Result<ToolImportPreview, String> {
    let doc = roxmltree::Document::parse(contents).map_err(|e| e.to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::JetBrainsDataSourcesXml),
        ..Default::default()
    };

    for node in doc.descendants().filter(|n| n.has_tag_name("data-source")) {
        let name = node.attribute("name").unwrap_or("Unnamed").to_string();
        let group = node.attribute("group").map(str::to_string);
        let jdbc_url = node
            .children()
            .find(|n| n.has_tag_name("jdbc-url"))
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let user_name = node
            .children()
            .find(|n| n.has_tag_name("user-name"))
            .and_then(|n| n.text())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let driver_ref = node
            .children()
            .find(|n| n.has_tag_name("driver-ref"))
            .and_then(|n| n.text())
            .unwrap_or("");

        let Some(url) = jdbc_url else {
            preview.skipped.push(SkippedConnection {
                name,
                reason: skip_reason(
                    "services.tool_import.skip_reasons.jdbc_url_missing",
                    "no JDBC URL",
                ),
            });
            continue;
        };

        match parse_connection_url(url) {
            Ok(parsed) => {
                let (mut parsed, had_password) = strip_password(parsed);
                if parsed.username.is_none() {
                    parsed.username = user_name;
                }
                preview.connections.push(ImportedConnection {
                    name,
                    parsed,
                    group,
                    environment: ConnectionEnvironment::Unset,
                    read_only: false,
                    had_password,
                    has_ssh_handler: false,
                    ssh_tunnel: None,
                });
            }
            Err(ParseUrlError::UnsupportedEngine(engine)) => {
                let hint = if driver_ref.is_empty() {
                    engine
                } else {
                    format!("{engine} ({driver_ref})")
                };
                preview.skipped.push(SkippedConnection {
                    name,
                    reason: skip_reason(
                        "services.tool_import.skip_reasons.engine_unsupported",
                        format!("unsupported engine: {hint}"),
                    )
                    .with_arg("engine_name", &hint),
                });
            }
            Err(error) => {
                preview.skipped.push(SkippedConnection {
                    name,
                    reason: jdbc_url_skip_reason(&error),
                });
            }
        }
    }

    Ok(preview)
}

fn parse_dbeaver_json(contents: &str) -> Result<ToolImportPreview, String> {
    let root: Value = serde_json::from_str(contents).map_err(|e| e.to_string())?;
    let connections = root
        .get("connections")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing connections object".to_string())?;

    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::DbeaverDataSourcesJson),
        ..Default::default()
    };

    for (_id, entry) in connections {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed")
            .to_string();
        let provider = entry.get("provider").and_then(Value::as_str).unwrap_or("");
        let driver = entry.get("driver").and_then(Value::as_str).unwrap_or("");
        let Some(engine) = provider_to_engine(provider, driver) else {
            preview.skipped.push(SkippedConnection {
                name,
                reason: skip_reason(
                    "services.tool_import.skip_reasons.provider_unsupported",
                    format!("unsupported provider: {provider}"),
                )
                .with_arg("provider", provider),
            });
            continue;
        };
        let group = entry
            .get("folder")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let read_only = entry
            .get("read-only")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let config = entry.get("configuration").cloned().unwrap_or(Value::Null);
        let config_type = config
            .get("configurationType")
            .or_else(|| entry.get("configurationType"))
            .and_then(Value::as_str)
            .unwrap_or("MANUAL");
        let environment = config
            .get("type")
            .and_then(Value::as_str)
            .map(dbeaver_environment)
            .unwrap_or(ConnectionEnvironment::Unset);
        let had_password = json_had_password(&config);
        let (has_ssh_handler, ssh_tunnel) = config
            .get("handlers")
            .and_then(|h| h.get("ssh_tunnel"))
            .map(map_dbeaver_ssh)
            .unwrap_or((false, None));

        let parsed_result = if config_type.eq_ignore_ascii_case("URL") {
            config
                .get("url")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    skip_reason(
                        "services.tool_import.skip_reasons.url_missing",
                        "URL configuration missing url",
                    )
                })
                .and_then(|url| {
                    parse_connection_url(url).map_err(|error| dbeaver_url_skip_reason(&error))
                })
        } else if matches!(engine, DatabaseType::Sqlite | DatabaseType::DuckDb) {
            let path = config
                .get("database")
                .or_else(|| config.get("path"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if path.is_empty() {
                Err(skip_reason(
                    "services.tool_import.skip_reasons.sqlite_path_missing",
                    "sqlite connection missing database path",
                ))
            } else {
                Ok(ParsedConnectionUrl {
                    engine,
                    server: path.to_string(),
                    port: None,
                    username: config
                        .get("user")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                    ..Default::default()
                })
            }
        } else {
            let host = config
                .get("host")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if host.is_empty() {
                Err(skip_reason(
                    "services.tool_import.skip_reasons.manual_host_missing",
                    "manual configuration missing host",
                ))
            } else {
                let port = config.get("port").and_then(|p| match p {
                    Value::String(s) => s.parse().ok(),
                    Value::Number(n) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
                    _ => None,
                });
                let database = config
                    .get("database")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                let username = config
                    .get("user")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);
                Ok(ParsedConnectionUrl {
                    engine,
                    server: host,
                    port,
                    database,
                    username,
                    ..Default::default()
                })
            }
        };

        match parsed_result {
            Ok(parsed) => {
                let (parsed, url_had_password) = strip_password(parsed);
                preview.connections.push(ImportedConnection {
                    name,
                    parsed,
                    group,
                    environment,
                    read_only,
                    had_password: had_password || url_had_password,
                    has_ssh_handler,
                    ssh_tunnel,
                });
            }
            Err(reason) => {
                preview.skipped.push(SkippedConnection { name, reason });
            }
        }
    }

    Ok(preview)
}

pub fn preview_to_bundle(
    preview: &ToolImportPreview,
    existing: &HashSet<String>,
) -> (ProfileExportBundle, Vec<SkippedConnection>) {
    let mut bundle = ProfileExportBundle {
        version: PROFILE_EXPORT_VERSION,
        profiles: Vec::new(),
        clients: Vec::new(),
        projects: Vec::new(),
        environments: Vec::new(),
        sql_auth_accounts: Vec::new(),
    };
    let mut skipped = preview.skipped.clone();
    let mut seen = existing.clone();
    let mut project_names = BTreeSet::new();

    let mut connections = preview.connections.clone();
    connections.sort_by(|a, b| a.name.cmp(&b.name));

    for conn in &connections {
        let port = conn
            .parsed
            .port
            .unwrap_or_else(|| default_port(conn.parsed.engine));
        let key = dedupe_key(
            conn.parsed.engine,
            &conn.parsed.server,
            port,
            conn.parsed.database.as_deref(),
        );
        if seen.contains(&key) {
            skipped.push(SkippedConnection {
                name: conn.name.clone(),
                reason: skip_reason(
                    "services.tool_import.skip_reasons.already_exists",
                    "already exists",
                ),
            });
            continue;
        }
        seen.insert(key);

        if let Some(group) = &conn.group {
            project_names.insert(group.clone());
        }

        let authentication = if conn.parsed.integrated_security {
            ExportedAuthentication::Integrated
        } else {
            ExportedAuthentication::SqlLogin {
                username: conn.parsed.username.clone().unwrap_or_default(),
            }
        };

        bundle.profiles.push(ExportedProfile {
            name: conn.name.clone(),
            server: conn.parsed.server.clone(),
            port,
            database: conn.parsed.database.clone(),
            environment: conn.environment,
            client: None,
            project: conn.group.clone(),
            authentication,
            encrypt: conn.parsed.encrypt.unwrap_or(false),
            trust_server_certificate: conn.parsed.trust_server_certificate.unwrap_or(false),
            timeout_seconds: 30,
            application_name: "SQLLY".to_string(),
            auto_wrap_rollback: false,
            block_all_but_select: conn.read_only,
            confirm_data_changes: false,
            allow_unbounded_mutations: true,
            database_type: conn.parsed.engine,
            statement_timeout_seconds: None,
            startup_sql: None,
            notes: None,
            redis_key_delimiter: None,
            redis_sentinel: None,
            redis_cluster: None,
            engine_auto_detect: false,
            redis_entra_user: None,
            clickhouse_native_port: None,
            entra_account: None,
            route: ConnectionRoute::default(),
            ssh_tunnel: conn.ssh_tunnel.clone(),
            tunnel_profile: None,
            transport: Default::default(),
            favorite: false,
            tls_mode: conn.parsed.tls_mode.unwrap_or_default(),
            tls_ca_file: conn.parsed.tls_ca_file.clone(),
            tls_client_cert_file: conn.parsed.tls_client_cert_file.clone(),
            tls_client_key_file: conn.parsed.tls_client_key_file.clone(),
            tls_trust_os_roots: false,
            tls_direct: false,
        });
    }

    bundle.projects = project_names
        .into_iter()
        .map(|name| ExportedProjectEntity {
            name,
            color_hex: None,
            image_path: None,
            safety: ExportedSafetyFeatures::default(),
        })
        .collect();

    (bundle, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupe_key_is_case_insensitive_on_host_and_database() {
        let a = dedupe_key(
            DatabaseType::Postgres,
            "DB.ACME.INTERNAL",
            5433,
            Some("AppDb"),
        );
        let b = dedupe_key(
            DatabaseType::Postgres,
            "db.acme.internal",
            5433,
            Some("appdb"),
        );
        assert_eq!(a, b);
    }

    #[test]
    fn dedupe_key_uses_effective_port() {
        let with_port = dedupe_key(DatabaseType::Postgres, "h", 5432, Some("db"));
        let default_port_key = dedupe_key(DatabaseType::Postgres, "h", 5432, Some("db"));
        assert_eq!(with_port, default_port_key);
    }

    #[test]
    fn detect_kind_on_plan143_export_is_none() {
        let export = r#"{"version":1,"profiles":[]}"#;
        assert!(detect_kind("connections-export.json", export).is_none());
    }

    #[test]
    fn map_ssh_password_auth_without_inline_secret() {
        let handler = serde_json::json!({
            "enabled": true,
            "host": "bastion",
            "port": "22",
            "user": "ubuntu",
            "authType": "PASSWORD",
            "password": "secret"
        });
        let (has, tunnel) = map_dbeaver_ssh(&handler);
        assert!(has);
        let tunnel = tunnel.unwrap();
        assert_eq!(tunnel.target.host, "bastion");
        assert!(matches!(tunnel.target.auth, ExportedSshAuth::Password));
    }

    #[test]
    fn tableplus_key_aliases_are_case_insensitive() {
        assert_eq!(tableplus_key("ConnectionName"), Some("name"));
        assert_eq!(tableplus_key("DATABASEHOST"), Some("host"));
        assert_eq!(tableplus_key("Enviroment"), Some("environment"));
        assert_eq!(tableplus_key("tLSMode"), Some("tls_mode"));
    }

    #[test]
    fn tableplus_driver_mapping_skips_unsupported() {
        assert_eq!(tableplus_driver_to_engine("Redis"), None);
        let rec = TablePlusRecord {
            name: Some("x".into()),
            driver: Some("Redis".into()),
            host: Some("localhost".into()),
            ..Default::default()
        };
        let err = tableplus_record_to_connection(rec, &HashMap::new()).unwrap_err();
        assert!(err.reason.contains("Redis"));
    }

    #[test]
    fn tableplus_tls_mode_maps_to_encrypt_and_trust() {
        let mut parsed = ParsedConnectionUrl {
            engine: DatabaseType::Postgres,
            server: "h".into(),
            ..Default::default()
        };
        tableplus_tls(&mut parsed, Some(0), None);
        assert_eq!(parsed.encrypt, Some(false));
        assert_eq!(parsed.trust_server_certificate, Some(false));

        tableplus_tls(&mut parsed, Some(1), None);
        assert_eq!(parsed.encrypt, Some(true));
        assert_eq!(parsed.trust_server_certificate, Some(true));

        tableplus_tls(&mut parsed, Some(2), None);
        assert_eq!(parsed.encrypt, Some(true));
        assert_eq!(parsed.trust_server_certificate, Some(false));
    }

    #[test]
    fn tableplus_ssh_key_vs_password_auth() {
        let rec = TablePlusRecord {
            ssh_host: Some("bastion".into()),
            ssh_use_key: Some(true),
            ssh_key_name: Some("/tmp/key".into()),
            ..Default::default()
        };
        let (_, tunnel) = tableplus_ssh_tunnel(&rec);
        assert!(matches!(
            tunnel.unwrap().target.auth,
            ExportedSshAuth::KeyFile { .. }
        ));

        let rec = TablePlusRecord {
            ssh_host: Some("bastion".into()),
            ssh_use_key: Some(false),
            ..Default::default()
        };
        let (_, tunnel) = tableplus_ssh_tunnel(&rec);
        assert!(matches!(
            tunnel.unwrap().target.auth,
            ExportedSshAuth::Password
        ));
    }

    #[test]
    fn tableplus_password_keys_only_set_flag() {
        let mut obj = serde_json::Map::new();
        obj.insert("DatabasePassword".into(), Value::String("hunter2".into()));
        let rec = tableplus_record_from_json_object(&obj);
        assert!(rec.had_password);
        assert!(!format!("{rec:?}").contains("hunter2"));
    }

    #[test]
    fn ncx_windows_auth_maps_to_integrated() {
        let xml = r#"<?xml version="1.0"?><Connections><Connection ConnectionName="w" ConnType="SQLSERVER" Host="h" Port="1433" MSSQLAuthenMode="Windows"/></Connections>"#;
        let preview = parse_navicat_ncx(xml).unwrap();
        assert!(preview.connections[0].parsed.integrated_security);
    }

    #[test]
    fn ncx_password_attribute_sets_flag_without_copying_value() {
        let xml = r#"<?xml version="1.0"?><Connections><Connection ConnectionName="p" ConnType="MYSQL" Host="h" Port="3306" Password="E00641F75A8FFC2E7AF00CC29732DB6E"/></Connections>"#;
        let preview = parse_navicat_ncx(xml).unwrap();
        assert!(preview.connections[0].had_password);
        assert!(preview.connections[0].parsed.password.is_none());
    }

    #[test]
    fn ncx_http_tunnel_is_skipped() {
        let xml = r#"<?xml version="1.0"?><Connections><Connection ConnectionName="h" ConnType="MYSQL" Host="h" HTTP="true"/></Connections>"#;
        let preview = parse_navicat_ncx(xml).unwrap();
        assert_eq!(preview.connections.len(), 0);
        assert!(preview.skipped[0].reason.contains("HTTP tunnel"));
    }

    #[test]
    fn ncx_pg_sslmode_require_sets_encrypt() {
        let xml = r#"<?xml version="1.0"?><Connections><Connection ConnectionName="p" ConnType="PGSQL" Host="h" Port="5432" SSL="false" SSL_PGSSLMode="REQUIRE"/></Connections>"#;
        let preview = parse_navicat_ncx(xml).unwrap();
        assert_eq!(preview.connections[0].parsed.encrypt, Some(true));
    }

    #[test]
    fn url_parse_failures_map_to_whole_keyed_clauses() {
        let empty = jdbc_url_skip_reason(&ParseUrlError::Empty);
        assert_eq!(
            empty.key(),
            Some("services.tool_import.skip_reasons.jdbc_url_empty")
        );
        let port = dbeaver_url_skip_reason(&ParseUrlError::InvalidPort("99999".into()));
        assert_eq!(
            port.key(),
            Some("services.tool_import.skip_reasons.url_invalid_port")
        );
        assert_eq!(
            port.arguments().collect::<Vec<_>>(),
            vec![("port", "99999")]
        );
        // Only the URL parser's own explanation is passed through raw, and
        // without the error's English "invalid URL:" prefix.
        let url = jdbc_url_skip_reason(&ParseUrlError::Url("empty host".into()));
        assert_eq!(
            url.arguments().collect::<Vec<_>>(),
            vec![("detail", "empty host")]
        );
        assert_eq!(url.fallback(), "could not parse JDBC URL: empty host");
    }
}
