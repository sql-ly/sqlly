//! Import Redis Insight's "Export databases" file.
//!
//! The format was checked against Redis Insight's own source (the
//! `ExportDatabase` model and `DatabaseImportService`, RedisInsight `main`,
//! October 2026): a JSON array — or the same array base64-encoded — of
//! database objects with `host`, `port`, `name`, `db`, `username`,
//! `password` (present only when exported "with secrets"),
//! `connectionType` (`STANDALONE` / `CLUSTER` / `SENTINEL`, or the legacy
//! `type` / `cluster` spellings the import side also reads),
//! `forceStandalone`, `tls`,
//! `tlsServername`, `verifyServerCert`, `caCert {name, certificate}`,
//! `clientCert {name, certificate, key}`, `ssh` + `sshOptions {host, port,
//! username, password, privateKey, passphrase}`, `sentinelMaster {name,
//! username, password}` (legacy: `sentinelOptions {masterName | name,
//! nodePassword, sentinelPassword}`), and
//! `providerDetails {provider: "azure", authType: "entraId", …}` for
//! Entra-signed Azure databases (whose credentials Redis Insight always
//! strips). The import side also accepts a few legacy spellings (`auth`,
//! `ssl`, `connectionName`, `cluster`); those are honoured here too.
//!
//! The output is a SQLLY credential file (`sqlly-credentials`), so the
//! existing credential importer moves every password, SSH password and key
//! passphrase into the keychain — none lands in the profile file. TLS is
//! never dropped: `tls` (or a CA / client certificate) means encryption,
//! `verifyServerCert` maps to Verify CA (chain checked) and its absence to
//! Require (encrypted, unverified) — the same split Redis Insight makes.
//! Certificate bodies are written to PEM files the caller places under the
//! app's data directory (keys owner-only), and the profile points at them.
//! A Sentinel database becomes a Sentinel connection: its `host:port` is the
//! sentinel to ask, the top-level user name and password are the
//! sentinel's own (Redis Insight signs in to the sentinel with them; SQLLY
//! keeps the password, moved to the keychain, and notes a sentinel user
//! name it cannot use), and `sentinelMaster`'s user name and password sign
//! in to the master. A cluster database becomes a Redis Cluster connection
//! seeded with its `host:port` (the export carries no node list; the client
//! discovers the other nodes), unless `forceStandalone` asks for one node.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{Map, Value, json};

use super::SkippedConnection;

/// A PEM file the caller writes before importing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PemFile {
    pub path: PathBuf,
    pub contents: String,
    /// A private key: write it readable by the owner only.
    pub private: bool,
}

/// The converted export.
#[derive(Debug, Clone, PartialEq)]
pub struct RedisInsightImport {
    /// A `sqlly-credentials` file for the credential importer.
    pub credential_file: Value,
    pub pem_files: Vec<PemFile>,
    pub skipped: Vec<SkippedConnection>,
    /// One line per connection that imported with a caveat: keyed sentences
    /// the UI words in its language (the fallback is English).
    pub notes: Vec<crate::DisplayMessage>,
}

/// The export's items: a JSON array, or the same array base64-encoded.
pub fn export_items(contents: &str) -> Option<Vec<Value>> {
    let trimmed = contents.trim().trim_start_matches('\u{feff}');
    let parsed = serde_json::from_str::<Value>(trimmed).ok().or_else(|| {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(trimmed.as_bytes())
            .ok()?;
        serde_json::from_slice::<Value>(&bytes).ok()
    })?;
    match parsed {
        Value::Array(items) => Some(items),
        _ => None,
    }
}

/// Whether `contents` is a Redis Insight export: an array whose objects
/// carry `host` plus at least one field only Redis Insight writes.
pub fn looks_like_redis_insight(contents: &str) -> bool {
    const MARKERS: [&str; 9] = [
        "connectionType",
        "verifyServerCert",
        "tlsServername",
        "nameFromProvider",
        "sentinelMaster",
        "forceStandalone",
        "compressor",
        "caCert",
        "providerDetails",
    ];
    export_items(contents).is_some_and(|items| {
        !items.is_empty()
            && items.iter().all(|item| item.get("host").is_some())
            && items
                .iter()
                .any(|item| MARKERS.iter().any(|m| item.get(m).is_some()))
    })
}

fn text(item: &Value, paths: &[&str]) -> Option<String> {
    paths.iter().find_map(|path| {
        let mut value = item;
        for part in path.split('.') {
            value = value.get(part)?;
        }
        match value {
            Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    })
}

fn flag(item: &Value, paths: &[&str]) -> Option<bool> {
    paths.iter().find_map(|path| {
        let mut value = item;
        for part in path.split('.') {
            value = value.get(part)?;
        }
        value.as_bool()
    })
}

/// A file-name-safe stem.
fn slug(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "redis".into()
    } else {
        slug
    }
}

