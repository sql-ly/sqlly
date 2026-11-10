//! Azure Data Studio and VS Code (MSSQL extension) user `settings.json`.
//!
//! Azure Data Studio keeps saved connections in `datasource.connections`
//! (each `{providerName, groupId, options: {...}}`) and folders in
//! `datasource.connectionGroups`; the VS Code MSSQL extension keeps flat
//! `mssql.connections` entries (and `mssql.connectionGroups` in newer
//! releases). Both live in the editor's settings file, which is JSONC:
//! comments and trailing commas are allowed, so they are stripped first.

use std::collections::{HashMap, HashSet};

use serde_json::Value;
use sqlly_lang::sqlmodel::DatabaseType;

use super::pgadmin::{str_field, u16_field};
use super::{ImportedConnection, SkippedConnection, ToolFileKind, ToolImportPreview, skip_reason};
use crate::connection::ConnectionEnvironment;
use crate::connection_url::ParsedConnectionUrl;

pub(crate) const ADS_CONNECTIONS: &str = "datasource.connections";
pub(crate) const ADS_GROUPS: &str = "datasource.connectionGroups";
pub(crate) const VSCODE_CONNECTIONS: &str = "mssql.connections";
pub(crate) const VSCODE_GROUPS: &str = "mssql.connectionGroups";

pub(super) fn looks_like(contents: &str) -> bool {
    serde_json::from_str::<Value>(&strip_jsonc(contents))
        .ok()
        .is_some_and(|v| {
            v.get(ADS_CONNECTIONS).is_some_and(Value::is_array)
                || v.get(VSCODE_CONNECTIONS).is_some_and(Value::is_array)
        })
}

pub(super) fn parse(contents: &str) -> Result<ToolImportPreview, String> {
    let root: Value = serde_json::from_str(&strip_jsonc(contents)).map_err(|e| e.to_string())?;
    let mut preview = ToolImportPreview {
        kind: Some(ToolFileKind::AzureDataStudioSettingsJson),
        ..Default::default()
    };
    // The same server saved in both lists (a file written for both editors)
    // is imported once, from the Azure Data Studio entry.
    let mut seen = HashSet::new();

    let ads_groups = group_names(root.get(ADS_GROUPS));
    for entry in root
        .get(ADS_CONNECTIONS)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let options = entry.get("options").unwrap_or(&Value::Null);
        let provider = str_field(entry, "providerName").unwrap_or_else(|| "MSSQL".to_string());
        let group = str_field(entry, "groupId").and_then(|id| ads_groups.get(&id).cloned());
        push(&mut preview, &mut seen, options, &provider, group);
    }

    let vscode_groups = group_names(root.get(VSCODE_GROUPS));
    for entry in root
        .get(VSCODE_CONNECTIONS)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let group = str_field(entry, "groupId").and_then(|id| vscode_groups.get(&id).cloned());
        push(&mut preview, &mut seen, entry, "MSSQL", group);
    }
    Ok(preview)
}

