//! What parameters keep outside the notebook file: each person's values of
//! personal parameters and personal named sets
//! (`<data dir>/SQLLY/notebooks/personal/<notebook id>.json`, written
//! atomically), and the names of secrets a `secret_ref` parameter can pick
//! (values only in the OS keyring, names in `notebooks/secrets.json`).

#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;

use sqlly_services::notebook::cells::parameter::PersonalParams;

#[cfg(not(target_arch = "wasm32"))]
const SECRET_SERVICE: &str = crate::distribution::keychain_service!("SQLLY.notebook-secret");

#[cfg(not(target_arch = "wasm32"))]
fn safe_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(not(target_arch = "wasm32"))]
/// The personal side file of notebook `id`; ids that are not plain tokens
/// are refused rather than joined into a path.
pub fn personal_path(id: &str) -> Option<PathBuf> {
    safe_id(id).then(|| {
        super::super::autosave::data_root()
            .join("notebooks")
            .join("personal")
            .join(format!("{id}.json"))
    })
}

#[cfg(not(target_arch = "wasm32"))]
pub fn load_personal(id: &str) -> PersonalParams {
    let Some(path) = personal_path(id) else {
        return PersonalParams::new(id);
    };
    match crate::fs_safe::load_json_store::<PersonalParams>(&path) {
        crate::fs_safe::StoreLoad::Loaded(mut personal) => {
            personal.notebook_id = id.to_string();
            personal
        }
        _ => PersonalParams::new(id),
    }
}

#[cfg(target_arch = "wasm32")]
pub fn load_personal(id: &str) -> PersonalParams {
    PersonalParams::new(id)
}

/// Write the side file, or remove it once it holds nothing.
#[cfg(not(target_arch = "wasm32"))]
pub fn save_personal(personal: &PersonalParams) -> std::io::Result<()> {
    let path = personal_path(&personal.notebook_id)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "notebook id"))?;
    if personal.values.is_empty() && personal.sets.is_empty() && personal.extra.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    crate::fs_safe::write_atomic(&path, personal.to_json().as_bytes())
}

#[cfg(target_arch = "wasm32")]
pub fn save_personal(_personal: &PersonalParams) -> std::io::Result<()> {
    Ok(())
}

/// Secrets a `secret_ref` parameter can name. Only names ever leave it.
pub trait SecretCatalog {
    fn names(&self) -> Vec<String>;
    /// Keep `value` under `name`; the value goes to the keyring only.
    fn add(&self, name: &str, value: &str) -> Result<(), String>;
}

/// Names in `notebooks/secrets.json`, values in the OS keyring.
pub struct KeyringSecrets;

#[cfg(not(target_arch = "wasm32"))]
fn secrets_index() -> PathBuf {
    super::super::autosave::data_root()
        .join("notebooks")
        .join("secrets.json")
}

/// Whether `name` can name a secret: a short plain token.
pub fn valid_secret_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl SecretCatalog for KeyringSecrets {
    fn names(&self) -> Vec<String> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            match crate::fs_safe::load_json_store::<Vec<String>>(&secrets_index()) {
                crate::fs_safe::StoreLoad::Loaded(names) => names,
                _ => Vec::new(),
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            Vec::new()
        }
    }

    fn add(&self, name: &str, value: &str) -> Result<(), String> {
        if !valid_secret_name(name) {
            return Err(crate::localization::text(
                "notebook.params.secret.invalid_name",
            ));
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            keyring::Entry::new(SECRET_SERVICE, name)
                .and_then(|entry| entry.set_password(value))
                .map_err(|error| error.to_string())?;
            let mut names = self.names();
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
                names.sort();
                let text = serde_json::to_string_pretty(&names).unwrap_or_default();
                crate::fs_safe::write_atomic(&secrets_index(), text.as_bytes())
                    .map_err(|error| error.to_string())?;
            }
            Ok(())
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = value;
            Err(crate::localization::text(
                "notebook.params.secret.unavailable",
            ))
        }
    }
}
