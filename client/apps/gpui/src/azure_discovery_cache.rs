//! On-disk cache of the most recently discovered Azure SQL resources.
//!
//! Azure SQL discovery (`azure_discovery_service::discover_azure_sql_resources`)
//! is a live, auth-gated network round-trip: it cannot run until the Entra
//! sign-in / token flow completes, so on a cold app launch the discovered
//! servers and databases are unavailable for the first few seconds. That means
//! the sidebar tree (and any attempt to restore an active Azure connection)
//! has nothing to show until auth finishes.
//!
//! To fix that, every successful live discovery is persisted here as a JSON
//! snapshot. On the next launch the app loads this cache synchronously and
//! populates the sidebar immediately — no network, no auth wait. Azure is only
//! re-contacted when the user explicitly asks for it (the Explorer "Refresh"
//! action on an Azure node, adding/removing an Entra account, or a new Entra
//! sign-in), at which point the fresh results overwrite this cache.
//!
//! The snapshot is a plain `Vec<AzureSqlDiscovery>` — the same combined,
//! all-accounts list the discovery service returns — so loading it and calling
//! `set_azure_discoveries` reproduces exactly the state a live discovery would
//! have produced. Loads are fault-tolerant (any error → empty), and saves are
//! atomic (`.tmp` write + rename), mirroring `workspace_session.rs`.

use std::path::{Path, PathBuf};

use sqlly_core::AzureSqlDiscovery;

/// Decode a cached discovery snapshot. Fault-tolerant: any decode error yields
/// an empty list so a corrupt cache never blocks startup.
pub fn decode(json: &str) -> Vec<AzureSqlDiscovery> {
    serde_json::from_str::<Vec<AzureSqlDiscovery>>(json).unwrap_or_default()
}

/// Encode a discovery snapshot to pretty JSON.
pub fn encode(discoveries: &[AzureSqlDiscovery]) -> String {
    serde_json::to_string_pretty(discoveries).unwrap_or_else(|_| "[]".to_string())
}

/// Load the cached discoveries from `path` (fault-tolerant → empty).
/// Browser build: reads localStorage instead of the filesystem.
pub fn load(path: &Path) -> Vec<AzureSqlDiscovery> {
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        match crate::host_storage::load_text(
            crate::host_storage::StorageArea::Data,
            "azure-discoveries.json",
        ) {
            Some(content) => decode(&content),
            None => Vec::new(),
        }
    };
    #[cfg(not(target_arch = "wasm32"))]
    match std::fs::read_to_string(path) {
        Ok(content) => decode(&content),
        Err(_) => Vec::new(),
    }
}

/// Save the discoveries to `path` (atomic via `.tmp` rename).
/// Browser build: writes localStorage instead of the filesystem.
pub fn save(discoveries: &[AzureSqlDiscovery], path: &Path) -> std::io::Result<()> {
    let json = encode(discoveries);
    #[cfg(target_arch = "wasm32")]
    return {
        let _ = path;
        crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            "azure-discoveries.json",
            &json,
        )
        .map_err(std::io::Error::other)
    };
    #[cfg(not(target_arch = "wasm32"))]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json.as_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}

/// Default cache file path: `<data_dir>/SQLLY/azure-discoveries.json`.
/// Shares the `SQLLY` data directory with `workspace_session::session_file`.
pub fn cache_file() -> PathBuf {
    #[cfg(not(target_arch = "wasm32"))]
    let mut p = dirs::data_dir().unwrap_or_else(std::env::temp_dir);
    // Browser build: nominal path only — load/save above use localStorage.
    #[cfg(target_arch = "wasm32")]
    let mut p = PathBuf::new();
    p.push("SQLLY");
    p.push("azure-discoveries.json");
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn sample(server_name: &str, database_name: &str) -> AzureSqlDiscovery {
        AzureSqlDiscovery {
            account_id: "acct-1".to_string(),
            username: "alex@contoso.com".to_string(),
            tenant_id: "tenant-a".to_string(),
            subscription_id: "sub-1".to_string(),
            subscription_name: Some("Prod".to_string()),
            resource_group: "rg-1".to_string(),
            server_id: format!("srv-id-{server_name}"),
            server_name: server_name.to_string(),
            server_fqdn: format!("{server_name}.database.windows.net"),
            location: "eastus".to_string(),
            database_id: format!("db-id-{database_name}"),
            database_name: database_name.to_string(),
            service_tier: Some("GeneralPurpose".to_string()),
            status: Some("Online".to_string()),
            linked_profile_id: Some(Uuid::nil()),
        }
    }

    #[test]
    fn round_trip_preserves_discoveries() {
        let discoveries = vec![sample("srv-a", "db-1"), sample("srv-b", "db-2")];
        let json = encode(&discoveries);
        assert_eq!(decode(&json), discoveries);
    }

    #[test]
    fn decode_corrupt_yields_empty() {
        assert!(decode("not json at all").is_empty());
        assert!(decode("{\"unexpected\":true}").is_empty());
        assert!(decode("").is_empty());
    }

    #[test]
    fn load_missing_file_yields_empty() {
        let path = std::env::temp_dir().join("sqlly-azure-cache-does-not-exist-xyz.json");
        let _ = std::fs::remove_file(&path);
        assert!(load(&path).is_empty());
    }

    #[test]
    fn save_then_load_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("sqlly-azure-cache-{}", Uuid::new_v4()));
        let path = dir.join("azure-discoveries.json");
        let discoveries = vec![sample("srv-a", "db-1")];

        save(&discoveries, &path).expect("save should succeed");
        assert_eq!(load(&path), discoveries);

        // Empty snapshot overwrites a prior one.
        save(&[], &path).expect("save empty should succeed");
        assert!(load(&path).is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_file_lives_under_sqlly_dir() {
        let path = cache_file();
        assert!(path.ends_with("SQLLY/azure-discoveries.json"));
    }
}
