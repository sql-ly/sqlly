//! Builds a SQLLY connection file (the format `credential_import` reads) from
//! saved connections the user picked. Both exports write it: Export
//! Connections leaves every secret out, Export Conn w/ Creds reads passwords
//! back out of the OS keyring into clear text. Icons are embedded as base64,
//! and the shared logins, tunnel profiles, and client/project/environment
//! labels those connections use come along with them.

use std::collections::{BTreeMap, HashMap, HashSet};

use sqlly_core::credential_import::{
    CREDENTIAL_FILE_FORMAT, CREDENTIAL_FILE_VERSION, CredentialEntity, CredentialFile,
    CredentialSqlAccount, CredentialTunnelProfile, SecretLookup, encode_image_data_uri,
    export_connection, export_transport, safety_export,
};
use sqlly_core::{AuthenticationType, ConnectionProfile, SecretProvider, SecretRef};
use uuid::Uuid;

use crate::connection::ConnectionStore;

/// One saved connection under a server in the export picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportDatabaseNode {
    pub id: Uuid,
    /// The database, or "(default database)" when the connection has none.
    pub label: String,
    /// The connection's name.
    pub detail: String,
}

/// A server (engine + host + port) and the saved connections to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportServerNode {
    pub label: String,
    pub databases: Vec<ExportDatabaseNode>,
}

/// Groups saved connections into the picker's server → database tree,
/// servers sorted by label and databases by label within each server.
pub fn export_tree(profiles: &[ConnectionProfile]) -> Vec<ExportServerNode> {
    let mut servers: BTreeMap<String, (String, Vec<ExportDatabaseNode>)> = BTreeMap::new();
    for profile in profiles {
        let file_engine = crate::engine_form::is_file_engine(profile.database_type);
        let label = if file_engine || profile.port == 0 {
            format!(
                "{} · {}",
                profile.server,
                profile.database_type.display_name()
            )
        } else {
            format!(
                "{}:{} · {}",
                profile.server,
                profile.port,
                profile.database_type.display_name()
            )
        };
        let key = label.to_lowercase();
        servers
            .entry(key)
            .or_insert_with(|| (label, Vec::new()))
            .1
            .push(ExportDatabaseNode {
                id: profile.id.0,
                label: profile
                    .database
                    .clone()
                    .filter(|d| !d.trim().is_empty())
                    .unwrap_or_else(|| {
                        crate::localization::text("connections.credentials.default_database")
                    }),
                detail: profile.name.clone(),
            });
    }
    servers
        .into_values()
        .map(|(label, mut databases)| {
            databases.sort_by(|a, b| {
                a.label
                    .to_lowercase()
                    .cmp(&b.label.to_lowercase())
                    .then_with(|| a.detail.cmp(&b.detail))
            });
            ExportServerNode { label, databases }
        })
        .collect()
}

/// Whether an export writes passwords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportSecrets {
    /// Export Conn w/ Creds: passwords in clear text.
    Include,
    /// Export Connections: every secret field left out.
    Omit,
}

/// What an export wrote, for the status line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialExportSummary {
    pub connections: usize,
    /// Connections whose password could not be read (exported without one).
    /// Always zero for an export that omits secrets.
    pub missing_passwords: usize,
}

/// Reads a stored secret back to clear text. Keyring references resolve
/// through the keyring; a non-empty in-memory value is already clear text.
fn lookup_secret(secret_ref: &SecretRef) -> Option<String> {
    match secret_ref.provider {
        SecretProvider::MacOsKeychain => Uuid::parse_str(&secret_ref.key)
            .ok()
            .and_then(|id| crate::sql_credentials::load_password(id).ok().flatten()),
        SecretProvider::InMemory => Some(secret_ref.key.clone()).filter(|k| !k.is_empty()),
        SecretProvider::Environment => std::env::var(&secret_ref.key).ok(),
    }
}

fn keychain_account(profile: &ConnectionProfile) -> Option<Uuid> {
    match &profile.authentication {
        AuthenticationType::SqlLogin { secret_ref, .. }
            if secret_ref.provider == SecretProvider::MacOsKeychain =>
        {
            Uuid::parse_str(&secret_ref.key).ok()
        }
        _ => None,
    }
}

