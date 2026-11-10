use crate::{SecretProvider, SecretRef, StudioError, StudioResult};

/// Must match the GPUI app's `sql_credentials` service name for the same
/// edition. The Mac App Store edition uses its own name so the two editions
/// never read each other's keychain items (which makes macOS ask for the
/// login password).
#[cfg(not(feature = "mac-app-store"))]
const KEYCHAIN_SERVICE: &str = "SQLLY.sql-auth";
#[cfg(feature = "mac-app-store")]
const KEYCHAIN_SERVICE: &str = "SQLLY.sql-auth.mac-app-store";

/// Resolves a [`SecretRef`] to the secret value. Fail closed: any failure is an
/// authentication error — never an empty string.
pub fn resolve_secret(secret_ref: &SecretRef) -> StudioResult<String> {
    match secret_ref.provider {
        SecretProvider::InMemory => Ok(secret_ref.key.clone()),
        SecretProvider::Environment => std::env::var(&secret_ref.key).map_err(|_| {
            StudioError::Authentication(format!("secret env var '{}' is not set", secret_ref.key))
        }),
        SecretProvider::MacOsKeychain => keychain_password(&secret_ref.key),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn keychain_password(key: &str) -> StudioResult<String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("credential.{key}"))
        .and_then(|entry| entry.get_password())
        .map_err(|error| {
            StudioError::Authentication(format!(
                "keychain secret for account {key} unavailable: {error}"
            ))
        })
}

#[cfg(target_arch = "wasm32")]
fn keychain_password(key: &str) -> StudioResult<String> {
    Err(StudioError::Authentication(format!(
        "keychain secret for account {key} cannot be resolved in the browser; re-enter the password"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn in_memory_passthrough() {
        let secret_ref = SecretRef {
            provider: SecretProvider::InMemory,
            key: "password".into(),
        };
        assert_eq!(resolve_secret(&secret_ref).unwrap(), "password");
    }

    #[test]
    fn environment_reads_env_var() {
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set("SQLLY_CORE_TEST_SECRET", "value");
        let secret_ref = SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_CORE_TEST_SECRET".into(),
        };
        assert_eq!(resolve_secret(&secret_ref).unwrap(), "value");
    }

    #[test]
    fn environment_missing_errors() {
        let secret_ref = SecretRef {
            provider: SecretProvider::Environment,
            key: "SQLLY_CORE_TEST_MISSING".into(),
        };
        let err = resolve_secret(&secret_ref).unwrap_err();
        assert!(
            matches!(err, StudioError::Authentication(message) if message.contains("SQLLY_CORE_TEST_MISSING"))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn macos_keychain_reads_from_keyring() {
        // The in-memory mock keys credentials by service/account, so a fresh
        // Entry in resolve_secret reads what this one wrote, without touching
        // the OS keychain (or needing a Secret Service on headless CI).
        keyring::mock::install();
        let account_id = Uuid::new_v4();
        let account = format!("credential.{account_id}");
        let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &account).expect("entry");
        entry.set_password("hunter2").expect("set password");
        let secret_ref = SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: account_id.to_string(),
        };
        assert_eq!(resolve_secret(&secret_ref).unwrap(), "hunter2");
        entry.delete_credential().ok();
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn macos_keychain_missing_errors_without_secret_value() {
        let account_id = Uuid::new_v4();
        let secret_ref = SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: account_id.to_string(),
        };
        let err = resolve_secret(&secret_ref).unwrap_err();
        assert!(
            matches!(err, StudioError::Authentication(message) if message.contains(&account_id.to_string()) && !message.contains("hunter2"))
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn macos_keychain_does_not_fall_back_to_env() {
        let account_id = Uuid::new_v4();
        let mut env = sqlly_testkit::EnvGuard::acquire();
        env.set(&account_id.to_string(), "from-env");
        let secret_ref = SecretRef {
            provider: SecretProvider::MacOsKeychain,
            key: account_id.to_string(),
        };
        assert!(resolve_secret(&secret_ref).is_err());
    }
}
