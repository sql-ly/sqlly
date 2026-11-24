//! Keys of encrypted notebook workspaces (local data): one OS keychain
//! entry per data scope. A workspace is encrypted exactly when its scope
//! has a key here; the key never leaves the keychain except to open the
//! workspace.

use std::collections::BTreeMap;
use std::sync::Mutex;

use sqlly_core::DisplayMessage;

pub const KEYRING_SERVICE: &str = "SQLLY.notebook-workspace";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeyError {
    pub detail: String,
}

impl std::fmt::Display for WorkspaceKeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the system keychain is unavailable: {}", self.detail)
    }
}

impl std::error::Error for WorkspaceKeyError {}

impl WorkspaceKeyError {
    pub fn message(&self) -> DisplayMessage {
        DisplayMessage::new(
            "notebook.fu_final.encryption.keychain_unavailable",
            format!(
                "The system keychain can't keep this notebook's key: {}",
                self.detail
            ),
        )
        .with_arg("detail", self.detail.clone())
    }
}

/// Where workspace keys are kept.
pub trait WorkspaceKeyStore: Send + Sync {
    fn load(&self, scope: &str) -> Result<Option<String>, WorkspaceKeyError>;
    fn store(&self, scope: &str, key: &str) -> Result<(), WorkspaceKeyError>;
    /// Forget `scope`'s key; a missing one is not an error.
    fn remove(&self, scope: &str) -> Result<(), WorkspaceKeyError>;
}

/// The OS credential store (Keychain, Credential Manager, Secret Service).
pub struct KeyringWorkspaceKeys {
    service: String,
}

impl Default for KeyringWorkspaceKeys {
    fn default() -> Self {
        KeyringWorkspaceKeys {
            service: KEYRING_SERVICE.into(),
        }
    }
}

impl KeyringWorkspaceKeys {
    fn entry(&self, scope: &str) -> Result<keyring::Entry, WorkspaceKeyError> {
        keyring::Entry::new(&self.service, scope).map_err(unavailable)
    }
}

fn unavailable(error: keyring::Error) -> WorkspaceKeyError {
    WorkspaceKeyError {
        detail: error.to_string(),
    }
}

impl WorkspaceKeyStore for KeyringWorkspaceKeys {
    fn load(&self, scope: &str) -> Result<Option<String>, WorkspaceKeyError> {
        match self.entry(scope)?.get_password() {
            Ok(key) => Ok(Some(key)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(unavailable(error)),
        }
    }

    fn store(&self, scope: &str, key: &str) -> Result<(), WorkspaceKeyError> {
        self.entry(scope)?.set_password(key).map_err(unavailable)
    }

    fn remove(&self, scope: &str) -> Result<(), WorkspaceKeyError> {
        match self.entry(scope)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(unavailable(error)),
        }
    }
}

/// Keys held in memory, for tests and hosts without a keychain.
#[derive(Default)]
pub struct MemoryWorkspaceKeys(Mutex<BTreeMap<String, String>>);

impl MemoryWorkspaceKeys {
    pub fn contains(&self, scope: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(scope)
    }
}

impl WorkspaceKeyStore for MemoryWorkspaceKeys {
    fn load(&self, scope: &str) -> Result<Option<String>, WorkspaceKeyError> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(scope)
            .cloned())
    }

    fn store(&self, scope: &str, key: &str) -> Result<(), WorkspaceKeyError> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(scope.to_string(), key.to_string());
        Ok(())
    }

    fn remove(&self, scope: &str) -> Result<(), WorkspaceKeyError> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(scope);
        Ok(())
    }
}

/// A fresh random workspace key (32 random bytes, hex).
pub fn generate_workspace_key() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_kept_per_scope_and_removed() {
        let keys = MemoryWorkspaceKeys::default();
        assert_eq!(keys.load("p_a").unwrap(), None);
        let key = generate_workspace_key();
        assert_eq!(key.len(), 64);
        assert_ne!(key, generate_workspace_key());
        keys.store("p_a", &key).unwrap();
        assert_eq!(keys.load("p_a").unwrap().as_deref(), Some(key.as_str()));
        assert_eq!(keys.load("p_b").unwrap(), None);
        keys.remove("p_a").unwrap();
        keys.remove("p_a").unwrap();
        assert!(!keys.contains("p_a"));
    }

    #[test]
    fn the_keychain_store_round_trips_through_the_credential_store() {
        keyring::mock::install();
        let keys = KeyringWorkspaceKeys {
            service: "SQLLY.notebook-workspace.test".into(),
        };
        assert_eq!(keys.load("scope-round-trip").unwrap(), None);
        keys.store("scope-round-trip", "abc").unwrap();
        assert_eq!(
            keys.load("scope-round-trip").unwrap().as_deref(),
            Some("abc")
        );
        keys.remove("scope-round-trip").unwrap();
        assert_eq!(keys.load("scope-round-trip").unwrap(), None);
        keys.remove("scope-round-trip").unwrap();
    }
}