fn entity_export(
    name: &str,
    color_hex: &Option<String>,
    image_path: &Option<String>,
    safety: &crate::connection::SafetyFeatures,
) -> CredentialEntity {
    let image = image_path
        .as_deref()
        .filter(|p| !p.trim().is_empty())
        .and_then(|path| std::fs::read(path).ok())
        .and_then(|bytes| encode_image_data_uri(&bytes));
    CredentialEntity {
        name: name.to_string(),
        color: color_hex.clone(),
        image,
        safety: safety_export(&crate::connection::export_safety_features(safety)),
    }
}

/// Builds the credential file for the selected connections, resolving
/// passwords with `lookup` (the keyring in the app).
///
/// The labels (clients, projects, environments) and the shared logins always
/// go in whole, whichever connections are picked, so the file carries the
/// whole organisation the way the old connection bundle did. Secrets are
/// only looked up for what a picked connection uses: a shared login no
/// picked connection signs in with is written without its password.
///
/// A shared login is a saved login that two or more connections sign in
/// with, or none does (one saved on its own). A login exactly one
/// connection uses is that connection's own saved password (the store pairs
/// one with every SQL login); it goes inline on the connection when that
/// connection is picked, and not at all otherwise.
pub fn build_credential_file_with(
    store: &ConnectionStore,
    selected: &HashSet<Uuid>,
    lookup: &mut SecretLookup<'_>,
) -> (CredentialFile, CredentialExportSummary) {
    let profiles: Vec<ConnectionProfile> = store
        .list()
        .into_iter()
        .filter(|p| selected.contains(&p.id.0))
        .collect();

    // How many connections in the whole store sign in with each login
    // (shared or a connection's own), and how many of the picked ones do.
    let mut store_uses: HashMap<Uuid, usize> = HashMap::new();
    for profile in &store.list() {
        if let Some(id) = keychain_account(profile) {
            *store_uses.entry(id).or_default() += 1;
        }
    }
    let mut uses: HashMap<Uuid, usize> = HashMap::new();
    for profile in &profiles {
        if let Some(id) = keychain_account(profile) {
            *uses.entry(id).or_default() += 1;
        }
    }
    let saved_accounts = store.sql_auth_accounts();
    let mut shared: HashMap<Uuid, String> = HashMap::new();
    let mut sql_auth_accounts = Vec::new();
    let mut taken_names: HashSet<String> = HashSet::new();
    for account in &saved_accounts {
        if store_uses.get(&account.id).copied().unwrap_or(0) == 1 {
            // One connection's own saved password: inline on it, if picked.
            continue;
        }
        if account.name.trim().is_empty() || account.username.trim().is_empty() {
            // The importer rejects a login without both; there is nothing
            // to carry.
            continue;
        }
        let mut name = account.name.clone();
        let mut n = 2;
        while !taken_names.insert(name.to_lowercase()) {
            name = format!("{} ({n})", account.name);
            n += 1;
        }
        let used = uses.get(&account.id).copied().unwrap_or(0) > 0;
        let password = used
            .then(|| {
                lookup(&SecretRef {
                    provider: SecretProvider::MacOsKeychain,
                    key: account.id.to_string(),
                })
            })
            .flatten()
            .filter(|p| !p.is_empty());
        shared.insert(account.id, name.clone());
        sql_auth_accounts.push(CredentialSqlAccount {
            name,
            username: account.username.clone(),
            password,
        });
    }

    let saved_tunnels = store.tunnel_profiles();
    let mut tunnel_profiles = Vec::new();
    let mut tunnel_names: HashMap<Uuid, String> = HashMap::new();
    for profile in &profiles {
        let Some(id) = profile.tunnel_profile else {
            continue;
        };
        if tunnel_names.contains_key(&id) {
            continue;
        }
        if let Some(tunnel) = saved_tunnels.iter().find(|t| t.id == id) {
            let transport = export_transport(&tunnel.chain, lookup);
            tunnel_names.insert(id, tunnel.name.clone());
            tunnel_profiles.push(CredentialTunnelProfile {
                name: tunnel.name.clone(),
                proxy: transport.proxy,
                ssh: transport.ssh,
                kube: transport.kube,
                ssm: transport.ssm,
            });
        }
    }

    let mut summary = CredentialExportSummary {
        connections: profiles.len(),
        missing_passwords: 0,
    };
    let connections = profiles
        .iter()
        .map(|profile| {
            let account = keychain_account(profile).and_then(|id| shared.get(&id));
            let tunnel = profile.tunnel_profile.and_then(|id| tunnel_names.get(&id));
            let exported = export_connection(
                profile,
                account.map(String::as_str),
                tunnel.map(String::as_str),
                lookup,
            );
            if let Some(sqlly_core::credential_import::CredentialAuth::SqlLogin {
                password: None,
                account: None,
                ..
            }) = &exported.authentication
            {
                summary.missing_passwords += 1;
            }
            exported
        })
        .collect();

    let clients = store
        .clients()
        .iter()
        .map(|e| entity_export(&e.name, &e.color_hex, &e.image_path, &e.safety))
        .collect();
    let projects = store
        .projects()
        .iter()
        .map(|e| entity_export(&e.name, &e.color_hex, &e.image_path, &e.safety))
        .collect();
    let environments = store
        .environments()
        .iter()
        .map(|e| entity_export(&e.name, &e.color_hex, &e.image_path, &e.safety))
        .collect();

    (
        CredentialFile {
            format: CREDENTIAL_FILE_FORMAT.to_string(),
            version: CREDENTIAL_FILE_VERSION,
            clients,
            projects,
            environments,
            sql_auth_accounts,
            tunnel_profiles,
            connections,
        },
        summary,
    )
}

