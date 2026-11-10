use keyring::Entry;
use sqlly_core::connection::{SshAuth, SshHop, SshTunnelConfig};
use sqlly_core::{ConnectionProfile, SecretProvider};
use uuid::Uuid;

const SERVICE: &str = crate::distribution::keychain_service!("SQLLY.sql-auth");

fn account_key(id: Uuid) -> String {
    format!("credential.{id}")
}

fn password_cache() -> &'static parking_lot::Mutex<std::collections::BTreeMap<Uuid, String>> {
    static CACHE: std::sync::OnceLock<
        parking_lot::Mutex<std::collections::BTreeMap<Uuid, String>>,
    > = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

pub fn store_password(id: Uuid, password: &str) -> Result<(), String> {
    Entry::new(SERVICE, &account_key(id))
        .map_err(|error| error.to_string())?
        .set_password(password)
        .map_err(|error| error.to_string())?;
    password_cache().lock().insert(id, password.to_string());
    Ok(())
}

pub fn load_password(id: Uuid) -> Result<Option<String>, String> {
    if let Some(password) = password_cache().lock().get(&id).cloned() {
        return Ok(Some(password));
    }
    match Entry::new(SERVICE, &account_key(id))
        .map_err(|error| error.to_string())?
        .get_password()
    {
        Ok(password) => Ok(Some(password)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Copy the secret stored under `from` into a new entry under `to`, so a
/// duplicated profile owns its own keyring entry. `Ok(false)` when `from`
/// has no stored secret (nothing is written); `Err` when the keyring itself
/// refused to read or write.
pub fn copy_password(from: Uuid, to: Uuid) -> Result<bool, String> {
    match load_password(from)? {
        Some(password) => {
            store_password(to, &password)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

pub fn delete_password(id: Uuid) -> Result<(), String> {
    password_cache().lock().remove(&id);
    match Entry::new(SERVICE, &account_key(id))
        .map_err(|error| error.to_string())?
        .delete_credential()
    {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

pub fn delete_all(ids: &[Uuid]) -> Result<(), String> {
    let failures: Vec<_> = ids
        .iter()
        .filter_map(|id| {
            delete_password(*id)
                .err()
                .map(|error| format!("{id}: {error}"))
        })
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("; "))
    }
}

fn push_keychain_secret_id(secret_ref: &sqlly_core::SecretRef, ids: &mut Vec<Uuid>) {
    if secret_ref.provider == SecretProvider::MacOsKeychain
        && let Ok(id) = Uuid::parse_str(&secret_ref.key)
    {
        ids.push(id);
    }
}

fn collect_ssh_hop_secret_ids(hop: &SshHop, ids: &mut Vec<Uuid>) {
    match &hop.auth {
        SshAuth::Password { secret_ref } => push_keychain_secret_id(secret_ref, ids),
        SshAuth::KeyFile {
            passphrase: Some(secret_ref),
            ..
        } => push_keychain_secret_id(secret_ref, ids),
        _ => {}
    }
}

/// Remove every keyring entry referenced by an SSH tunnel configuration.
pub fn delete_ssh_secrets(tunnel: &SshTunnelConfig) -> Result<(), String> {
    let mut ids = Vec::new();
    collect_ssh_hop_secret_ids(&tunnel.target, &mut ids);
    if let Some(jump) = &tunnel.jump {
        collect_ssh_hop_secret_ids(jump, &mut ids);
    }
    ids.sort_unstable();
    ids.dedup();
    delete_all(&ids)
}

/// A credential-hydration failure. Hydration runs on background executors
/// and the tokio runtime, where the UI language is not visible (translation
/// state is per thread), so it carries a catalog key, arguments, and an
/// English fallback; the UI thread words it with
/// [`crate::localization::message`].
pub type CredentialError = sqlly_core::DisplayMessage;

/// Read a keyring entry for hydration, wording a keyring failure (whose text
/// comes from the OS and stays as-is) inside a complete keyed sentence.
fn load_for_hydration(id: Uuid) -> Result<Option<String>, CredentialError> {
    load_password(id).map_err(|detail| {
        crate::localization::deferred(
            "connections.credentials.keychain_read_failed",
            &[("detail", detail)],
        )
    })
}

fn hydrate_secret_ref(secret_ref: &mut sqlly_core::SecretRef) -> Result<(), CredentialError> {
    if secret_ref.provider != SecretProvider::MacOsKeychain {
        return Ok(());
    }
    let account_id = Uuid::parse_str(&secret_ref.key).map_err(|_| {
        crate::localization::deferred_text("connections.credentials.invalid_ssh_secret_id")
    })?;
    let password = load_for_hydration(account_id)?.ok_or_else(|| {
        crate::localization::deferred_text("connections.credentials.missing_ssh_secret")
    })?;
    secret_ref.provider = SecretProvider::InMemory;
    secret_ref.key = password;
    Ok(())
}

fn hydrate_ssh_auth(auth: &mut SshAuth) -> Result<(), CredentialError> {
    match auth {
        SshAuth::Password { secret_ref } => hydrate_secret_ref(secret_ref)?,
        SshAuth::KeyFile {
            passphrase: Some(secret_ref),
            ..
        } => hydrate_secret_ref(secret_ref)?,
        _ => {}
    }
    Ok(())
}

/// Stores a legacy cleartext password under a freshly minted SQL account id.
pub fn migrate_legacy_password(password: &str) -> Result<Uuid, String> {
    let id = Uuid::new_v4();
    store_password(id, password)?;
    Ok(id)
}

/// Resolves a saved SQL account into a runtime-only profile clone. Persisted
/// profiles retain only the account id and never receive the password.
///
/// Safe on any thread; the error is worded on the UI thread with
/// [`crate::localization::message`].
pub fn hydrate_profile(
    mut profile: ConnectionProfile,
) -> Result<ConnectionProfile, CredentialError> {
    // SQL-login passwords and libSQL auth tokens share the keyring path.
    if let Some(secret_ref) = profile.authentication.secret_ref_mut()
        && secret_ref.provider == SecretProvider::MacOsKeychain
        && let Ok(account_id) = Uuid::parse_str(&secret_ref.key)
    {
        let password = load_for_hydration(account_id)?.ok_or_else(|| {
            crate::localization::deferred_text("connections.credentials.missing_sql_password")
        })?;
        secret_ref.provider = SecretProvider::InMemory;
        secret_ref.key = password;
    }
    // A Redis Sentinel password shares the keyring path too.
    if let Some(secret_ref) = profile
        .redis_sentinel
        .as_mut()
        .and_then(|s| s.sentinel_password_ref.as_mut())
        && secret_ref.provider == SecretProvider::MacOsKeychain
        && let Ok(id) = Uuid::parse_str(&secret_ref.key)
    {
        let password = load_for_hydration(id)?.ok_or_else(|| {
            crate::localization::deferred_text("connections.credentials.missing_sentinel_password")
        })?;
        secret_ref.provider = SecretProvider::InMemory;
        secret_ref.key = password;
    }
    if let Some(tunnel) = profile.ssh_tunnel.as_mut() {
        hydrate_ssh_auth(&mut tunnel.target.auth)?;
        if let Some(jump) = tunnel.jump.as_mut() {
            hydrate_ssh_auth(&mut jump.auth)?;
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlly_core::{AuthenticationType, SecretProvider, SecretRef};

    fn profile_with_secret(secret_ref: SecretRef) -> ConnectionProfile {
        ConnectionProfile::sql_login("prod", "127.0.0.1", "sa", secret_ref)
    }

    /// A libSQL auth token stored in the keyring hydrates like a password.
    #[test]
    fn hydrate_resolves_a_keyring_auth_token() {
        keyring::mock::install();
        let id = Uuid::new_v4();
        store_password(id, "jwt-token").unwrap();
        let profile = ConnectionProfile::auth_token(
            "turso",
            "shop-acme.turso.io",
            443,
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: id.to_string(),
            },
        );
        let hydrated = hydrate_profile(profile).expect("hydrate token");
        let AuthenticationType::AuthToken { secret_ref } = hydrated.authentication else {
            panic!("expected AuthToken");
        };
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert_eq!(secret_ref.key, "jwt-token");
        delete_password(id).unwrap();

        // An anonymous (blank in-memory) token passes through untouched.
        let anonymous = ConnectionProfile::auth_token(
            "local",
            "127.0.0.1",
            8080,
            SecretRef {
                provider: SecretProvider::InMemory,
                key: String::new(),
            },
        );
        let out = hydrate_profile(anonymous).unwrap();
        assert!(matches!(
            out.authentication,
            AuthenticationType::AuthToken { ref secret_ref } if secret_ref.key.is_empty()
        ));
    }

    /// A saved SQL account keeps only its *account id* on disk. Anything that
    /// reaches the engine must hydrate first, or TDS resolves `secret_ref.key`
    /// as an environment variable, fails to find the account id in the
    /// environment, and reports "missing secret for key <uuid>" — which is what
    /// the schema tree did for every lazy load until it learned to call this.
    #[test]
    fn hydrate_turns_an_account_id_into_a_password() {
        keyring::mock::install();
        let account = Uuid::new_v4();
        store_password(account, "hunter2").unwrap();

        let profile = profile_with_secret(SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: account.to_string(),
        });
        let hydrated = hydrate_profile(profile).expect("account resolves");
        let AuthenticationType::SqlLogin { secret_ref, .. } = &hydrated.authentication else {
            panic!("expected a SQL login");
        };
        // The engine reads `InMemory` keys as the password itself.
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert_eq!(secret_ref.key, "hunter2");
        assert_ne!(
            secret_ref.key,
            account.to_string(),
            "the account id must not survive as the secret"
        );
    }

    #[test]
    fn hydrate_reports_a_readable_reason_when_the_keyring_has_nothing() {
        keyring::mock::install();
        let profile = profile_with_secret(SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: Uuid::new_v4().to_string(),
        });
        let error = hydrate_profile(profile).expect_err("no password stored");
        assert!(
            error.contains("keyring"),
            "the user should be told the account has no password, not handed a \
             TDS key they've never seen: {error}"
        );
    }

    #[test]
    fn hydrate_leaves_other_auth_kinds_untouched() {
        keyring::mock::install();
        // An already-resolved password passes through unchanged...
        let inline = profile_with_secret(SecretRef {
            provider: SecretProvider::InMemory,
            key: "already-a-password".to_string(),
        });
        let out = hydrate_profile(inline).expect("passthrough");
        let AuthenticationType::SqlLogin { secret_ref, .. } = &out.authentication else {
            panic!("expected a SQL login");
        };
        assert_eq!(secret_ref.key, "already-a-password");

        // ...and Entra carries a runtime token instead of a secret ref, so it
        // must not be treated as a missing account.
        let mut entra = profile_with_secret(SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        });
        entra.authentication = AuthenticationType::EntraBrowser;
        assert!(hydrate_profile(entra).is_ok());

        // Cloud IAM variants also pass through without a keyring lookup.
        let mut aws = profile_with_secret(SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        });
        aws.authentication = AuthenticationType::AwsRdsIam {
            username: "jane".into(),
            region: "us-west-2".into(),
            profile: None,
        };
        let aws_out = hydrate_profile(aws).expect("aws iam passthrough");
        assert!(matches!(
            aws_out.authentication,
            AuthenticationType::AwsRdsIam { .. }
        ));

        let mut gcp = profile_with_secret(SecretRef {
            provider: SecretProvider::InMemory,
            key: String::new(),
        });
        gcp.authentication = AuthenticationType::GcpCloudSqlIam {
            username: "jane@example.com".into(),
        };
        let gcp_out = hydrate_profile(gcp).expect("gcp iam passthrough");
        assert!(matches!(
            gcp_out.authentication,
            AuthenticationType::GcpCloudSqlIam { .. }
        ));
    }

    #[test]
    fn password_round_trip_and_delete() {
        keyring::mock::install();
        let id = Uuid::new_v4();

        store_password(id, "secret").unwrap();
        assert_eq!(load_password(id).unwrap().as_deref(), Some("secret"));
        delete_password(id).unwrap();
        assert_eq!(load_password(id).unwrap(), None);
    }

    #[test]
    fn delete_all_removes_every_known_password() {
        keyring::mock::install();
        let ids = [Uuid::new_v4(), Uuid::new_v4()];
        for id in ids {
            store_password(id, "secret").unwrap();
        }

        delete_all(&ids).unwrap();
        for id in ids {
            assert_eq!(load_password(id).unwrap(), None);
        }
    }

    #[test]
    fn hydrate_profile_resolves_keyring_reference_without_mutating_source() {
        keyring::mock::install();
        let id = Uuid::new_v4();
        store_password(id, "secret").unwrap();
        let source = ConnectionProfile::sql_login(
            "saved",
            "localhost",
            "sa",
            SecretRef {
                provider: SecretProvider::MacOsKeychain,
                key: id.to_string(),
            },
        );

        let hydrated = hydrate_profile(source.clone()).unwrap();
        let AuthenticationType::SqlLogin { secret_ref, .. } = hydrated.authentication else {
            panic!("expected SQL login");
        };
        assert_eq!(secret_ref.provider, SecretProvider::InMemory);
        assert_eq!(secret_ref.key, "secret");
        assert!(matches!(
            source.authentication,
            AuthenticationType::SqlLogin {
                secret_ref: SecretRef {
                    provider: SecretProvider::MacOsKeychain,
                    ..
                },
                ..
            }
        ));
    }
}
