//! The OS credential store, behind the keyring 3 API surface SQLly uses.
//!
//! keyring 4 became a thin facade over `keyring-core` plus per-platform store
//! crates, and its `v1` facade installs the platform store on the first
//! `Entry::new` even when a test already installed the mock. This crate keeps
//! `keyring::Entry::new(service, user)` / `set_password` / `get_password` /
//! `delete_credential` and `keyring::Error` (the workspace renames it to
//! `keyring`), installs the platform store lazily only when no store is set,
//! and gives tests [`mock::install`].
//!
//! Credentials stay where keyring 3 put them: the macOS keychain (generic
//! password, service + account), Windows Credential Manager (`user.service`
//! target names) and the Secret Service (`service` + `username` attributes).

use std::sync::LazyLock;

pub use keyring_core::{Error, Result};

/// A named credential in the default store.
pub struct Entry {
    inner: keyring_core::Entry,
}

impl Entry {
    /// An entry for `service` + `user` in the default store, installing the
    /// platform store first if nothing is installed yet.
    pub fn new(service: &str, user: &str) -> Result<Self> {
        // A store installed after a failed platform setup (the test mock on a
        // headless box) takes over; the cached failure only stands while no
        // store is installed.
        if let Err(message) = &*PLATFORM_STORE
            && keyring_core::get_default_store().is_none()
        {
            return Err(Error::PlatformFailure(message.clone().into()));
        }
        Ok(Self {
            inner: keyring_core::Entry::new(service, user)?,
        })
    }

    pub fn set_password(&self, password: &str) -> Result<()> {
        self.inner.set_password(password)
    }

    pub fn get_password(&self) -> Result<String> {
        self.inner.get_password()
    }

    pub fn delete_credential(&self) -> Result<()> {
        self.inner.delete_credential()
    }
}

/// One-time platform store setup. Leaves an already-installed store (the test
/// mock) alone. A failure (for example no Secret Service on a headless Linux
/// box) is kept as text so every later `Entry::new` reports it.
static PLATFORM_STORE: LazyLock<std::result::Result<(), String>> = LazyLock::new(|| {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    install_platform_store().map_err(|error| error.to_string())
});

#[cfg(target_os = "macos")]
fn install_platform_store() -> Result<()> {
    keyring_core::set_default_store(apple_native_keyring_store::keychain::Store::new()?);
    Ok(())
}

#[cfg(target_os = "windows")]
fn install_platform_store() -> Result<()> {
    keyring_core::set_default_store(windows_native_keyring_store::Store::new()?);
    Ok(())
}

#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "android"))
))]
fn install_platform_store() -> Result<()> {
    keyring_core::set_default_store(dbus_secret_service_keyring_store::Store::new()?);
    Ok(())
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "windows",
    all(
        unix,
        not(any(target_os = "macos", target_os = "ios", target_os = "android"))
    )
)))]
fn install_platform_store() -> Result<()> {
    Err(Error::NoDefaultStore)
}

/// In-memory credential store for tests.
pub mod mock {
    use std::sync::Once;

    static INSTALL: Once = Once::new();

    /// Make the process-wide default store an in-memory mock, so tests never
    /// touch the real OS credential store. Idempotent; every caller in a test
    /// binary shares the one mock, so tests should use distinct service/user
    /// names when they assert on stored values.
    pub fn install() {
        INSTALL.call_once(|| match keyring_core::mock::Store::new() {
            Ok(store) => keyring_core::set_default_store(store),
            Err(error) => panic!("keyring mock store: {error}"),
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mock_round_trips_and_reports_missing_entries() {
        super::mock::install();
        let entry = super::Entry::new("sqlly-keyring-test", "round-trip").unwrap();
        assert!(matches!(entry.get_password(), Err(super::Error::NoEntry)));
        entry.set_password("secret").unwrap();
        let again = super::Entry::new("sqlly-keyring-test", "round-trip").unwrap();
        assert_eq!(again.get_password().unwrap(), "secret");
        again.delete_credential().unwrap();
        assert!(matches!(entry.get_password(), Err(super::Error::NoEntry)));
    }
}