/// True when the store holds anything an export would carry: a connection,
/// a label, an environment beyond the built-in ones, or a saved login. A
/// store with nothing but its built-in environments has nothing to export.
pub fn store_has_exportable_content(store: &ConnectionStore) -> bool {
    let built_in = crate::connection::built_in_environments();
    !store.list().is_empty()
        || !store.clients().is_empty()
        || !store.projects().is_empty()
        || store.environments().iter().any(|e| !built_in.contains(e))
        || !store.sql_auth_accounts().is_empty()
}

/// Builds the connection file JSON for the selected connections. With
/// [`ExportSecrets::Include`] passwords are read from the system keychain;
/// with [`ExportSecrets::Omit`] no secret is ever looked up, and the result
/// is checked to hold no secret field before it is returned.
pub fn export_connections_json(
    store: &ConnectionStore,
    selected: &HashSet<Uuid>,
    secrets: ExportSecrets,
) -> Result<(String, CredentialExportSummary), String> {
    let (file, mut summary) = match secrets {
        ExportSecrets::Include => build_credential_file_with(store, selected, &mut lookup_secret),
        ExportSecrets::Omit => build_credential_file_with(store, selected, &mut |_| None),
    };
    let json = serde_json::to_string_pretty(&file).map_err(|error| {
        crate::localization::format(
            "connections.credentials.export_write_failed",
            &[("error", &error.to_string())],
        )
    })?;
    if secrets == ExportSecrets::Omit {
        if !connection_file_json_is_secret_free(&json) {
            return Err(crate::localization::text(
                "connections.credentials.export_secret_material_refused",
            ));
        }
        summary.missing_passwords = 0;
    }
    Ok((json, summary))
}

/// Field names that carry a secret anywhere in a connection file.
const SECRET_FIELDS: &[&str] = &["password", "passphrase", "token", "redis_sentinel_password"];