fn push(
    preview: &mut ToolImportPreview,
    seen: &mut HashSet<String>,
    options: &Value,
    provider: &str,
    group: Option<String>,
) {
    let server_field = str_field(options, "server").or_else(|| str_field(options, "host"));
    let name = str_field(options, "connectionName")
        .or_else(|| str_field(options, "profileName"))
        .or_else(|| server_field.clone())
        .unwrap_or_else(|| "Unnamed".to_string());
    let Some(engine) = provider_engine(provider) else {
        preview.skipped.push(SkippedConnection {
            name,
            reason: skip_reason(
                "services.tool_import.skip_reasons.ads_provider_unsupported",
                format!("unsupported connection provider: {provider}"),
            )
            .with_arg("provider", provider),
        });
        return;
    };
    let Some(server_field) = server_field else {
        preview.skipped.push(SkippedConnection {
            name,
            reason: skip_reason("services.tool_import.skip_reasons.no_host", "no host"),
        });
        return;
    };
    let (server, comma_port) = split_server(&server_field, engine);
    let port = u16_field(options, "port").or(comma_port);
    let database = str_field(options, "database").or_else(|| str_field(options, "dbname"));
    let auth = str_field(options, "authenticationType").unwrap_or_default();
    let integrated = auth.eq_ignore_ascii_case("Integrated");
    let username = if integrated {
        None
    } else {
        str_field(options, "user").or_else(|| str_field(options, "email"))
    };
    let key = format!(
        "{:?}|{}|{:?}|{}",
        engine,
        server.to_ascii_lowercase(),
        port,
        database.as_deref().unwrap_or("").to_ascii_lowercase()
    );
    if !seen.insert(key) {
        return;
    }
    let had_password = options
        .get("password")
        .and_then(Value::as_str)
        .is_some_and(|p| !p.is_empty());
    let parsed = ParsedConnectionUrl {
        engine,
        server,
        port,
        database,
        username,
        encrypt: encrypt_flag(options.get("encrypt")),
        trust_server_certificate: options
            .get("trustServerCertificate")
            .and_then(Value::as_bool),
        tls_mode: str_field(options, "sslmode")
            .as_deref()
            .and_then(super::libpq_tls_mode),
        integrated_security: integrated,
        ..Default::default()
    };
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

fn provider_engine(provider: &str) -> Option<DatabaseType> {
    match provider.to_ascii_uppercase().as_str() {
        "MSSQL" | "" => Some(DatabaseType::SqlServer),
        "PGSQL" | "POSTGRES" | "POSTGRESQL" => Some(DatabaseType::Postgres),
        "MYSQL" => Some(DatabaseType::MySql),
        _ => None,
    }
}

/// SQL Server tools write a non-default port as `host,port` (and accept a
/// `tcp:` prefix); a named instance stays as `host\instance`.
fn split_server(raw: &str, engine: DatabaseType) -> (String, Option<u16>) {
    let raw = raw.trim();
    let raw = raw
        .strip_prefix("tcp:")
        .or_else(|| raw.strip_prefix("TCP:"))
        .unwrap_or(raw);
    if engine == DatabaseType::SqlServer
        && let Some((host, port)) = raw.rsplit_once(',')
        && let Ok(port) = port.trim().parse::<u16>()
    {
        return (host.trim().to_string(), Some(port));
    }
    (raw.to_string(), None)
}

fn encrypt_flag(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "mandatory" | "strict" | "yes" => Some(true),
            "false" | "optional" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Group id → display name. The tree's invisible `ROOT` group is no folder.
fn group_names(groups: Option<&Value>) -> HashMap<String, String> {
    groups
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|g| {
            let id = str_field(g, "id")?;
            let name = str_field(g, "name")?;
            (!name.eq_ignore_ascii_case("ROOT")).then_some((id, name))
        })
        .collect()
}

/// Removes `//` and `/* */` comments outside strings and commas that sit
/// directly before a closing `}` or `]`, turning a JSONC settings file into
/// JSON `serde_json` accepts.
pub(crate) fn strip_jsonc(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    let mut in_string = false;
    while i < chars.len() {
        let c = chars[i];
        if in_string {
            out.push(c);
            if c == '\\' && i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
                i += 1;
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 2;
            }
            ',' => {
                // Look past whitespace and comments for a closing bracket.
                let mut j = i + 1;
                loop {
                    while j < chars.len() && chars[j].is_whitespace() {
                        j += 1;
                    }
                    if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'/') {
                        while j < chars.len() && chars[j] != '\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if chars.get(j) == Some(&'/') && chars.get(j + 1) == Some(&'*') {
                        j += 2;
                        while j < chars.len()
                            && !(chars[j] == '*' && chars.get(j + 1) == Some(&'/'))
                        {
                            j += 1;
                        }
                        j += 2;
                        continue;
                    }
                    break;
                }
                if !matches!(chars.get(j), Some('}') | Some(']')) {
                    out.push(',');
                }
                i += 1;
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_jsonc_keeps_slashes_inside_strings() {
        let text =
            "{\n  // comment\n  \"url\": \"http://x/*y*/\", /* block */\n  \"a\": [1, 2,],\n}";
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["url"], "http://x/*y*/");
        assert_eq!(value["a"], serde_json::json!([1, 2]));
    }

    #[test]
    fn split_server_reads_comma_port_for_sql_server_only() {
        assert_eq!(
            split_server("tcp:db.example.com,14330", DatabaseType::SqlServer),
            ("db.example.com".to_string(), Some(14330))
        );
        assert_eq!(
            split_server(r"host\SQLEXPRESS", DatabaseType::SqlServer),
            (r"host\SQLEXPRESS".to_string(), None)
        );
    }
}
