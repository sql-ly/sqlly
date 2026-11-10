//! pgAdmin 4 `servers.json` (Tools ▸ Import/Export Servers, or
//! `setup.py dump-servers`), documented at
//! <https://www.pgadmin.org/docs/pgadmin4/latest/import_export_servers.html>.
//!
//! The file is `{"Servers": {"1": {...}, "2": {...}}}`. pgAdmin 7 moved the
//! SSL settings from top-level `SSLMode`/`SSLRootCert`/… keys into a libpq
//! `ConnectionParameters` object; both layouts are read. Passwords are never
//! part of a pgAdmin export (only a `PassFile` path), so `had_password` is set
//! when either `Password` or `PassFile` is present.

use serde_json::Value;
use sqlly_lang::sqlmodel::DatabaseType;

use super::{ImportedConnection, SkippedConnection, ToolFileKind, ToolImportPreview, skip_reason};
use crate::connection::ConnectionEnvironment;
use crate::connection_url::ParsedConnectionUrl;
use crate::profile_store::{ExportedSshAuth, ExportedSshHop, ExportedSshTunnel};

/// pgAdmin's built-in top-level server group; a server in it has no folder.
pub(crate) const DEFAULT_GROUP: &str = "Servers";

pub(super) fn looks_like(contents: &str) -> bool {
    serde_json::from_str::<Value>(contents)
        .ok()
        .and_then(|v| v.get("Servers").map(Value::is_object))
        .unwrap_or(false)
}

pub(super) fn parse(contents: &str) -> Result<ToolImportPreview, String> {
    let root: Value = serde_json::from_str(contents).map_err(|e| e.to_string())?;
    let servers = root
        .get("Servers")
        .and_then(Value::as_object)
        .ok_or_else(|| "missing Servers object".to_string())?;

    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::PgAdminServersJson),
        ..Default::default()
    };

    // pgAdmin numbers its servers "1", "2", …; keep that order.
    let mut entries: Vec<(&String, &Value)> = servers.iter().collect();
    entries.sort_by_key(|(key, _)| key.parse::<u64>().unwrap_or(u64::MAX));

    for (_, server) in entries {
        let name = str_field(server, "Name").unwrap_or_else(|| "Unnamed".to_string());
        let host = str_field(server, "Host")
            .or_else(|| str_field(server, "HostAddr"))
            .unwrap_or_default();
        // A server defined only by a libpq `Service` name has no host to
        // carry over; the service file itself can be imported instead.
        if host.is_empty() {
            preview.skipped.push(SkippedConnection {
                name,
                reason: skip_reason("services.tool_import.skip_reasons.no_host", "no host"),
            });
            continue;
        }
        let params = server.get("ConnectionParameters");
        let param = |key: &str, legacy: &str| {
            params
                .and_then(|p| str_field(p, key))
                .or_else(|| str_field(server, legacy))
        };
        let tls_mode = param("sslmode", "SSLMode")
            .as_deref()
            .and_then(super::libpq_tls_mode);
        let parsed = ParsedConnectionUrl {
            engine: DatabaseType::Postgres,
            server: host,
            port: u16_field(server, "Port"),
            database: str_field(server, "MaintenanceDB"),
            username: str_field(server, "Username"),
            tls_mode,
            encrypt: tls_mode.map(super::tls_mode_encrypts),
            tls_ca_file: param("sslrootcert", "SSLRootCert").map(Into::into),
            tls_client_cert_file: param("sslcert", "SSLCert").map(Into::into),
            tls_client_key_file: param("sslkey", "SSLKey").map(Into::into),
            ..Default::default()
        };
        let group = str_field(server, "Group").filter(|g| g != DEFAULT_GROUP);
        let had_password = server.get("Password").is_some_and(|v| !v.is_null())
            || server.get("PassFile").is_some();
        let (has_ssh_handler, ssh_tunnel) = ssh_tunnel(server);
        preview.connections.push(ImportedConnection {
            name,
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

fn ssh_tunnel(server: &Value) -> (bool, Option<ExportedSshTunnel>) {
    let enabled = match server.get("UseSSHTunnel") {
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0) != 0,
        Some(Value::String(s)) => matches!(s.as_str(), "1" | "true"),
        _ => false,
    };
    if !enabled {
        return (false, None);
    }
    let Some(host) = str_field(server, "TunnelHost") else {
        return (true, None);
    };
    let identity = str_field(server, "TunnelIdentityFile");
    let key_auth = match server.get("TunnelAuthentication") {
        Some(Value::Number(n)) => n.as_i64() == Some(1),
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "1",
        _ => identity.is_some(),
    };
    let auth = match (key_auth, identity) {
        (true, Some(path)) => ExportedSshAuth::KeyFile { path },
        (true, None) => ExportedSshAuth::Agent,
        (false, _) => ExportedSshAuth::Password,
    };
    let tunnel = ExportedSshTunnel {
        target: ExportedSshHop {
            host,
            port: u16_field(server, "TunnelPort").unwrap_or(22),
            username: str_field(server, "TunnelUsername").unwrap_or_default(),
            auth,
        },
        jump: None,
        keepalive: Default::default(),
    };
    (true, Some(tunnel))
}

pub(super) fn str_field(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

pub(super) fn u16_field(value: &Value, key: &str) -> Option<u16> {
    match value.get(key)? {
        Value::Number(n) => n.as_u64().and_then(|v| u16::try_from(v).ok()),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
