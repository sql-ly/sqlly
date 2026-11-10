//! Applies a SQLLY connection file (see `sqlly_core::credential_import`) to
//! the connection store, whether or not it carries passwords: any clear-text
//! secrets move into the OS keyring, embedded
//! icon images are written next to `connections.json`, and everything else is
//! merged in a single save.
//!
//! The import is all-or-nothing: the file is fully validated (including
//! references to saved accounts and tunnel profiles) before anything is
//! written, and a keyring or store failure part-way through restores every
//! keyring entry this import touched.

use std::collections::HashMap;
use std::path::Path;

use sha2::{Digest, Sha256};
use sqlly_core::connection::{SshAuth, SshHop};
use sqlly_core::credential_import::{DecodedImage, PlannedEntity, parse_credential_file};
use sqlly_core::{AuthenticationType, SecretProvider, SecretRef, TransportChain, TunnelProfile};
use uuid::Uuid;

use crate::connection::{ConnectionStore, SecuredCredentialImport, SecuredEntity, SqlAuthAccount};

/// What an import added, for the status line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CredentialImportSummary {
    pub connections: usize,
    pub sql_auth_accounts: usize,
    pub tunnel_profiles: usize,
    pub entities: usize,
    /// SQL logins imported without a password (asked for on first connect).
    pub without_password: usize,
    /// Passwords, passphrases, and tokens the file carried, now in the
    /// system keychain. Zero for a file exported without credentials.
    pub passwords: usize,
}

/// Which file the imported secrets came from: the password sentence tells
/// the user to delete that file, so its wording names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialImportSource {
    /// A SQLLY connection file.
    ConnectionFile,
    /// A Redis Insight export converted by Import From Other Tools.
    RedisInsightExport,
}

impl CredentialImportSummary {
    pub fn message(&self, file: &Path) -> String {
        self.message_for(file, CredentialImportSource::ConnectionFile)
    }

    /// The status line for an import from `source`; every sentence is a
    /// complete catalog message.
    pub fn message_for(&self, file: &Path, source: CredentialImportSource) -> String {
        fn localized(key: &str, count: usize, file: Option<&Path>) -> String {
            let count = count.to_string();
            let file = file
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            let mut arguments = vec![("count", count.as_str())];
            if !file.is_empty() {
                arguments.push(("file_path", file.as_str()));
            }
            crate::localization::format(key, &arguments)
        }
        let mut sentences = vec![localized(
            if self.connections == 1 {
                "connections.credentials.import_connection_singular"
            } else {
                "connections.credentials.import_connection_plural"
            },
            self.connections,
            Some(file),
        )];
        for (key, count) in [
            (
                if self.sql_auth_accounts == 1 {
                    "connections.credentials.import_shared_login_singular"
                } else {
                    "connections.credentials.import_shared_login_plural"
                },
                self.sql_auth_accounts,
            ),
            (
                if self.tunnel_profiles == 1 {
                    "connections.credentials.import_tunnel_singular"
                } else {
                    "connections.credentials.import_tunnel_plural"
                },
                self.tunnel_profiles,
            ),
            (
                if self.entities == 1 {
                    "connections.credentials.import_label_singular"
                } else {
                    "connections.credentials.import_label_plural"
                },
                self.entities,
            ),
        ] {
            if count > 0 {
                sentences.push(localized(key, count, None));
            }
        }
        if self.passwords > 0 {
            let password_key = match (source, self.passwords == 1) {
                (CredentialImportSource::ConnectionFile, true) => {
                    "connections.credentials.import_password_singular"
                }
                (CredentialImportSource::ConnectionFile, false) => {
                    "connections.credentials.import_password_plural"
                }
                (CredentialImportSource::RedisInsightExport, true) => {
                    "connections.credentials.import_password_redis_insight_singular"
                }
                (CredentialImportSource::RedisInsightExport, false) => {
                    "connections.credentials.import_password_redis_insight_plural"
                }
            };
            sentences.push(localized(password_key, self.passwords, None));
        }
        if self.without_password > 0 {
            sentences.push(localized(
                if self.without_password == 1 {
                    "connections.credentials.import_without_password_singular"
                } else {
                    "connections.credentials.import_without_password_plural"
                },
                self.without_password,
                None,
            ));
        }
        sentences.join(" ")
    }
}

