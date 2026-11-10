//! Host-storage seam: the one place the app touches durable storage and
//! secrets, with per-platform backends behind an identical API.
//!
//! - **Native**: text artifacts are real files under the OS config/data dirs
//!   (exactly the paths the app has always used — callers pass the same
//!   relative file names); secrets go to the OS credential store via
//!   `keyring` (Keychain Services / Credential Manager / Secret Service).
//! - **Browser (wasm)**: text artifacts live in `localStorage` (they survive
//!   reloads but stay in this browser profile); secrets live in
//!   `sessionStorage`, which the browser clears when the tab closes. The
//!   credentials UI states this difference explicitly — the desktop app is
//!   the one that uses your operating system's secure credential storage.

#![allow(dead_code)]

/// Which root a text artifact belongs to. Mirrors the app's existing split
/// between `dirs::config_dir()/SQLLY` and `dirs::data_dir()/SQLLY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageArea {
    Config,
    Data,
}

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::StorageArea;
    use std::path::PathBuf;

    fn root(area: StorageArea) -> Option<PathBuf> {
        // Tests can redirect the roots to a temp dir so they don't read, write,
        // or evict against the developer's / CI agent's real SQLLY data — see
        // `super::test_support`.
        #[cfg(test)]
        if let Some(root) = super::test_support::root_override() {
            return Some(root.join(match area {
                StorageArea::Config => "config",
                StorageArea::Data => "data",
            }));
        }
        let base = match area {
            StorageArea::Config => dirs::config_dir()?,
            StorageArea::Data => dirs::data_dir()?,
        };
        Some(base.join("SQLLY"))
    }

    /// Absolute path of a named artifact (`name` may contain subdirectories,
    /// e.g. `"history/queries.json"`).
    pub fn artifact_path(area: StorageArea, name: &str) -> Option<PathBuf> {
        Some(root(area)?.join(name))
    }

    pub fn load_text(area: StorageArea, name: &str) -> Option<String> {
        std::fs::read_to_string(artifact_path(area, name)?).ok()
    }

    pub fn store_text(area: StorageArea, name: &str, contents: &str) -> Result<(), String> {
        let path = artifact_path(area, name).ok_or("no storage root")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, contents).map_err(|e| e.to_string())
    }

    pub fn remove(area: StorageArea, name: &str) {
        if let Some(path) = artifact_path(area, name) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::StorageArea;

    fn key(area: StorageArea, name: &str) -> String {
        let area = match area {
            StorageArea::Config => "config",
            StorageArea::Data => "data",
        };
        format!("sqlly.{area}.{name}")
    }

    fn local_storage() -> Option<web_sys::Storage> {
        web_sys::window()?.local_storage().ok().flatten()
    }

    pub fn load_text(area: StorageArea, name: &str) -> Option<String> {
        local_storage()?.get_item(&key(area, name)).ok().flatten()
    }

    pub fn store_text(area: StorageArea, name: &str, contents: &str) -> Result<(), String> {
        local_storage()
            .ok_or("localStorage unavailable")?
            .set_item(&key(area, name), contents)
            .map_err(|_| "localStorage write failed (quota?)".to_string())
    }

    pub fn remove(area: StorageArea, name: &str) {
        if let Some(storage) = local_storage() {
            let _ = storage.remove_item(&key(area, name));
        }
    }
}

/// Test-only redirection of the native storage roots. `ai_conversations` and
/// other host-storage callers otherwise read/write/evict against the real
/// `dirs::data_dir()/SQLLY`, so parallel tests race and a full real index can
/// evict a test's own freshly-saved conversation. A test holds the returned
/// guard for its body to point the roots at a temp dir instead.
#[cfg(all(test, not(target_arch = "wasm32")))]
pub mod test_support {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::sync::{Mutex, MutexGuard};

    static ROOT_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);
    thread_local! {
        static ROOT_OVERRIDE_LOCAL: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }
    static SERIAL: Mutex<()> = Mutex::new(());

    pub(super) fn root_override() -> Option<PathBuf> {
        ROOT_OVERRIDE_LOCAL
            .with(|slot| slot.borrow().clone())
            .or_else(|| ROOT_OVERRIDE.lock().ok().and_then(|guard| guard.clone()))
    }

    /// Redirects the native storage roots to `root` for the current test,
    /// serialized against other storage-redirecting tests. Resets on drop.
    pub struct StorageRootGuard {
        _serial: MutexGuard<'static, ()>,
    }

    pub fn redirect_storage(root: PathBuf) -> StorageRootGuard {
        let serial = SERIAL.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Ok(mut guard) = ROOT_OVERRIDE.lock() {
            *guard = Some(root.clone());
        }
        ROOT_OVERRIDE_LOCAL.with(|slot| *slot.borrow_mut() = Some(root));
        StorageRootGuard { _serial: serial }
    }

    impl Drop for StorageRootGuard {
        fn drop(&mut self) {
            if let Ok(mut guard) = ROOT_OVERRIDE.lock() {
                *guard = None;
            }
            ROOT_OVERRIDE_LOCAL.with(|slot| *slot.borrow_mut() = None);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[allow(unused_imports)]
// consumed by the wasm build's storage seams; kept symmetric on native
pub use native::{artifact_path, load_text, remove, store_text};
#[cfg(target_arch = "wasm32")]
#[allow(unused_imports)]
pub use wasm::{load_text, remove, store_text};

/// Secrets: OS credential store on native, sessionStorage in the browser.
pub mod secrets {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        entry.set_password(value).map_err(|e| e.to_string())
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn session_storage() -> Result<web_sys::Storage, String> {
        web_sys::window()
            .and_then(|w| w.session_storage().ok().flatten())
            .ok_or_else(|| "sessionStorage unavailable".to_string())
    }

    #[cfg(target_arch = "wasm32")]
    fn key(service: &str, account: &str) -> String {
        format!("sqlly.secret.{service}.{account}")
    }

    #[cfg(target_arch = "wasm32")]
    pub fn get(service: &str, account: &str) -> Result<Option<String>, String> {
        Ok(session_storage()?
            .get_item(&key(service, account))
            .ok()
            .flatten())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn set(service: &str, account: &str, value: &str) -> Result<(), String> {
        session_storage()?
            .set_item(&key(service, account), value)
            .map_err(|_| "sessionStorage write failed".to_string())
    }

    #[cfg(target_arch = "wasm32")]
    pub fn delete(service: &str, account: &str) -> Result<(), String> {
        session_storage()?
            .remove_item(&key(service, account))
            .map_err(|_| "sessionStorage remove failed".to_string())
    }
}