/// A certificate field: a PEM body to write, or a path to keep.
fn pem_or_path(
    value: String,
    dir: &Path,
    stem: &str,
    suffix: &str,
    private: bool,
    files: &mut Vec<PemFile>,
) -> String {
    if value.contains("-----BEGIN") {
        let path = dir.join(format!("{stem}-{suffix}.pem"));
        files.push(PemFile {
            path: path.clone(),
            contents: value,
            private,
        });
        path.display().to_string()
    } else {
        value
    }
}

/// Convert an export into a credential file plus the PEM files it names.
/// `pem_dir` is where the caller will write those files.
pub fn convert(contents: &str, pem_dir: &Path) -> Result<RedisInsightImport, String> {
    let items = export_items(contents).ok_or_else(|| {
        "not a Redis Insight export (expected a JSON array of databases)".to_string()
    })?;
    let mut connections = Vec::new();
    let mut pem_files = Vec::new();
    let mut skipped = Vec::new();
    let mut notes = Vec::new();
    let mut used_stems = std::collections::HashSet::new();
    for item in &items {
        let host = text(item, &["host"]).unwrap_or_default();
        let port = text(item, &["port"])
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(6379);
        let name =
            text(item, &["name", "connectionName"]).unwrap_or_else(|| format!("{host}:{port}"));
        if host.trim().is_empty() {
            skipped.push(SkippedConnection {
                name,
                reason: crate::DisplayMessage::new(
                    "services.tool_import.skip_reasons.no_host",
                    "no host",
                ),
            });
            continue;
        }
        let connection_type = text(item, &["connectionType", "type"])
            .unwrap_or_default()
            .to_ascii_uppercase();
        // The same order Redis Insight's own import decides the type in.
        let sentinel_master = text(
            item,
            &[
                "sentinelMaster.name",
                "sentinelOptions.masterName",
                "sentinelOptions.name",
            ],
        );
        let kind = match connection_type.as_str() {
            "SENTINEL" => "SENTINEL",
            "CLUSTER" => "CLUSTER",
            "STANDALONE" => "STANDALONE",
            _ if flag(item, &["cluster"]) == Some(true) => "CLUSTER",
            _ if sentinel_master.is_some() => "SENTINEL",
            _ => "STANDALONE",
        };
        let sentinel = if kind == "SENTINEL" {
            let Some(master_name) = sentinel_master else {
                skipped.push(SkippedConnection {
                    name,
                    reason: crate::DisplayMessage::new(
                        "services.tool_import.skip_reasons.sentinel_master_missing",
                        "a Sentinel database without a master group name",
                    ),
                });
                continue;
            };
            Some(master_name)
        } else {
            None
        };
        let cluster_mode = if kind == "CLUSTER" {
            if flag(item, &["forceStandalone"]) == Some(true) {
                notes.push(
                    crate::DisplayMessage::new(
                        "services.tool_import.redis_insight.note_forced_standalone",
                        format!(
                            "{name}: a cluster database Redis Insight was told to treat as one node, imported as a standalone connection to {host}:{port}"
                        ),
                    )
                    .with_arg("name", &name)
                    .with_arg("host", &host)
                    .with_arg("port", port.to_string()),
                );
                Some("standalone")
            } else {
                Some("cluster")
            }
        } else {
            None
        };
        let mut stem = slug(&name);
        while !used_stems.insert(stem.clone()) {
            stem.push('-');
            stem.push('x');
        }

        // TLS: never dropped. Certificates imply TLS even without the flag.
        let ca = text(
            item,
            &[
                "caCert.certificate",
                "caCert",
                "sslOptions.ca",
                "ssl_ca_cert_path",
            ],
        );
        let client_cert = text(
            item,
            &[
                "clientCert.certificate",
                "certificate",
                "sslOptions.cert",
                "ssl_local_cert_path",
            ],
        );
        let client_key = text(
            item,
            &[
                "clientCert.key",
                "keyFile",
                "sslOptions.key",
                "ssl_private_key_path",
            ],
        );
        let tls = flag(item, &["tls", "ssl"]).unwrap_or(false)
            || ca.is_some()
            || client_cert.is_some()
            || client_key.is_some();
        let verify =
            flag(item, &["verifyServerCert", "sslOptions.rejectUnauthorized"]).unwrap_or(false);
        let mut tls_json = Map::new();
        if tls {
            tls_json.insert(
                "mode".into(),
                json!(if verify { "verify-ca" } else { "require" }),
            );
            if let Some(ca) = ca {
                tls_json.insert(
                    "ca_file".into(),
                    json!(pem_or_path(ca, pem_dir, &stem, "ca", false, &mut pem_files)),
                );
            }
            if let Some(cert) = client_cert {
                tls_json.insert(
                    "client_cert_file".into(),
                    json!(pem_or_path(
                        cert,
                        pem_dir,
                        &stem,
                        "client",
                        false,
                        &mut pem_files
                    )),
                );
            }
            if let Some(key) = client_key {
                tls_json.insert(
                    "client_key_file".into(),
                    json!(pem_or_path(
                        key,
                        pem_dir,
                        &stem,
                        "client-key",
                        true,
                        &mut pem_files
                    )),
                );
            }
        } else {
            tls_json.insert("mode".into(), json!("disable"));
        }

        let entra = item.get("providerDetails").is_some_and(|d| {
            d.get("provider").and_then(Value::as_str) == Some("azure")
                && d.get("authType").and_then(Value::as_str) == Some("entraId")
        });
        let authentication = if entra {
            notes.push(
                crate::DisplayMessage::new(
                    "services.tool_import.redis_insight.note_entra_sign_in",
                    format!(
                        "{name}: signs in with Microsoft Entra — pick the Azure account in the connection editor"
                    ),
                )
                .with_arg("name", &name),
            );
            json!({"type": "entra-browser"})
        } else if sentinel.is_some() {
            // The master's own credentials; the top-level ones are the
            // sentinel's (below).
            let mut auth = Map::new();
            auth.insert("type".into(), json!("sql-login"));
            let user = text(item, &["sentinelMaster.username"]).unwrap_or_else(|| "default".into());
            auth.insert("username".into(), json!(user));
            if let Some(password) = text(
                item,
                &["sentinelMaster.password", "sentinelOptions.nodePassword"],
            ) {
                auth.insert("password".into(), json!(password));
            }
            Value::Object(auth)
        } else {
            let mut auth = Map::new();
            auth.insert("type".into(), json!("sql-login"));
            // No user name means Redis's `default` user.
            let user = text(item, &["username"]).unwrap_or_else(|| "default".into());
            auth.insert("username".into(), json!(user));
            if let Some(password) = text(item, &["password", "auth"]) {
                auth.insert("password".into(), json!(password));
            }
            Value::Object(auth)
        };

        let mut connection = Map::new();
        connection.insert("name".into(), json!(name));
        connection.insert("engine".into(), json!("redis"));
        connection.insert("server".into(), json!(host));
        connection.insert("port".into(), json!(port));
        if let Some(db) = text(item, &["db"]).filter(|db| db != "0") {
            connection.insert("database".into(), json!(db));
        }
        connection.insert("authentication".into(), authentication);
        connection.insert("tls".into(), Value::Object(tls_json));
        if let Some(master_name) = &sentinel {
            connection.insert(
                "redis_sentinel".into(),
                json!({"master_name": master_name, "sentinels": [format!("{host}:{port}")]}),
            );
            if let Some(password) = text(item, &["password", "sentinelOptions.sentinelPassword"]) {
                connection.insert("redis_sentinel_password".into(), json!(password));
            }
            if let Some(user) = text(item, &["username"]).filter(|u| u != "default") {
                notes.push(
                    crate::DisplayMessage::new(
                        "services.tool_import.redis_insight.note_sentinel_user_dropped",
                        format!(
                            "{name}: the sentinel user name {user} is not carried over (SQLLY signs in to sentinels with the password only)"
                        ),
                    )
                    .with_arg("name", &name)
                    .with_arg("user_name", &user),
                );
            }
        }
        if let Some(mode) = cluster_mode {
            connection.insert("redis_cluster".into(), json!({"mode": mode}));
        }

        let ssh_enabled = flag(item, &["ssh"]).unwrap_or(false)
            || text(item, &["sshOptions.host", "ssh_host", "sshHost"]).is_some();
        if ssh_enabled
            && let Some(ssh_host) = text(item, &["sshOptions.host", "ssh_host", "sshHost"])
        {
            let ssh_port = text(item, &["sshOptions.port", "ssh_port", "sshPort"])
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(22);
            let ssh_user =
                text(item, &["sshOptions.username", "ssh_user", "sshUser"]).unwrap_or_default();
            let key = text(
                item,
                &[
                    "sshOptions.privateKey",
                    "sshOptions.privatekey",
                    "ssh_private_key_path",
                    "sshKeyFile",
                ],
            );
            let auth = if let Some(key) = key {
                let path = pem_or_path(key, pem_dir, &stem, "ssh-key", true, &mut pem_files);
                let mut auth = Map::new();
                auth.insert("type".into(), json!("key-file"));
                auth.insert("path".into(), json!(path));
                if let Some(phrase) = text(item, &["sshOptions.passphrase", "sshKeyPassphrase"]) {
                    auth.insert("passphrase".into(), json!(phrase));
                }
                Value::Object(auth)
            } else if let Some(password) = text(
                item,
                &["sshOptions.password", "ssh_password", "sshPassword"],
            ) {
                json!({"type": "password", "password": password})
            } else {
                json!({"type": "agent"})
            };
            connection.insert(
                "transport".into(),
                json!({"ssh": {"host": ssh_host, "port": ssh_port, "username": ssh_user, "auth": auth}}),
            );
        }
        connections.push(Value::Object(connection));
    }
    Ok(RedisInsightImport {
        credential_file: json!({
            "format": crate::credential_import::CREDENTIAL_FILE_FORMAT,
            "version": crate::credential_import::CREDENTIAL_FILE_VERSION,
            "connections": connections,
        }),
        pem_files,
        skipped,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::{AuthenticationType, SecretProvider, TlsMode};
    use crate::credential_import::parse_credential_file;
    use sqlly_lang::sqlmodel::DatabaseType;

    const FIXTURE: &str =
        include_str!("../../tests/fixtures/tool_import/RedisInsight_connections.json");

    fn plan() -> (
        RedisInsightImport,
        crate::credential_import::CredentialImportPlan,
    ) {
        let import = convert(FIXTURE, Path::new("/certs")).unwrap();
        let text = serde_json::to_string(&import.credential_file).unwrap();
        let plan = parse_credential_file(&text, |_| 6379).expect("valid credential file");
        (import, plan)
    }

    fn profile<'a>(
        plan: &'a crate::credential_import::CredentialImportPlan,
        name: &str,
    ) -> &'a crate::connection::ConnectionProfile {
        &plan
            .connections
            .iter()
            .find(|c| c.profile.name == name)
            .unwrap_or_else(|| panic!("{name} missing"))
            .profile
    }

    #[test]
    fn redis_insight_detection_accepts_json_and_base64_only() {
        assert!(looks_like_redis_insight(FIXTURE));
        let encoded = base64::engine::general_purpose::STANDARD.encode(FIXTURE);
        assert!(looks_like_redis_insight(&encoded));
        assert_eq!(export_items(&encoded).unwrap().len(), 9);
        // A TablePlus array or a DBeaver file is not mistaken for one.
        assert!(!looks_like_redis_insight(
            r#"[{"ConnectionName":"x","DatabaseHost":"h"}]"#
        ));
        assert!(!looks_like_redis_insight(r#"{"connections":{}}"#));
        assert!(!looks_like_redis_insight("[]"));
        assert_eq!(
            super::super::detect_kind("RedisInsight_connections.json", FIXTURE),
            Some(super::super::ToolFileKind::RedisInsightConnectionsJson)
        );
        assert_eq!(
            super::super::detect_kind("export.txt", &encoded),
            Some(super::super::ToolFileKind::RedisInsightConnectionsJson)
        );
    }

    #[test]
    fn redis_insight_tls_is_never_dropped() {
        let (import, plan) = plan();
        let prod = profile(&plan, "Prod sessions");
        assert_eq!(prod.database_type, DatabaseType::Redis);
        assert_eq!(prod.tls_mode, TlsMode::VerifyCa);
        assert_eq!(prod.port, 12345);
        assert_eq!(prod.database.as_deref(), Some("3"));
        assert_eq!(
            prod.tls_ca_file.as_deref(),
            Some(Path::new("/certs/prod-sessions-ca.pem"))
        );
        assert_eq!(
            prod.tls_client_key_file.as_deref(),
            Some(Path::new("/certs/prod-sessions-client-key.pem"))
        );
        let staging = profile(&plan, "Staging via bastion");
        assert_eq!(
            staging.tls_mode,
            TlsMode::Require,
            "tls without verification"
        );
        let local = profile(&plan, "Local cache");
        assert_eq!(local.tls_mode, TlsMode::Disable);
        assert!(local.database.is_none());
        // The key files are owner-only; the CA and the certificate are not.
        let key = import
            .pem_files
            .iter()
            .find(|f| f.path.ends_with("prod-sessions-client-key.pem"))
            .unwrap();
        assert!(key.private && key.contents.contains("PRIVATE KEY"));
        let ca = import
            .pem_files
            .iter()
            .find(|f| f.path.ends_with("prod-sessions-ca.pem"))
            .unwrap();
        assert!(!ca.private);
    }

    #[test]
    fn redis_insight_secrets_stay_out_of_the_profile() {
        let (import, plan) = plan();
        let prod = profile(&plan, "Prod sessions");
        match &prod.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "app");
                // In memory for the importer to move into the keychain.
                assert_eq!(secret_ref.provider, SecretProvider::InMemory);
                assert_eq!(secret_ref.key, "pr0d-s3cret");
            }
            other => panic!("{other:?}"),
        }
        // Every secret arrives as an in-memory reference — the credential
        // importer's job is to move exactly those into the keychain — and
        // none is written into a PEM file or a path.
        let staging = profile(&plan, "Staging via bastion");
        let ssh = staging.transport.ssh.as_ref().expect("ssh kept");
        assert_eq!(ssh.target.host, "bastion.example.com");
        assert_eq!(ssh.target.port, 2222);
        for file in &import.pem_files {
            assert!(!file.contents.contains("st4ging") && !file.contents.contains("k3y-phrase"));
        }
        let azure = profile(&plan, "Azure Managed Redis");
        assert_eq!(azure.authentication, AuthenticationType::EntraBrowser);
        assert_eq!(azure.tls_mode, TlsMode::VerifyCa);
    }

    #[test]
    fn redis_insight_maps_sentinel_and_cluster_databases() {
        use crate::redis_cluster::RedisClusterMode;
        let (import, plan) = plan();
        assert!(import.skipped.is_empty(), "{:?}", import.skipped);
        assert_eq!(plan.connections.len(), 9);

        // Sentinel: the sentinel is the server, the master group and the
        // sentinel password come over, the master's password signs in.
        let ha = profile(&plan, "HA via Sentinel");
        assert_eq!(ha.server, "sentinel-1.internal");
        assert_eq!(ha.port, 26379);
        let sentinel = ha.redis_sentinel.as_ref().expect("sentinel settings");
        assert_eq!(sentinel.master_name, "mymaster");
        assert_eq!(sentinel.sentinels, vec!["sentinel-1.internal:26379"]);
        let sentinel_secret = sentinel
            .sentinel_password_ref
            .as_ref()
            .expect("sentinel password");
        assert_eq!(sentinel_secret.provider, SecretProvider::InMemory);
        assert_eq!(sentinel_secret.key, "s3ntinel");
        match &ha.authentication {
            AuthenticationType::SqlLogin {
                username,
                secret_ref,
            } => {
                assert_eq!(username, "app");
                assert_eq!(secret_ref.key, "m4ster");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            import
                .notes
                .iter()
                .any(|n| n.starts_with("HA via Sentinel") && n.contains("sentinel-admin")),
            "{:?}",
            import.notes
        );
        // A legacy export spelling the master under sentinelOptions.
        let legacy = profile(&plan, "Legacy sentinel");
        let legacy_sentinel = legacy.redis_sentinel.as_ref().expect("legacy sentinel");
        assert_eq!(legacy_sentinel.master_name, "cachegroup");
        match &legacy.authentication {
            AuthenticationType::SqlLogin { secret_ref, .. } => {
                assert_eq!(secret_ref.key, "node-pass")
            }
            other => panic!("{other:?}"),
        }

        // Cluster: mode Cluster seeded from the exported host.
        let orders = profile(&plan, "Orders cluster");
        assert_eq!(orders.server, "cluster-node-1.internal");
        assert_eq!(orders.port, 7000);
        assert_eq!(
            orders.redis_cluster.as_ref().map(|c| c.mode),
            Some(RedisClusterMode::Cluster)
        );
        assert!(orders.redis_sentinel.is_none());
        let legacy_cluster = profile(&plan, "Legacy cluster flag");
        assert_eq!(
            legacy_cluster.redis_cluster.as_ref().map(|c| c.mode),
            Some(RedisClusterMode::Cluster)
        );
        // forceStandalone: one node, said so.
        let pinned = profile(&plan, "Pinned cluster node");
        assert_eq!(
            pinned.redis_cluster.as_ref().map(|c| c.mode),
            Some(RedisClusterMode::Standalone)
        );
        assert!(
            import
                .notes
                .iter()
                .any(|n| n.starts_with("Pinned cluster node")),
            "{:?}",
            import.notes
        );
        // A plain standalone database carries neither.
        let local = profile(&plan, "Local cache");
        assert!(local.redis_cluster.is_none() && local.redis_sentinel.is_none());
    }
}