/// True when no secret-bearing field in a connection file has a value.
/// Walks the JSON structure (a `"type": "password"` sign-in method is a
/// value, not a secret), so it can't be fooled by text that merely looks
/// like one.
pub fn connection_file_json_is_secret_free(json: &str) -> bool {
    fn clean(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(map) => map.iter().all(|(key, value)| {
                let secret = SECRET_FIELDS.contains(&key.as_str())
                    && !matches!(value, serde_json::Value::Null);
                !secret && clean(value)
            }),
            serde_json::Value::Array(items) => items.iter().all(clean),
            _ => true,
        }
    }
    serde_json::from_str::<serde_json::Value>(json).is_ok_and(|value| clean(&value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::DatabaseType;

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn store_with(file: &str) -> (tempfile::TempDir, ConnectionStore) {
        keyring::mock::install();
        let dir = tempfile::tempdir().unwrap();
        let store = ConnectionStore::from_path(dir.path().join("connections.json"));
        crate::credential_import::import_credential_text(file, &store).unwrap();
        (dir, store)
    }

    fn source_file() -> String {
        format!(
            r##"{{ "format": "sqlly-credentials", "version": 1,
  "clients": [{{ "name": "Acme", "color": "#1E88E5", "image": "{PNG_B64}" }}, {{ "name": "Other" }}],
  "sql_auth_accounts": [{{ "name": "Reader", "username": "ro", "password": "ro-pass" }}],
  "tunnel_profiles": [{{ "name": "Bastion", "ssh": {{ "host": "b", "username": "u", "auth": {{ "type": "password", "password": "ssh-pass" }} }} }}],
  "connections": [
    {{ "name": "A sales", "engine": "postgres", "server": "a.example.com", "database": "sales", "client": "Acme",
       "authentication": {{ "type": "sql-login", "account": "Reader" }}, "tunnel_profile": "Bastion" }},
    {{ "name": "A hr", "engine": "postgres", "server": "a.example.com", "database": "hr", "client": "Acme",
       "authentication": {{ "type": "sql-login", "account": "Reader" }} }},
    {{ "name": "B ops", "engine": "sql-server", "server": "b.example.com", "database": "ops", "environment": "production",
       "authentication": {{ "type": "sql-login", "username": "ops", "password": "ops-pass" }} }},
    {{ "name": "Scratch", "engine": "sqlite", "server": "/tmp/x.db" }}
  ] }}"##
        )
    }

    #[test]
    fn tree_groups_databases_under_their_server() {
        let (_dir, store) = store_with(&source_file());
        let tree = export_tree(&store.list());
        let labels: Vec<_> = tree.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "/tmp/x.db · SQLite",
                "a.example.com:5432 · PostgreSQL",
                "b.example.com:1433 · Microsoft SQL Server",
            ]
        );
        let a: Vec<_> = tree[1].databases.iter().map(|d| d.label.as_str()).collect();
        assert_eq!(a, vec!["hr", "sales"]);
        assert_eq!(tree[0].databases[0].label, "(default database)");
    }

    #[test]
    fn export_reads_passwords_back_and_imports_into_a_fresh_store() {
        let (_dir, store) = store_with(&source_file());
        let all: HashSet<Uuid> = store.list().iter().map(|p| p.id.0).collect();
        let (json, summary) =
            export_connections_json(&store, &all, ExportSecrets::Include).unwrap();
        assert_eq!(summary.connections, 4);
        assert_eq!(summary.missing_passwords, 0);
        for secret in ["ro-pass", "ssh-pass", "ops-pass"] {
            assert!(json.contains(secret), "{secret} missing from export");
        }
        assert!(json.contains("data:image/png;base64,"));
        assert!(
            json.contains("\"Other\""),
            "every label is exported, used or not"
        );
        assert!(
            json.contains("\"account\": \"Reader\""),
            "shared login stays shared"
        );

        let dir = tempfile::tempdir().unwrap();
        let target = ConnectionStore::from_path(dir.path().join("connections.json"));
        crate::credential_import::import_credential_text(&json, &target).unwrap();
        let mut names: Vec<_> = target.list().into_iter().map(|p| p.name).collect();
        names.sort();
        assert_eq!(names, vec!["A hr", "A sales", "B ops", "Scratch"]);
        let ops = target
            .list()
            .into_iter()
            .find(|p| p.name == "B ops")
            .unwrap();
        assert_eq!(ops.database_type, DatabaseType::SqlServer);
        let hydrated = crate::sql_credentials::hydrate_profile(ops).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = hydrated.authentication else {
            panic!("sql login");
        };
        assert_eq!(secret_ref.key, "ops-pass");
        let acme = target
            .clients()
            .into_iter()
            .find(|c| c.name == "Acme")
            .unwrap();
        assert_eq!(acme.color_hex.as_deref(), Some("#1E88E5"));
        assert!(acme.image_path.is_some());
    }

    /// Every kind of secret the store can hold, each with a value no other
    /// field could contain by accident.
    const SECRETS: &[&str] = &[
        "s1-inline-sql",
        "s2-shared-login",
        "s3-ssh-password",
        "s4-key-passphrase",
        "s5-jump-password",
        "s6-proxy-password",
        "s7-libsql-token",
    ];

    fn every_secret_file() -> String {
        r##"{ "format": "sqlly-credentials", "version": 1,
  "sql_auth_accounts": [{ "name": "Shared", "username": "sh", "password": "s2-shared-login" }],
  "tunnel_profiles": [{ "name": "Bastion", "ssh": { "host": "b", "username": "u",
      "auth": { "type": "password", "password": "s3-ssh-password" },
      "jump": { "host": "j", "username": "ju", "auth": { "type": "password", "password": "s5-jump-password" } } } }],
  "connections": [
    { "name": "Inline", "engine": "postgres", "server": "a", "database": "d1",
      "authentication": { "type": "sql-login", "username": "app", "password": "s1-inline-sql" },
      "transport": { "proxy": { "kind": "http-connect", "host": "p", "port": 8080, "username": "pu", "password": "s6-proxy-password" } } },
    { "name": "Shared one", "engine": "postgres", "server": "a", "database": "d2",
      "authentication": { "type": "sql-login", "account": "Shared" }, "tunnel_profile": "Bastion" },
    { "name": "Shared two", "engine": "postgres", "server": "a", "database": "d3",
      "authentication": { "type": "sql-login", "account": "Shared" },
      "transport": { "ssh": { "host": "k", "username": "ku", "auth": { "type": "key-file", "path": "/tmp/id", "passphrase": "s4-key-passphrase" } } } },
    { "name": "Turso", "engine": "libsql", "server": "db.turso.io", "port": 443,
      "authentication": { "type": "auth-token", "token": "s7-libsql-token" } }
  ] }"##
            .to_string()
    }

    #[test]
    fn export_without_credentials_writes_no_secret_at_all() {
        let (_dir, store) = store_with(&every_secret_file());
        let all: HashSet<Uuid> = store.list().iter().map(|p| p.id.0).collect();

        let (with, _) = export_connections_json(&store, &all, ExportSecrets::Include).unwrap();
        for secret in SECRETS {
            assert!(
                with.contains(secret),
                "{secret} missing from the credentials export"
            );
        }
        assert!(!connection_file_json_is_secret_free(&with));

        let (without, summary) =
            export_connections_json(&store, &all, ExportSecrets::Omit).unwrap();
        assert_eq!(summary.connections, 4);
        assert_eq!(summary.missing_passwords, 0);
        for secret in SECRETS {
            assert!(!without.contains(secret), "{secret} leaked:\n{without}");
        }
        assert!(connection_file_json_is_secret_free(&without));
        // Same format, same settings: sign-in methods and shared names stay.
        assert!(sqlly_core::credential_import::is_credential_file(&without));
        assert!(without.contains("\"type\": \"password\""), "{without}");
        assert!(without.contains("\"account\": \"Shared\""), "{without}");
        assert!(without.contains("\"path\": \"/tmp/id\""), "{without}");
    }

    #[test]
    fn export_without_credentials_imports_with_passwords_left_to_ask() {
        let (_dir, store) = store_with(&every_secret_file());
        let all: HashSet<Uuid> = store.list().iter().map(|p| p.id.0).collect();
        let (json, _) = export_connections_json(&store, &all, ExportSecrets::Omit).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let target = ConnectionStore::from_path(dir.path().join("connections.json"));
        let summary = crate::credential_import::import_credential_text(&json, &target).unwrap();
        assert_eq!(summary.connections, 4);
        assert_eq!(summary.passwords, 0, "nothing was written to the keychain");
        let mut names: Vec<_> = target.list().into_iter().map(|p| p.name).collect();
        names.sort();
        assert_eq!(names, vec!["Inline", "Shared one", "Shared two", "Turso"]);
        let tunnel = target
            .tunnel_profiles()
            .into_iter()
            .find(|t| t.name == "Bastion")
            .unwrap();
        assert!(matches!(
            tunnel.chain.ssh.unwrap().target.auth,
            sqlly_core::connection::SshAuth::Password { .. }
        ));
    }

    #[test]
    fn secret_check_reads_structure_not_text() {
        assert!(connection_file_json_is_secret_free(
            r#"{ "auth": { "type": "password" }, "note": "password" }"#
        ));
        assert!(connection_file_json_is_secret_free(
            r#"{ "password": null }"#
        ));
        assert!(!connection_file_json_is_secret_free(
            r#"{ "a": [{ "token": "x" }] }"#
        ));
        assert!(!connection_file_json_is_secret_free(
            r#"{ "passphrase": "" }"#
        ));
        assert!(!connection_file_json_is_secret_free("not json"));
    }

    #[test]
    fn exporting_a_subset_takes_only_what_it_uses() {
        let (_dir, store) = store_with(&source_file());
        let pick: HashSet<Uuid> = store
            .list()
            .iter()
            .filter(|p| p.name == "A hr")
            .map(|p| p.id.0)
            .collect();
        let (file, _) = build_credential_file_with(&store, &pick, &mut lookup_secret);
        assert_eq!(file.connections.len(), 1);
        // The picked connection's login is shared by two connections in the
        // store, so it stays shared by name, with its password. B ops' own
        // saved password isn't carried (B ops wasn't picked), and there's no
        // tunnel profile, since nothing picked uses one.
        assert!(!serde_json::to_string(&file).unwrap().contains("ops-pass"));
        assert_eq!(file.sql_auth_accounts.len(), 1);
        assert_eq!(file.sql_auth_accounts[0].name, "Reader");
        assert_eq!(
            file.sql_auth_accounts[0].password.as_deref(),
            Some("ro-pass")
        );
        assert!(file.tunnel_profiles.is_empty());
        // Labels go in whole, used or not.
        assert_eq!(file.clients.len(), 2);
    }

    #[test]
    fn a_login_nothing_picked_uses_goes_without_its_password() {
        let (_dir, store) = store_with(&source_file());
        let pick: HashSet<Uuid> = store
            .list()
            .iter()
            .filter(|p| p.name == "Scratch")
            .map(|p| p.id.0)
            .collect();
        let (file, _) = build_credential_file_with(&store, &pick, &mut lookup_secret);
        assert_eq!(file.sql_auth_accounts.len(), 1);
        assert_eq!(file.sql_auth_accounts[0].name, "Reader");
        assert_eq!(file.sql_auth_accounts[0].password, None);
        let json = serde_json::to_string(&file).unwrap();
        assert!(!json.contains("ro-pass"));
        assert_eq!(file.clients.len(), 2);
    }

    #[test]
    fn a_store_with_only_labels_exports_them() {
        keyring::mock::install();
        let dir = tempfile::tempdir().unwrap();
        let store = ConnectionStore::from_path(dir.path().join("connections.json"));
        crate::credential_import::import_credential_text(
            r##"{ "format": "sqlly-credentials", "version": 1,
  "clients": [{ "name": "Acme", "color": "#1E88E5" }],
  "projects": [{ "name": "Billing" }],
  "sql_auth_accounts": [{ "name": "Shared", "username": "sh", "password": "sh-pass" }],
  "connections": [] }"##,
            &store,
        )
        .unwrap();
        assert!(store.list().is_empty());
        assert!(store_has_exportable_content(&store));
        for secrets in [ExportSecrets::Omit, ExportSecrets::Include] {
            let (json, summary) =
                export_connections_json(&store, &HashSet::new(), secrets).unwrap();
            assert_eq!(summary.connections, 0);
            assert!(json.contains("\"Acme\"") && json.contains("\"Billing\""));
            assert!(json.contains("\"Shared\""));
            assert!(
                !json.contains("sh-pass"),
                "{secrets:?}: unused login keeps its password"
            );
        }

        let empty_dir = tempfile::tempdir().unwrap();
        let empty = ConnectionStore::from_path(empty_dir.path().join("connections.json"));
        assert!(!store_has_exportable_content(&empty));
    }
}
