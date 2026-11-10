//! PostgreSQL connection service file (`pg_service.conf`,
//! `~/.pg_service.conf`), documented in the PostgreSQL manual under "The
//! Connection Service File": INI sections named after each service, holding
//! libpq connection keywords. psql, pgAdmin, DBeaver, DataGrip and every
//! libpq client read it.

use sqlly_lang::sqlmodel::DatabaseType;

use super::{ImportedConnection, ToolFileKind, ToolImportPreview};
use crate::connection::ConnectionEnvironment;
use crate::connection_url::ParsedConnectionUrl;

/// A section's keywords in file order, lowercased keys.
pub(crate) struct Service {
    pub name: String,
    pub keys: Vec<(String, String)>,
}

impl Service {
    fn get(&self, key: &str) -> Option<&str> {
        self.keys
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }
}

pub(crate) fn sections(contents: &str) -> Vec<Service> {
    let mut services: Vec<Service> = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            services.push(Service {
                name: name.trim().to_string(),
                keys: Vec::new(),
            });
            continue;
        }
        if let (Some(service), Some((key, value))) = (services.last_mut(), line.split_once('=')) {
            service
                .keys
                .push((key.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    services
}

pub(super) fn looks_like(file_name: &str, contents: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    let sections = sections(contents);
    if sections.is_empty() {
        return false;
    }
    if lower.contains("pg_service") {
        return true;
    }
    lower.ends_with(".conf")
        && sections.iter().any(|s| {
            s.keys
                .iter()
                .any(|(k, _)| matches!(k.as_str(), "dbname" | "host" | "hostaddr"))
        })
}

pub(super) fn parse(contents: &str) -> Result<ToolImportPreview, String> {
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::PgServiceConf),
        ..Default::default()
    };
    for service in sections(contents) {
        // A multi-host list (`host=a,b`) fails over in order; the first host
        // is the primary.
        let host = service
            .get("host")
            .or_else(|| service.get("hostaddr"))
            .and_then(|h| h.split(',').next())
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .unwrap_or("localhost")
            .to_string();
        let port = service
            .get("port")
            .and_then(|p| p.split(',').next())
            .and_then(|p| p.trim().parse().ok());
        let tls_mode = service.get("sslmode").and_then(super::libpq_tls_mode);
        let parsed = ParsedConnectionUrl {
            engine: DatabaseType::Postgres,
            server: host,
            port,
            database: service.get("dbname").map(str::to_string),
            username: service.get("user").map(str::to_string),
            tls_mode,
            encrypt: tls_mode.map(super::tls_mode_encrypts),
            tls_ca_file: service.get("sslrootcert").map(Into::into),
            tls_client_cert_file: service.get("sslcert").map(Into::into),
            tls_client_key_file: service.get("sslkey").map(Into::into),
            ..Default::default()
        };
        preview.connections.push(ImportedConnection {
            had_password: service.get("password").is_some(),
            name: service.name,
            parsed,
            group: None,
            environment: ConnectionEnvironment::Unset,
            read_only: false,
            has_ssh_handler: false,
            ssh_tunnel: None,
        });
    }
    Ok(preview)
}