/// Import Connections: reads a connection file — the current format with or
/// without passwords, or the older secret-free export bundle — into the
/// store. Returns the status line, or `None` when the file held nothing.
pub fn import_connection_file(
    text: &str,
    path: &Path,
    store: &ConnectionStore,
) -> Result<Option<String>, String> {
    if sqlly_core::credential_import::is_credential_file(text) {
        return import_credential_text(text, store)
            .map(|summary| {
                (summary.connections > 0 || summary.entities > 0).then(|| summary.message(path))
            })
            .map_err(|error| {
                crate::localization::format(
                    "connections.credentials.import_failed",
                    &[("error", &error)],
                )
            });
    }
    // Files written by Export Connections before it moved to the one format.
    if !crate::connection::export_bundle_json_is_secret_free(text) {
        return Err(crate::localization::text(
            "connections.credentials.import_unexpected_secret_fields",
        ));
    }
    let bundle: sqlly_core::ProfileExportBundle = serde_json::from_str(text).map_err(|error| {
        crate::localization::format(
            "connections.credentials.import_invalid_file",
            &[("error", &error.to_string())],
        )
    })?;
    let count = store.import_bundle(&bundle);
    Ok((count > 0).then(|| {
        let key = if count == 1 {
            "connections.credentials.import_legacy_singular"
        } else {
            "connections.credentials.import_legacy_plural"
        };
        crate::localization::format(
            key,
            &[
                ("count", &count.to_string()),
                ("file_path", &path.display().to_string()),
            ],
        )
    }))
}

/// Keyring writes made by one import, so a failure can put every entry back.
#[derive(Default)]
struct KeyringTxn {
    /// (entry id, value before this import wrote it).
    written: Vec<(Uuid, Option<String>)>,
}

impl KeyringTxn {
    fn store(&mut self, id: Uuid, secret: &str) -> Result<(), String> {
        let previous = crate::sql_credentials::load_password(id)?;
        crate::sql_credentials::store_password(id, secret).map_err(|error| {
            crate::localization::format(
                "connections.credentials.import_keychain_save_failed",
                &[("error", &error)],
            )
        })?;
        self.written.push((id, previous));
        Ok(())
    }

    /// Moves an `InMemory` clear-text secret into a fresh keyring entry.
    /// Empty secrets stay `InMemory` so the app asks for them on connect.
    fn secure(&mut self, secret_ref: &mut SecretRef) -> Result<Option<Uuid>, String> {
        if secret_ref.provider != SecretProvider::InMemory || secret_ref.key.is_empty() {
            return Ok(None);
        }
        let id = Uuid::new_v4();
        self.store(id, &secret_ref.key)?;
        *secret_ref = keychain_ref(id);
        Ok(Some(id))
    }

    fn rollback(self) {
        for (id, previous) in self.written.into_iter().rev() {
            let _ = match previous {
                Some(value) => crate::sql_credentials::store_password(id, &value),
                None => crate::sql_credentials::delete_password(id),
            };
        }
    }
}

fn keychain_ref(id: Uuid) -> SecretRef {
    SecretRef {
        provider: SecretProvider::MacOsKeychain,
        key: id.to_string(),
    }
}

fn secure_hop(hop: &mut SshHop, keyring: &mut KeyringTxn) -> Result<(), String> {
    match &mut hop.auth {
        SshAuth::Password { secret_ref } => {
            keyring.secure(secret_ref)?;
        }
        SshAuth::KeyFile {
            passphrase: Some(secret_ref),
            ..
        } => {
            keyring.secure(secret_ref)?;
        }
        _ => {}
    }
    Ok(())
}

fn secure_chain(chain: &mut TransportChain, keyring: &mut KeyringTxn) -> Result<(), String> {
    if let Some(proxy) = chain.proxy.as_mut()
        && let Some(secret_ref) = proxy.password.as_mut()
    {
        keyring.secure(secret_ref)?;
    }
    if let Some(ssh) = chain.ssh.as_mut() {
        secure_hop(&mut ssh.target, keyring)?;
        if let Some(jump) = ssh.jump.as_mut() {
            secure_hop(jump, keyring)?;
        }
    }
    Ok(())
}

