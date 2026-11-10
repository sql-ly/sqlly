//! Browser (wasm) variant of [`crate::sql_credentials`]: SQL login passwords
//! live in `sessionStorage` via [`crate::host_storage::secrets`] instead of
//! the OS keyring. They survive reloads within the tab session but are
//! cleared when the tab closes — the desktop app is the one that uses the
//! operating system's secure credential storage.

use sqlly_core::connection::SshTunnelConfig;
use sqlly_core::{ConnectionProfile, SecretProvider};
use uuid::Uuid;

use crate::host_storage::secrets;

const SERVICE: &str = "SQLLY.sql-auth";

fn account_key(id: Uuid) -> String {
    format!("credential.{id}")
}

pub fn store_password(id: Uuid, password: &str) -> Result<(), String> {
    secrets::set(SERVICE, &account_key(id), password)
}

pub fn load_password(id: Uuid) -> Result<Option<String>, String> {
    secrets::get(SERVICE, &account_key(id))
}

pub fn delete_password(id: Uuid) -> Result<(), String> {
    secrets::delete(SERVICE, &account_key(id))
}

/// Copy the secret stored under `from` into a new entry under `to`.
/// `Ok(false)` when `from` has nothing stored.
pub fn copy_password(from: Uuid, to: Uuid) -> Result<bool, String> {
    match load_password(from)? {
        Some(password) => {
            store_password(to, &password)?;
            Ok(true)
        }
        None => Ok(false),
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

/// SSH tunnels are desktop-only; nothing to delete in the browser build.
pub fn delete_ssh_secrets(_tunnel: &SshTunnelConfig) -> Result<(), String> {
    Ok(())
}

/// Stores a legacy cleartext password under a freshly minted SQL account id.
pub fn migrate_legacy_password(password: &str) -> Result<Uuid, String> {
    let id = Uuid::new_v4();
    store_password(id, password)?;
    Ok(id)
}

/// A credential-hydration failure, worded on the UI thread (see the desktop
/// `sql_credentials::CredentialError`).
pub type CredentialError = sqlly_core::DisplayMessage;

/// Resolves a saved SQL account into a runtime-only profile clone. Persisted
/// profiles retain only the account id and never receive the password.
///
/// Safe on any thread; the error is worded on the UI thread with
/// [`crate::localization::message`].
pub fn hydrate_profile(
    mut profile: ConnectionProfile,
) -> Result<ConnectionProfile, CredentialError> {
    let Some(secret_ref) = profile.authentication.secret_ref_mut() else {
        return Ok(profile);
    };
    if secret_ref.provider != SecretProvider::MacOsKeychain {
        return Ok(profile);
    }
    let Ok(account_id) = Uuid::parse_str(&secret_ref.key) else {
        return Ok(profile);
    };
    let password = load_password(account_id)
        .map_err(|detail| {
            crate::localization::deferred(
                "connections.credentials.browser_session_read_failed",
                &[("detail", detail)],
            )
        })?
        .ok_or_else(|| {
            crate::localization::deferred_text(
                "connections.credentials.missing_browser_sql_password",
            )
        })?;
    secret_ref.provider = SecretProvider::InMemory;
    secret_ref.key = password;
    Ok(profile)
}
