//! DbGate connection definitions in environment-variable form, documented at
//! <https://docs.dbgate.io/env-variables/>: `CONNECTIONS=a,b` names the
//! connections, and `LABEL_a`, `ENGINE_a`, `SERVER_a`, `PORT_a`, `USER_a`,
//! `DATABASE_a`, `FILE_a`, `READONLY_a`, `USE_SSL_a`, `USE_SSH_a`,
//! `SSH_HOST_a`, … describe each one. DbGate reads them from the process
//! environment or a `.env` file, which is what this parser accepts.

use std::collections::HashMap;

use sqlly_lang::sqlmodel::DatabaseType;

use super::{ImportedConnection, SkippedConnection, ToolFileKind, ToolImportPreview, skip_reason};
use crate::connection::{ConnectionEnvironment, TlsMode};
use crate::connection_url::ParsedConnectionUrl;
use crate::profile_store::{ExportedSshAuth, ExportedSshHop, ExportedSshTunnel};

/// `KEY=value` pairs from a `.env` file: `#` comments, optional `export `,
/// single- or double-quoted values.
pub(crate) fn env_pairs(contents: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        let value = if value.len() >= 2
            && ((value.starts_with('"') && value.ends_with('"'))
                || (value.starts_with('\'') && value.ends_with('\'')))
        {
            &value[1..value.len() - 1]
        } else {
            value
        };
        vars.insert(key.trim().to_string(), value.to_string());
    }
    vars
}

pub(super) fn looks_like(contents: &str) -> bool {
    contents.lines().any(|line| {
        let line = line.trim();
        let line = line.strip_prefix("export ").unwrap_or(line).trim_start();
        line.starts_with("CONNECTIONS=")
    })
}

/// DbGate engine id (the part before `@`) → SQLLY engine.
pub(crate) fn engine_from_id(engine: &str) -> Option<DatabaseType> {
    let id = engine.split('@').next().unwrap_or("").to_ascii_lowercase();
    Some(match id.as_str() {
        "mysql" => DatabaseType::MySql,
        "mariadb" => DatabaseType::MariaDb,
        "postgres" | "postgresql" => DatabaseType::Postgres,
        "cockroach" => DatabaseType::CockroachDb,
        "mssql" => DatabaseType::SqlServer,
        "sqlite" => DatabaseType::Sqlite,
        "libsql" => DatabaseType::LibSql,
        "duckdb" => DatabaseType::DuckDb,
        "clickhouse" => DatabaseType::ClickHouse,
        "redis" => DatabaseType::Redis,
        _ => return None,
    })
}

fn truthy(value: Option<&String>) -> bool {
    value.is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

pub(super) fn parse(contents: &str) -> Result<ToolImportPreview, String> {
    let vars = env_pairs(contents);
    let ids = vars
        .get("CONNECTIONS")
        .ok_or_else(|| "missing CONNECTIONS".to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::DbGateEnv),
        ..Default::default()
    };
    for id in ids.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let get = |prefix: &str| {
            vars.get(&format!("{prefix}_{id}"))
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let name = get("LABEL").unwrap_or_else(|| id.to_string());
        let engine_id = get("ENGINE").unwrap_or_default();
        let Some(engine) = engine_from_id(&engine_id) else {
            preview.skipped.push(SkippedConnection {
                name,
                reason: skip_reason(
                    "services.tool_import.skip_reasons.dbgate_engine_unsupported",
                    format!("unsupported DbGate engine: {engine_id}"),
                )
                .with_arg("engine_name", &engine_id),
            });
            continue;
        };
        let file_engine = matches!(engine, DatabaseType::Sqlite | DatabaseType::DuckDb);
        // `URL_<id>` (PostgreSQL, CockroachDB, Oracle, …) replaces the
        // host/port/database fields when it is set and readable.
        let from_url =
            get("URL").and_then(|url| crate::connection_url::parse_connection_url(&url).ok());
        let server = if file_engine {
            get("FILE").or_else(|| get("DATABASE_FILE"))
        } else {
            get("SERVER")
        };
        let server = server.or_else(|| from_url.as_ref().map(|u| u.server.clone()));
        let Some(server) = server else {
            let reason = if file_engine {
                skip_reason(
                    "services.tool_import.skip_reasons.file_path_missing",
                    "no database file path",
                )
            } else {
                skip_reason("services.tool_import.skip_reasons.no_host", "no host")
            };
            preview.skipped.push(SkippedConnection { name, reason });
            continue;
        };
        let use_ssl = truthy(vars.get(&format!("USE_SSL_{id}")));
        let tls_mode = use_ssl.then(|| {
            if truthy(vars.get(&format!("SSL_REJECT_UNAUTHORIZED_{id}"))) {
                TlsMode::VerifyFull
            } else {
                TlsMode::Require
            }
        });
        let integrated = get("AUTH_TYPE").is_some_and(|t| t.eq_ignore_ascii_case("sspi"));
        let url_had_password = from_url.as_ref().is_some_and(|u| u.password.is_some());
        let mut parsed = ParsedConnectionUrl {
            engine,
            server,
            port: if file_engine {
                None
            } else {
                get("PORT").and_then(|p| p.parse().ok())
            },
            database: if file_engine { None } else { get("DATABASE") },
            username: if integrated { None } else { get("USER") },
            integrated_security: integrated,
            trust_server_certificate: truthy(vars.get(&format!("SSL_TRUST_CERTIFICATE_{id}")))
                .then_some(true),
            tls_mode,
            encrypt: use_ssl.then_some(true),
            tls_ca_file: get("SSL_CA_FILE").map(Into::into),
            tls_client_cert_file: get("SSL_CERT_FILE").map(Into::into),
            tls_client_key_file: get("SSL_KEY_FILE").map(Into::into),
            ..Default::default()
        };
        if let Some(url) = from_url {
            let (url, _) = super::strip_password(url);
            parsed = ParsedConnectionUrl {
                engine,
                tls_mode: parsed.tls_mode.or(url.tls_mode),
                username: parsed.username.or(url.username.clone()),
                ..url
            };
        }
        let use_ssh = truthy(vars.get(&format!("USE_SSH_{id}")));
        let ssh_tunnel = if use_ssh {
            get("SSH_HOST").map(|host| {
                let mode = get("SSH_MODE").unwrap_or_default();
                let auth = match mode.as_str() {
                    "keyFile" => match get("SSH_KEY_FILE") {
                        Some(path) => ExportedSshAuth::KeyFile { path },
                        None => ExportedSshAuth::Agent,
                    },
                    "agent" => ExportedSshAuth::Agent,
                    _ => ExportedSshAuth::Password,
                };
                ExportedSshTunnel {
                    target: ExportedSshHop {
                        host,
                        port: get("SSH_PORT").and_then(|p| p.parse().ok()).unwrap_or(22),
                        username: get("SSH_LOGIN").unwrap_or_default(),
                        auth,
                    },
                    jump: None,
                    keepalive: Default::default(),
                }
            })
        } else {
            None
        };
        preview.connections.push(ImportedConnection {
            name,
            parsed,
            group: None,
            environment: ConnectionEnvironment::Unset,
            read_only: truthy(vars.get(&format!("READONLY_{id}"))),
            had_password: get("PASSWORD").is_some() || url_had_password,
            has_ssh_handler: use_ssh,
            ssh_tunnel,
        });
    }
    Ok(preview)
}