/// Writes an icon under a content-addressed name so re-importing the same
/// image reuses one file. Returns the path stored on the entity.
pub(crate) fn write_image(dir: &Path, image: &DecodedImage) -> Result<String, String> {
    let digest = Sha256::digest(&image.bytes);
    let stem: String = digest.iter().take(12).map(|b| format!("{b:02x}")).collect();
    let path = dir.join(format!("{stem}.{}", image.extension));
    if !path.is_file() {
        std::fs::create_dir_all(dir).map_err(|error| {
            crate::localization::format(
                "connections.credentials.import_create_icon_directory_failed",
                &[
                    ("path", &dir.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
        crate::fs_safe::write_atomic(&path, &image.bytes).map_err(|error| {
            crate::localization::format(
                "connections.credentials.import_save_icon_failed",
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
    }
    Ok(path.to_string_lossy().to_string())
}

fn secure_entities(
    entities: &[PlannedEntity],
    images_dir: &Path,
) -> Result<Vec<SecuredEntity>, String> {
    entities
        .iter()
        .map(|entity| {
            Ok(SecuredEntity {
                name: entity.name.clone(),
                color_hex: entity.color_hex.clone(),
                image_path: entity
                    .image
                    .as_ref()
                    .map(|image| write_image(images_dir, image))
                    .transpose()?,
                safety: entity.safety.clone(),
            })
        })
        .collect()
}

/// Parses, validates, secures, and merges a credential file into `store`.
pub fn import_credential_text(
    text: &str,
    store: &ConnectionStore,
) -> Result<CredentialImportSummary, String> {
    let plan = parse_credential_file(text, crate::engine_form::default_port)
        .map_err(|error| error.to_string())?;

    // Resolve name references against the file first, then saved items, and
    // report every dangling one before touching the keyring.
    let saved_accounts = store.sql_auth_accounts();
    let saved_tunnels = store.tunnel_profiles();
    let mut problems = Vec::new();
    for (index, planned) in plan.connections.iter().enumerate() {
        let at = format!("connections[{index}] \"{}\"", planned.profile.name);
        if let Some(name) = &planned.sql_account
            && !plan
                .sql_auth_accounts
                .iter()
                .any(|a| a.name.eq_ignore_ascii_case(name))
            && !saved_accounts
                .iter()
                .any(|a| a.name.eq_ignore_ascii_case(name))
        {
            problems.push(crate::localization::format(
                "connections.credentials.import_missing_shared_login",
                &[("location", &at), ("account_name", name)],
            ));
        }
        if let Some(name) = &planned.tunnel_profile
            && !plan
                .tunnel_profiles
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(name))
            && !saved_tunnels
                .iter()
                .any(|t| t.name.eq_ignore_ascii_case(name))
        {
            problems.push(crate::localization::format(
                "connections.credentials.import_missing_tunnel_profile",
                &[("location", &at), ("profile_name", name)],
            ));
        }
    }
    if !problems.is_empty() {
        return Err(problems.join("; "));
    }

    let mut keyring = KeyringTxn::default();
    let mut summary = CredentialImportSummary {
        connections: plan.connections.len(),
        sql_auth_accounts: plan.sql_auth_accounts.len(),
        tunnel_profiles: plan.tunnel_profiles.len(),
        entities: plan.clients.len() + plan.projects.len() + plan.environments.len(),
        without_password: 0,
        passwords: 0,
    };

    let secured = (|| -> Result<SecuredCredentialImport, String> {
        let images_dir = store.entity_images_dir();
        let mut secured = SecuredCredentialImport {
            clients: secure_entities(&plan.clients, &images_dir)?,
            projects: secure_entities(&plan.projects, &images_dir)?,
            environments: secure_entities(&plan.environments, &images_dir)?,
            ..SecuredCredentialImport::default()
        };

        // Shared logins: a saved account with the same name and user name is
        // updated in place so connections already using it keep working.
        let mut accounts: HashMap<String, (Uuid, String)> = saved_accounts
            .iter()
            .map(|a| (a.name.to_lowercase(), (a.id, a.username.clone())))
            .collect();
        for account in &plan.sql_auth_accounts {
            let id = saved_accounts
                .iter()
                .find(|saved| {
                    saved.name.eq_ignore_ascii_case(&account.name)
                        && saved.username.eq_ignore_ascii_case(&account.username)
                })
                .map(|saved| saved.id)
                .unwrap_or_else(Uuid::new_v4);
            if let Some(password) = &account.password {
                keyring.store(id, password)?;
            }
            accounts.insert(account.name.to_lowercase(), (id, account.username.clone()));
            secured.sql_auth_accounts.push(SqlAuthAccount {
                id,
                name: account.name.clone(),
                username: account.username.clone(),
            });
        }

        // Tunnel profiles: same-named saved profiles are replaced in place.
        let mut tunnels: HashMap<String, Uuid> = saved_tunnels
            .iter()
            .map(|t| (t.name.to_lowercase(), t.id))
            .collect();
        for tunnel in &plan.tunnel_profiles {
            let id = tunnels
                .get(&tunnel.name.to_lowercase())
                .copied()
                .unwrap_or_else(Uuid::new_v4);
            let mut chain = tunnel.chain.clone();
            secure_chain(&mut chain, &mut keyring)?;
            tunnels.insert(tunnel.name.to_lowercase(), id);
            secured.tunnel_profiles.push(TunnelProfile {
                id,
                name: tunnel.name.clone(),
                chain,
            });
        }

        for planned in &plan.connections {
            let mut profile = planned.profile.clone();
            if let Some(name) = &planned.sql_account {
                let (id, username) = accounts[&name.to_lowercase()].clone();
                profile.authentication = AuthenticationType::SqlLogin {
                    username,
                    secret_ref: keychain_ref(id),
                };
            } else {
                let username = match &profile.authentication {
                    AuthenticationType::SqlLogin { username, .. } => Some(username.clone()),
                    _ => None,
                };
                let is_file = crate::engine_form::is_file_engine(profile.database_type);
                if let Some(secret_ref) = profile.authentication.secret_ref_mut() {
                    match keyring.secure(secret_ref)? {
                        // The connection editor pairs every saved SQL login
                        // password with an account of the same id.
                        Some(id) => {
                            if let Some(username) = username {
                                secured.sql_auth_accounts.push(SqlAuthAccount {
                                    id,
                                    name: profile.name.clone(),
                                    username,
                                });
                            }
                        }
                        None if username.is_some() && !is_file => summary.without_password += 1,
                        None => {}
                    }
                }
            }
            // A Sentinel password goes to the keychain like the login's
            // (the profile keeps only its keychain reference).
            if let Some(secret_ref) = profile
                .redis_sentinel
                .as_mut()
                .and_then(|sentinel| sentinel.sentinel_password_ref.as_mut())
            {
                keyring.secure(secret_ref)?;
            }
            if let Some(name) = &planned.tunnel_profile {
                profile.tunnel_profile = Some(tunnels[&name.to_lowercase()]);
                profile.transport = TransportChain::default();
                profile.ssh_tunnel = None;
            } else {
                secure_chain(&mut profile.transport, &mut keyring)?;
                profile.ssh_tunnel = profile.transport.ssh.clone();
            }
            secured.profiles.push(profile);
        }
        Ok(secured)
    })();

    match secured.and_then(|secured| store.apply_credential_import(secured)) {
        Ok(()) => Ok(CredentialImportSummary {
            passwords: keyring.written.len(),
            ..summary
        }),
        Err(error) => {
            keyring.rollback();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::ConnectionProvenance;

    const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

    fn temp_store() -> (tempfile::TempDir, ConnectionStore) {
        keyring::mock::install();
        let dir = tempfile::tempdir().unwrap();
        let store = ConnectionStore::from_path(dir.path().join("connections.json"));
        (dir, store)
    }

    /// The old Export Connections file (a secret-free bundle), as that
    /// version wrote it.
    fn legacy_bundle_json() -> String {
        let (_dir, source) = temp_store();
        source.add(ConnectionStore::create_sql_login(
            "Old export",
            "legacy.example.com",
            1433,
            Some("app".to_string()),
            "sa",
            "never-exported",
            true,
            false,
        ));
        let json = serde_json::to_string_pretty(&source.export_bundle()).unwrap();
        assert!(!json.contains("never-exported"));
        json
    }

    #[test]
    fn import_connections_reads_the_old_export_bundle() {
        let (_dir, store) = temp_store();
        let message = import_connection_file(&legacy_bundle_json(), Path::new("old.json"), &store)
            .unwrap()
            .expect("something imported");
        assert!(
            message.starts_with("Imported 1 connection from old.json."),
            "{message}"
        );
        assert!(
            message.contains("asks for one on first connect"),
            "{message}"
        );
        assert_eq!(store.list()[0].name, "Old export");
    }

    #[test]
    fn import_connections_reads_files_with_and_without_passwords() {
        let with = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "With", "engine": "postgres", "server": "h", "database": "d",
              "authentication": { "type": "sql-login", "username": "u", "password": "pw-1" } } ] }"#;
        let without = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "Without", "engine": "postgres", "server": "h", "database": "e",
              "authentication": { "type": "sql-login", "username": "u" } } ] }"#;
        let (_dir, store) = temp_store();
        let message = import_connection_file(with, Path::new("w.json"), &store)
            .unwrap()
            .unwrap();
        assert!(
            message.contains("1 password is now in your system keychain"),
            "{message}"
        );
        assert!(message.contains("holds it in clear text"), "{message}");
        let message = import_connection_file(without, Path::new("n.json"), &store)
            .unwrap()
            .unwrap();
        assert!(!message.contains("keychain"), "{message}");
        assert!(message.contains("had no password"), "{message}");
        assert_eq!(store.list().len(), 2);
    }

    #[test]
    fn import_connections_refuses_an_unknown_file_with_secrets() {
        let (_dir, store) = temp_store();
        let error = import_connection_file(
            r#"{ "profiles": [], "password": "x" }"#,
            Path::new("odd.json"),
            &store,
        )
        .unwrap_err();
        assert!(error.contains("not a SQLLY connection file"), "{error}");
        assert!(store.list().is_empty());
    }

    fn keychain_id(secret_ref: &SecretRef) -> Uuid {
        assert_eq!(secret_ref.provider, SecretProvider::MacOsKeychain);
        Uuid::parse_str(&secret_ref.key).unwrap()
    }

    fn password(id: Uuid) -> Option<String> {
        crate::sql_credentials::load_password(id).unwrap()
    }

    #[test]
    fn secrets_move_to_the_keyring_and_icons_to_disk() {
        let (dir, store) = temp_store();
        let file = format!(
            r##"{{ "format": "sqlly-credentials", "version": 1,
  "clients": [{{ "name": "Acme", "color": "#123", "image": "data:image/png;base64,{PNG_B64}" }}],
  "environments": [{{ "name": "Production", "color": "#C62828", "safety": {{ "only_allow_select": true }} }}],
  "sql_auth_accounts": [{{ "name": "Shared DBA", "username": "dba", "password": "dba-pass" }}],
  "tunnel_profiles": [{{ "name": "Bastion", "ssh": {{ "host": "b", "username": "u", "auth": {{ "type": "password", "password": "ssh-pass" }} }} }}],
  "connections": [
    {{ "name": "Prod", "engine": "postgres", "server": "db", "client": "Acme", "environment": "prod",
       "authentication": {{ "type": "sql-login", "username": "app", "password": "app-pass" }},
       "transport": {{ "proxy": {{ "host": "p", "port": 1080, "username": "pu", "password": "proxy-pass" }} }} }},
    {{ "name": "Shared", "engine": "mssql", "server": "sql01",
       "authentication": {{ "type": "sql-login", "account": "Shared DBA" }}, "tunnel_profile": "Bastion" }},
    {{ "name": "NoPass", "engine": "mysql", "server": "m", "authentication": {{ "type": "sql-login", "username": "x" }} }}
  ] }}"##
        );
        let summary = import_credential_text(&file, &store).expect("import");
        assert_eq!(summary.connections, 3);
        assert_eq!(summary.without_password, 1);

        let json = std::fs::read_to_string(dir.path().join("connections.json")).unwrap();
        for secret in ["app-pass", "dba-pass", "ssh-pass", "proxy-pass"] {
            assert!(
                !json.contains(secret),
                "{secret} leaked into connections.json"
            );
        }

        let profiles = store.list();
        let prod = profiles.iter().find(|p| p.name == "Prod").unwrap();
        assert_eq!(prod.provenance, ConnectionProvenance::Imported);
        let AuthenticationType::SqlLogin { secret_ref, .. } = &prod.authentication else {
            panic!("sql login");
        };
        let prod_id = keychain_id(secret_ref);
        assert_eq!(password(prod_id).as_deref(), Some("app-pass"));
        assert!(store.sql_auth_accounts().iter().any(|a| a.id == prod_id));
        let proxy_id = keychain_id(
            prod.transport
                .proxy
                .as_ref()
                .unwrap()
                .password
                .as_ref()
                .unwrap(),
        );
        assert_eq!(password(proxy_id).as_deref(), Some("proxy-pass"));

        let shared = profiles.iter().find(|p| p.name == "Shared").unwrap();
        let AuthenticationType::SqlLogin {
            username,
            secret_ref,
        } = &shared.authentication
        else {
            panic!("sql login");
        };
        assert_eq!(username, "dba");
        assert_eq!(
            password(keychain_id(secret_ref)).as_deref(),
            Some("dba-pass")
        );
        let tunnel = store
            .tunnel_profile(shared.tunnel_profile.unwrap())
            .unwrap();
        let SshAuth::Password { secret_ref } = &tunnel.chain.ssh.as_ref().unwrap().target.auth
        else {
            panic!("ssh password");
        };
        assert_eq!(
            password(keychain_id(secret_ref)).as_deref(),
            Some("ssh-pass")
        );

        let acme = store
            .clients()
            .into_iter()
            .find(|c| c.name == "Acme")
            .unwrap();
        assert_eq!(acme.color_hex.as_deref(), Some("#112233"));
        let icon = acme.image_path.unwrap();
        assert!(icon.ends_with(".png"));
        assert!(Path::new(&icon).starts_with(dir.path()));
        assert!(std::fs::read(&icon).unwrap().starts_with(b"\x89PNG"));

        let production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .unwrap();
        assert_eq!(production.color_hex.as_deref(), Some("#C62828"));
        assert!(production.safety.only_allow_select);
        assert!(
            production.safety.is_production,
            "a partial safety block must not clear the flags it leaves out"
        );
    }

    #[test]
    fn redis_sentinel_password_moves_to_the_keyring_and_cluster_mode_is_kept() {
        let (dir, store) = temp_store();
        let file = r##"{ "format": "sqlly-credentials", "version": 1,
  "connections": [
    { "name": "HA", "engine": "redis", "server": "s1", "port": 26379,
      "authentication": { "type": "sql-login", "username": "default", "password": "data-pass" },
      "redis_sentinel": { "master_name": "mymaster", "sentinels": ["s1:26379"] },
      "redis_sentinel_password": "sentinel-pass" },
    { "name": "Orders", "engine": "redis", "server": "n1", "port": 7000,
      "authentication": { "type": "sql-login", "username": "default" },
      "redis_cluster": { "mode": "cluster" } }
  ] }"##;
        import_credential_text(file, &store).expect("import");
        let json = std::fs::read_to_string(dir.path().join("connections.json")).unwrap();
        assert!(!json.contains("sentinel-pass") && !json.contains("data-pass"));
        let profiles = store.list();
        let ha = profiles.iter().find(|p| p.name == "HA").unwrap();
        let sentinel = ha.redis_sentinel.as_ref().expect("sentinel kept");
        assert_eq!(sentinel.master_name, "mymaster");
        let id = keychain_id(sentinel.sentinel_password_ref.as_ref().unwrap());
        assert_eq!(password(id).as_deref(), Some("sentinel-pass"));
        let orders = profiles.iter().find(|p| p.name == "Orders").unwrap();
        assert_eq!(
            orders.redis_cluster.as_ref().map(|c| c.mode),
            Some(sqlly_core::redis_cluster::RedisClusterMode::Cluster)
        );
    }

    #[test]
    fn entity_without_safety_keeps_existing_protection() {
        let (_dir, store) = temp_store();
        let file = r##"{ "format": "sqlly-credentials", "version": 1,
            "environments": [{ "name": "production", "color": "#000000" }] }"##;
        import_credential_text(file, &store).unwrap();
        let production = store
            .environments()
            .into_iter()
            .find(|e| e.name == "Production")
            .unwrap();
        assert!(production.safety.is_production, "built-in flag survives");
        assert_eq!(production.color_hex.as_deref(), Some("#000000"));
    }

    #[test]
    fn many_servers_databases_and_accounts_in_one_file() {
        let (_dir, store) = temp_store();
        let file = r#"{ "format": "sqlly-credentials", "version": 1,
  "sql_auth_accounts": [
    { "name": "Reader", "username": "ro", "password": "ro-pass" },
    { "name": "Writer", "username": "rw", "password": "rw-pass" }
  ],
  "connections": [
    { "name": "A sales (ro)", "engine": "postgres", "server": "a.example.com", "database": "sales",
      "authentication": { "type": "sql-login", "account": "Reader" } },
    { "name": "A sales (rw)", "engine": "postgres", "server": "a.example.com", "database": "sales",
      "authentication": { "type": "sql-login", "account": "Writer" } },
    { "name": "A hr", "engine": "postgres", "server": "a.example.com", "database": "hr",
      "authentication": { "type": "sql-login", "account": "Reader" } },
    { "name": "B ops", "engine": "sql-server", "server": "b.example.com", "database": "ops",
      "authentication": { "type": "sql-login", "username": "ops", "password": "ops-pass" } }
  ] }"#;
        let summary = import_credential_text(file, &store).unwrap();
        assert_eq!(summary.connections, 4);
        assert_eq!(summary.sql_auth_accounts, 2);

        let profiles = store.list();
        let secret_of = |name: &str| {
            let profile = profiles.iter().find(|p| p.name == name).unwrap();
            let AuthenticationType::SqlLogin { secret_ref, .. } = &profile.authentication else {
                panic!("sql login");
            };
            keychain_id(secret_ref)
        };
        // Connections naming the same account share one keychain entry.
        assert_eq!(secret_of("A sales (ro)"), secret_of("A hr"));
        assert_ne!(secret_of("A sales (ro)"), secret_of("A sales (rw)"));
        assert_eq!(
            password(secret_of("A sales (rw)")).as_deref(),
            Some("rw-pass")
        );
        assert_eq!(password(secret_of("B ops")).as_deref(), Some("ops-pass"));
        // Two file accounts plus the inline login's own account.
        assert_eq!(store.sql_auth_accounts().len(), 3);
    }

    #[test]
    fn dangling_references_import_nothing() {
        let (_dir, store) = temp_store();
        let file = r#"{ "format": "sqlly-credentials", "version": 1, "connections": [
            { "name": "a", "engine": "postgres", "server": "h",
              "authentication": { "type": "sql-login", "username": "u", "password": "pw" } },
            { "name": "b", "engine": "postgres", "server": "h",
              "authentication": { "type": "sql-login", "account": "Missing" }, "tunnel_profile": "Nope" }] }"#;
        let error = import_credential_text(file, &store).unwrap_err();
        assert!(
            error.contains("no shared login named \"Missing\""),
            "{error}"
        );
        assert!(
            error.contains("no tunnel profile named \"Nope\""),
            "{error}"
        );
        assert!(store.list().is_empty());
    }

    #[test]
    fn a_saved_shared_login_is_updated_in_place() {
        let (_dir, store) = temp_store();
        let id = Uuid::new_v4();
        store.upsert_sql_auth_account(SqlAuthAccount {
            id,
            name: "Shared DBA".into(),
            username: "dba".into(),
        });
        crate::sql_credentials::store_password(id, "old").unwrap();
        let file = r#"{ "format": "sqlly-credentials", "version": 1,
            "sql_auth_accounts": [{ "name": "shared dba", "username": "DBA", "password": "new" }],
            "connections": [{ "name": "c", "engine": "sqlserver", "server": "h",
              "authentication": { "type": "sql-login", "account": "Shared DBA" } }] }"#;
        import_credential_text(file, &store).unwrap();
        assert_eq!(password(id).as_deref(), Some("new"));
        assert_eq!(store.sql_auth_accounts().len(), 1);
        let AuthenticationType::SqlLogin { secret_ref, .. } = &store.list()[0].authentication
        else {
            panic!("sql login");
        };
        assert_eq!(keychain_id(secret_ref), id);
    }
}
